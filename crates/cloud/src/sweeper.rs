//! Orphan sweeper: nothing tagged jamstream may keep billing. Runs across
//! every configured provider on every app and CLI launch.

use crate::provider::{Provider, ProviderError, Sleeper, WaitOpts};
use crate::types::{Instance, ProviderKind, RegionId};

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
    /// Providers that could not be listed at all, by the name a session
    /// record spells. Nothing can be attached to an instance here, and that
    /// is exactly why it is reported: a provider whose listing fails is a
    /// provider whose strays were never looked for, and a sweep that says
    /// nothing about it reads as clean.
    pub unswept: Vec<(&'static str, ProviderError)>,
    /// One entry per provider that answered, saying how much of it the
    /// search reached. Nothing else in this report can tell an instance
    /// that is gone from one nobody looked for.
    pub searches: Vec<ProviderSearch>,
    /// Per-session firewalls with no instance left behind them, deleted on
    /// the way past. AWS will not delete a security group until the
    /// terminating instance's network interface is gone, so a group that was
    /// still attached during one sweep is collected by the next.
    pub firewalls_removed: Vec<String>,
}

/// How long a teardown keeps asking for the firewall. AWS detaches a
/// terminated instance's network interface in tens of seconds; past this a
/// sweep is a better place for it than a teardown that will not return.
pub const FIREWALL_WAIT: WaitOpts = WaitOpts {
    initial_backoff: std::time::Duration::from_secs(1),
    max_backoff: std::time::Duration::from_secs(8),
    total_timeout: std::time::Duration::from_secs(90),
};

/// Deletes the firewall a finished session leaves behind, retrying while the
/// provider still refuses.
///
/// A teardown asks for this straight after `destroy`, and on AWS that is too
/// early: a security group cannot be deleted until the terminated instance's
/// network interface has detached, which takes longer than the destroy call
/// does. One attempt therefore leaves a firewall per session in the account
/// until some later sweep collects it. This keeps asking until the session has
/// no ingress left or the budget is spent, and reports what it removed.
///
/// `session_ingress` is the condition rather than a non-empty return, because a
/// session that never opened one has nothing to delete and must not spend the
/// whole budget discovering that.
pub async fn close_session_firewall(
    provider: &dyn Provider,
    session: &str,
    sleeper: &dyn Sleeper,
    opts: WaitOpts,
) -> Result<Vec<String>, ProviderError> {
    let mut backoff = opts.initial_backoff;
    let mut elapsed = std::time::Duration::ZERO;
    let mut removed = Vec::new();
    let mut refusal = None;
    loop {
        match provider.destroy_orphan_firewalls().await {
            Ok(names) => removed.extend(names),
            Err(err) => refusal = Some(err),
        }
        match provider.session_ingress(session).await {
            Ok(open) if open.is_empty() => return Ok(removed),
            Ok(_) => {}
            Err(err) => refusal = Some(err),
        }
        if elapsed + backoff > opts.total_timeout {
            return Err(refusal.unwrap_or_else(|| {
                ProviderError::Transient(format!(
                    "session {session} still has ingress open after {:?}",
                    opts.total_timeout
                ))
            }));
        }
        sleeper.sleep(backoff).await;
        elapsed += backoff;
        backoff = (backoff * 2).min(opts.max_backoff);
    }
}

/// How much of one provider a sweep managed to search. A provider whose
/// listing failed outright is in [`SweepReport::unswept`] instead and has no
/// entry here.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderSearch {
    /// The name a session record spells for this provider, which is what
    /// ties a record to the search that can speak for it. Names rather than
    /// kinds, because the mock's instances borrow a real kind.
    pub provider: &'static str,
    pub kind: ProviderKind,
    /// Regions that could not be listed while others answered. Empty when
    /// the search covered the whole provider, which is the only case where
    /// an instance missing from [`SweepReport::found`] is one that is gone.
    pub unsearched: Vec<RegionId>,
}

