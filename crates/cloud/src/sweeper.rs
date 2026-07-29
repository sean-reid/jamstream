//! Orphan sweeper: nothing tagged jamstream may keep billing. Runs across
//! every configured provider on every app and CLI launch.

use crate::provider::{Provider, ProviderError};
use crate::types::{Instance, ProviderKind};

#[derive(Debug, Clone, PartialEq, Default)]
pub enum SweepFilter {
    /// Every jamstream-tagged instance.
    #[default]
    All,
    /// Only instances belonging to this session.
    Session(String),
    /// Everything except this session, e.g. sweep around a live jam.
    Excluding(String),
}

impl SweepFilter {
    fn matches(&self, instance: &Instance) -> bool {
        match self {
            SweepFilter::All => true,
            SweepFilter::Session(s) => instance.session_id() == Some(s.as_str()),
            SweepFilter::Excluding(s) => instance.session_id() != Some(s.as_str()),
        }
    }
}

/// What one sweep did, and what it could not do.
///
/// `#[must_use]` for the same reason as [`crate::RetentionEnforcement`]:
/// [`sweep`] destroys machines, so a caller can plausibly run it for the effect
/// and drop the answer, and the answer is the only place `unswept` and `failed`
/// exist. A sweep whose report is dropped is a host who was never told that an
/// account still has a machine in it, which is the one thing this file
/// promises.
#[must_use]
#[derive(Debug, Default)]
pub struct SweepReport {
    pub found: Vec<Instance>,
    pub destroyed: Vec<Instance>,
    pub failed: Vec<(Instance, ProviderError)>,
    /// Providers that could not be listed at all. Nothing can be attached
    /// to an instance here, and that is exactly why it is reported: a
    /// provider whose listing fails is a provider whose strays were never
    /// looked for, and a sweep that says nothing about it reads as clean.
    pub unswept: Vec<(ProviderKind, ProviderError)>,
    /// Per-session firewalls with no instance left behind them, deleted on
    /// the way past. AWS will not delete a security group until the
    /// terminating instance's network interface is gone, so a group that was
    /// still attached during one sweep is collected by the next.
    pub firewalls_removed: Vec<String>,
}

impl SweepReport {
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty() && self.unswept.is_empty()
    }
}

