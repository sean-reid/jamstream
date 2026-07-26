//! Orphan sweeper: nothing tagged jamstream may keep billing. Runs across
//! every configured provider on every app and CLI launch.

use crate::provider::{Provider, ProviderError};
use crate::types::Instance;

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

#[derive(Debug, Default)]
pub struct SweepReport {
    pub found: Vec<Instance>,
    pub destroyed: Vec<Instance>,
    pub failed: Vec<(Instance, ProviderError)>,
}

impl SweepReport {
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty()
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
                // Nothing to attach a failure to; surface it and move on so
                // one broken provider never blocks sweeping the others.
                tracing::warn!(provider = p.kind().as_str(), error = %e, "sweep list failed");
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
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockProvider;
    use crate::types::{ProviderKind, session_tag};

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
        assert_eq!(providers[0].list_tagged(None).await.unwrap().len(), 1);
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