impl ProviderSearch {
    pub fn is_complete(&self) -> bool {
        self.unsearched.is_empty()
    }
}

impl SweepReport {
    /// Whether this sweep can promise nothing tagged jamstream is still
    /// billing. A skipped region is as much a hole in that promise as a
    /// destroy that failed, so it is not clean either.
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty()
            && self.unswept.is_empty()
            && self.searches.iter().all(ProviderSearch::is_complete)
    }

    /// The search that can speak for a session record naming `provider`, if
    /// this sweep held such a provider at all.
    pub fn search_for(&self, provider: &str) -> Option<&ProviderSearch> {
        self.searches.iter().find(|s| s.provider == provider)
    }
}

pub async fn sweep(
    providers: &[Box<dyn Provider>],
    filter: SweepFilter,
    dry_run: bool,
) -> SweepReport {
    let mut report = SweepReport::default();
    for p in providers {
        let listing = match p.list_tagged(None).await {
            Ok(v) => v,
            Err(e) => {
                // Carry on with the others, but say so: the promise at the
                // top of this file is that nothing tagged jamstream keeps
                // billing, and a provider that could not be listed is one
                // this sweep cannot make that promise about.
                tracing::warn!(provider = p.name(), error = %e, "sweep list failed");
                report.unswept.push((p.name(), e));
                continue;
            }
        };
        if !listing.is_complete() {
            tracing::warn!(
                provider = p.name(),
                regions = listing.unsearched_display(),
                "sweep searched only part of a provider"
            );
        }
        report.searches.push(ProviderSearch {
            provider: p.name(),
            kind: p.kind(),
            unsearched: listing.unsearched,
        });
        for inst in listing.instances.into_iter().filter(|i| filter.matches(i)) {
            report.found.push(inst.clone());
            if dry_run {
                continue;
            }
            match p.destroy(&inst.region.id, &inst.id).await {
                Ok(()) => report.destroyed.push(inst),
                Err(e) => {
                    tracing::warn!(
                        provider = p.name(),
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
                tracing::warn!(provider = p.name(), error = %e, "firewall cleanup failed");
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::mock::{MockProvider, RecordingSleeper};
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
            assert!(p.list_tagged(None).await.unwrap().instances.is_empty());
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
        assert_eq!(
            providers[0]
                .list_tagged(None)
                .await
                .unwrap()
                .instances
                .len(),
            1
        );
    }

    /// One session, launched then destroyed, with the firewall left behind the
    /// way AWS leaves it while the network interface detaches.
    async fn ended_session(refusals: usize) -> MockProvider {
        let p = MockProvider::with_default_regions(ProviderKind::Aws);
        let region = p.regions()[0].clone();
        let instance = p
            .launch(LaunchSpec {
                region: region.clone(),
                instance_class: InstanceClass::Small,
                user_data: String::new(),
                tags: vec![session_tag("ended")],
            })
            .await
            .unwrap();
        p.destroy(&instance.region.id, &instance.id).await.unwrap();
        p.fail_next_firewall_sweeps(
            refusals,
            ProviderError::Transient("resource sg-1 has a dependent object".to_owned()),
        );
        p
    }

    /// The whole point: a teardown asks too early, is refused, and the firewall
    /// is still gone by the time the call returns rather than being left for a
    /// later sweep.
    #[tokio::test]
    async fn a_firewall_refused_at_teardown_is_closed_before_the_call_returns() {
        let p = ended_session(3).await;
        let sleeper = RecordingSleeper::default();
        let closed = close_session_firewall(&p, "ended", &sleeper, WaitOpts::default())
            .await
            .expect("the firewall closes once the interface detaches");
        assert_eq!(closed, vec!["ended".to_owned()]);
        assert!(
            p.session_ingress("ended").await.unwrap().is_empty(),
            "the session still has ingress open"
        );
        // Refused three times, so it waited three times and backed off.
        let waits = sleeper.slept();
        assert_eq!(waits.len(), 3, "{waits:?}");
        assert!(waits[1] > waits[0], "the backoff must grow: {waits:?}");
    }

    /// A session that never opened one must not spend the budget finding out.
    #[tokio::test]
    async fn a_session_with_no_firewall_returns_without_waiting() {
        let p = MockProvider::with_default_regions(ProviderKind::Aws);
        let sleeper = RecordingSleeper::default();
        let closed = close_session_firewall(&p, "never-launched", &sleeper, WaitOpts::default())
            .await
            .expect("nothing to close is not a failure");
        assert!(closed.is_empty(), "{closed:?}");
        assert!(
            sleeper.slept().is_empty(),
            "it waited for a firewall that never existed"
        );
    }

    /// When the provider refuses for longer than the budget, the caller has to
    /// hear about it: that is the case a sweep still has to collect.
    #[tokio::test]
    async fn a_firewall_that_never_detaches_is_reported_not_swallowed() {
        // More refusals than the budget below allows attempts, without asking
        // the mock to queue an unbounded number of them.
        let p = ended_session(50).await;
        let sleeper = RecordingSleeper::default();
        let err = close_session_firewall(
            &p,
            "ended",
            &sleeper,
            WaitOpts {
                initial_backoff: Duration::from_millis(500),
                max_backoff: Duration::from_secs(8),
                total_timeout: Duration::from_secs(30),
            },
        )
        .await
        .expect_err("a firewall that never detaches is not a success");
        assert!(
            err.to_string().contains("dependent object"),
            "the provider's own refusal is what the caller reports: {err}"
        );
        assert!(
            !p.session_ingress("ended").await.unwrap().is_empty(),
            "the fixture must still have the firewall open"
        );
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
        assert_eq!(left.instances.len(), 1);
        assert_eq!(left.instances[0].session_id(), Some("live"));
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
        assert_eq!(report.unswept[0].0, MockProvider::NAME);
        assert!(
            !report.is_clean(),
            "a sweep that missed a provider is not clean"
        );
        // A provider that answered nothing has no search to its name, so
        // nothing downstream can mistake it for one that came back empty.
        assert_eq!(report.searches.len(), 1);
    }

    /// A provider that reached some of its regions is not a provider that
    /// searched itself. AWS and GCP return the regions that answered, so a
    /// throttled one otherwise reads as an account with nothing in it.
    #[tokio::test]
    async fn a_partly_searched_provider_says_which_regions_it_missed() {
        let p = MockProvider::with_default_regions(ProviderKind::Aws);
        let east = p.regions()[0].clone();
        let west = p.regions()[1].clone();
        p.seed_instance(&east, vec![session_tag("reachable")]);
        p.seed_instance(&west, vec![session_tag("hidden")]);
        p.unsearchable_region(&west.id);
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(p)];

        let report = sweep(&providers, SweepFilter::All, false).await;
        assert_eq!(report.destroyed.len(), 1, "only the reachable one is found");
        assert_eq!(report.destroyed[0].session_id(), Some("reachable"));
        let search = report.search_for(MockProvider::NAME).expect("searched");
        assert_eq!(search.unsearched, vec![west.id]);
        assert!(!search.is_complete());
        assert!(
            !report.is_clean(),
            "a region nobody could list is a hole in the promise that nothing is billing"
        );
    }

    /// The double answers to its own name. Reporting the kind its instances
    /// borrow would let a sweep holding only the mock claim it searched AWS.
    #[tokio::test]
    async fn the_mock_is_searched_under_its_own_name() {
        let p = seeded(ProviderKind::Aws, &["s1"]);
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(p)];
        let report = sweep(&providers, SweepFilter::All, false).await;
        assert_eq!(report.searches.len(), 1);
        assert_eq!(report.searches[0].provider, MockProvider::NAME);
        assert_eq!(report.searches[0].kind, ProviderKind::Aws);
        assert!(report.search_for("aws").is_none());
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