pub async fn sweep(
    providers: &[Box<dyn Provider>],
    filter: SweepFilter,
    dry_run: bool,
) -> SweepReport {
    let mut report = SweepReport::default();
    for p in providers {
        let instances = match p.list_tagged(None).await {
            Ok(v) => v,
            Err(e) => {
                // Carry on with the others, but say so: the promise at the
                // top of this file is that nothing tagged jamstream keeps
                // billing, and a provider that could not be listed is one
                // this sweep cannot make that promise about.
                tracing::warn!(provider = p.kind().as_str(), error = %e, "sweep list failed");
                report.unswept.push((p.kind(), e));
                continue;
            }
        };
        for inst in instances.into_iter().filter(|i| filter.matches(i)) {
            report.found.push(inst.clone());
            if dry_run {
                continue;
            }
            match p.destroy(&inst.region.id, &inst.id).await {
                Ok(()) => report.destroyed.push(inst),
                Err(e) => {
                    tracing::warn!(
                        provider = p.kind().as_str(),
                        instance = inst.id,
                        error = %e,
                        "sweep destroy failed"
                    );
                    report.failed.push((inst, e));
                }
            }
        }
        if dry_run {
            continue;
        }
        // Firewalls cost nothing and cannot be swept by instance id, so they
        // are collected here rather than reported as strays. A live session's
        // firewall is never touched, including the one a filtered sweep is
        // deliberately sparing.
        match p.destroy_orphan_firewalls().await {
            Ok(names) => report.firewalls_removed.extend(names),
            Err(e) => {
                tracing::warn!(provider = p.kind().as_str(), error = %e, "firewall cleanup failed");
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockProvider;
    use crate::types::{InstanceClass, LaunchSpec, session_tag};

    fn seeded(kind: ProviderKind, sessions: &[&str]) -> MockProvider {
        let p = MockProvider::with_default_regions(kind);
        let region = p.regions()[0].clone();
        for s in sessions {
            p.seed_instance(&region, vec![session_tag(s)]);
        }
        p
    }

    #[tokio::test]
    async fn sweeps_all_tagged_instances_across_providers() {
        let aws = seeded(ProviderKind::Aws, &["s1", "s2"]);
        let gcp = seeded(ProviderKind::Gcp, &["s3"]);
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(aws), Box::new(gcp)];
        let report = sweep(&providers, SweepFilter::All, false).await;
        assert_eq!(report.found.len(), 3);
        assert_eq!(report.destroyed.len(), 3);
        assert!(report.is_clean());
        for p in &providers {
            assert!(p.list_tagged(None).await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn dry_run_destroys_nothing() {
        let p = seeded(ProviderKind::DigitalOcean, &["s1"]);
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(p)];
        let report = sweep(&providers, SweepFilter::All, true).await;
        assert_eq!(report.found.len(), 1);
        assert!(report.destroyed.is_empty());
        assert!(report.firewalls_removed.is_empty());
        assert_eq!(providers[0].list_tagged(None).await.unwrap().len(), 1);
    }

    /// A stray instance leaves a stray firewall, and a sweep that took down
    /// the firewall of the session it is deliberately sparing would end that
    /// session.
    #[tokio::test]
    async fn sweep_collects_stray_firewalls_and_spares_the_live_one() {
        let p = MockProvider::with_default_regions(ProviderKind::Aws);
        let region = p.regions()[0].clone();
        for session in ["live", "leaked"] {
            p.launch(LaunchSpec {
                region: region.clone(),
                instance_class: InstanceClass::Small,
                user_data: String::new(),
                tags: vec![session_tag(session)],
            })
            .await
            .unwrap();
        }
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(p)];
        let report = sweep(&providers, SweepFilter::Excluding("live".into()), false).await;
        assert_eq!(report.destroyed.len(), 1);
        assert_eq!(report.firewalls_removed, vec!["leaked".to_owned()]);
        assert!(
            !providers[0]
                .session_ingress("live")
                .await
                .unwrap()
                .is_empty(),
            "the live session must still be reachable after a sweep"
        );
        assert!(
            providers[0]
                .session_ingress("leaked")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn excluding_filter_spares_the_live_session() {
        let p = seeded(ProviderKind::Aws, &["live", "leaked"]);
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(p)];
        let report = sweep(&providers, SweepFilter::Excluding("live".into()), false).await;
        assert_eq!(report.destroyed.len(), 1);
        assert_eq!(report.destroyed[0].session_id(), Some("leaked"));
        let left = providers[0].list_tagged(None).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].session_id(), Some("live"));
    }

    /// A provider whose listing fails was never searched, so a report that
    /// called that clean would tell a host their account is empty when
    /// nobody looked. The other providers are still swept.
    #[tokio::test]
    async fn a_provider_that_cannot_be_listed_is_reported_not_skipped() {
        let broken = seeded(ProviderKind::Local, &["s1"]);
        broken.fail_next_lists(1, ProviderError::Other("registry is corrupt".to_owned()));
        let working = seeded(ProviderKind::Aws, &["s2"]);
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(broken), Box::new(working)];

        let report = sweep(&providers, SweepFilter::All, false).await;
        assert_eq!(report.destroyed.len(), 1, "the working provider is swept");
        assert_eq!(report.destroyed[0].session_id(), Some("s2"));
        assert_eq!(report.unswept.len(), 1);
        assert_eq!(report.unswept[0].0, ProviderKind::Local);
        assert!(
            !report.is_clean(),
            "a sweep that missed a provider is not clean"
        );
    }

    #[tokio::test]
    async fn destroy_failure_lands_in_failed() {
        let p = seeded(ProviderKind::Gcp, &["s1", "s2"]);
        p.fail_next_destroys(1, ProviderError::RateLimited { retry_after: None });
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(p)];
        let report = sweep(&providers, SweepFilter::All, false).await;
        assert_eq!(report.found.len(), 2);
        assert_eq!(report.destroyed.len(), 1);
        assert_eq!(report.failed.len(), 1);
        assert!(!report.is_clean());
        assert!(matches!(
            report.failed[0].1,
            ProviderError::RateLimited { .. }
        ));
    }
}
