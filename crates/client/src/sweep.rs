//! The sweep, in the app: find and destroy every jamstream-tagged machine,
//! then bring the session records back in line with what is really running.
//!
//! `jamstream sweep` is what saves a host when a launch half-failed or a
//! record is wrong, and it lived only in the CLI. The DMG and the Scoop app
//! package ship no CLI, so the person most likely to strand a billing machine
//! was the one who could not run the command that finds it (#371).
//!
//! The engine is [`jamstream_cloud::sweep`] and the record reconciliation is
//! [`jamstream_cli::sweep::reconcile`], both called here rather than
//! reimplemented: the app and the CLI have to agree about what was destroyed
//! and which records that closed, and two copies of that rule would not.
//!
//! What is this file's own is the honesty about coverage. A provider whose
//! listing failed was never searched, and a provider with no credentials on
//! this computer was never searched either. Neither is the same as finding
//! nothing, and both are a bill, so they are counted apart from the strays.

use std::sync::Arc;

use jamstream_cloud::{Provider, ProviderKind, SweepFilter, SweepReport};

use crate::creds::{self, CredStore, EnvReader};

/// The providers to sweep, and the ones this computer cannot even ask.
pub struct Resolved {
    pub providers: Vec<Box<dyn Provider>>,
    /// Kinds with no credential here, so nothing could be looked for in them.
    pub unconfigured: Vec<ProviderKind>,
}

/// How the app decides what to sweep. A parameter for the same reason as
/// [`crate::app::Joiner`]: the real one reads the keychain and talks to four
/// clouds, and a test has neither, while the mock provider is something
/// [`creds::build_provider`] will never hand back.
pub type Resolver = Arc<dyn Fn() -> Resolved + Send + Sync>;

/// Every provider this computer holds a credential for, plus local, which
/// needs none. The same set the wizard offers, which is what makes the sweep
/// cover everything this app could have launched.
pub fn system_resolver(creds: Arc<dyn CredStore>, env: EnvReader) -> Resolver {
    Arc::new(move || {
        let mut resolved = Resolved {
            providers: Vec::new(),
            unconfigured: Vec::new(),
        };
        for kind in ProviderKind::ALL {
            match creds::build_provider(kind.as_str(), creds.as_ref(), &env) {
                Ok(provider) => resolved.providers.push(provider),
                Err(_) => resolved.unconfigured.push(kind),
            }
        }
        resolved
    })
}

/// What one sweep did, in the terms the Home card reports it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepOutcome {
    pub found: usize,
    pub destroyed: usize,
    /// Machines the destroy call refused. Still billing, named so.
    pub still_running: Vec<String>,
    /// `provider: reason` for a provider whose listing failed.
    pub unswept: Vec<String>,
    /// Providers with no credential here.
    pub unconfigured: Vec<ProviderKind>,
    /// Session records this closed, by short id.
    pub closed: Vec<String>,
    pub firewalls: usize,
    /// The records could not be rewritten. The machines are still gone; what
    /// failed is the bookkeeping, and saying so beats a silent disagreement
    /// between the app and `jamstream status`.
    pub records_error: Option<String>,
}

impl SweepOutcome {
    /// Reads a real report. `closed` is what [`jamstream_cli::sweep::reconcile`]
    /// answered.
    pub fn new(
        report: &SweepReport,
        closed: Result<Vec<String>, String>,
        unconfigured: Vec<ProviderKind>,
    ) -> SweepOutcome {
        let (closed, records_error) = match closed {
            Ok(ids) => (ids, None),
            Err(err) => (Vec::new(), Some(err)),
        };
        SweepOutcome {
            found: report.found.len(),
            destroyed: report.destroyed.len(),
            still_running: report
                .failed
                .iter()
                .map(|(inst, err)| format!("{} in {}: {err}", inst.id, inst.region.id))
                .collect(),
            unswept: report
                .unswept
                .iter()
                .map(|(kind, err)| format!("{kind}: could not be searched: {err}"))
                .collect(),
            unconfigured,
            closed: closed
                .iter()
                .map(|id| id.chars().take(8).collect())
                .collect(),
            firewalls: report.firewalls_removed.len(),
            records_error,
        }
    }

    /// True when every provider was searched and everything found is gone.
    /// Anything else means a machine may still be billing.
    pub fn accounted_for(&self) -> bool {
        self.still_running.is_empty() && self.unswept.is_empty() && self.unconfigured.is_empty()
    }

    /// The headline: what was found and what is gone.
    pub fn summary(&self) -> String {
        match (self.found, self.destroyed) {
            (0, _) => "Nothing tagged jamstream was running.".to_owned(),
            (found, destroyed) if found == destroyed => {
                format!("Stopped {destroyed} machine(s).")
            }
            (found, destroyed) => format!("Found {found} machine(s), stopped {destroyed}."),
        }
    }

    /// Everything that is not the headline and not a warning: what the sweep
    /// tidied up on the way past.
    pub fn notes(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.firewalls > 0 {
            out.push(format!(
                "Closed {} leftover session firewall(s).",
                self.firewalls
            ));
        }
        for id in &self.closed {
            out.push(format!(
                "Session {id} was recorded running; marked it ended."
            ));
        }
        out
    }

    /// What this sweep could not account for, each line a thing that may
    /// still be costing money. Empty when [`SweepOutcome::accounted_for`].
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        for line in &self.still_running {
            out.push(format!("Still running, could not stop it: {line}"));
        }
        out.extend(self.unswept.iter().cloned());
        if !self.unconfigured.is_empty() {
            out.push(format!(
                "Not searched, no credentials saved here: {}.",
                ProviderKind::name_list(self.unconfigured.iter().copied())
            ));
        }
        if let Some(err) = &self.records_error {
            out.push(format!("The session records could not be updated: {err}"));
        }
        out
    }
}

/// Sweeps, then reconciles, exactly as `jamstream sweep` does.
///
/// Never a dry run: the app's button is the act, and a dry run would leave
/// the records saying one thing and the account another.
pub async fn run(resolved: Resolved) -> SweepOutcome {
    let report = jamstream_cloud::sweep(&resolved.providers, SweepFilter::All, false).await;
    let closed =
        jamstream_cli::sweep::reconcile(&report, &resolved.providers).map_err(|e| e.to_string());
    SweepOutcome::new(&report, closed, resolved.unconfigured)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::MemStore;

    /// The set the button sweeps is every provider this computer holds a key
    /// for, and the rest are reported rather than dropped: a sweep that
    /// silently skipped DigitalOcean would read as an all-clear.
    #[test]
    fn the_resolver_separates_what_it_can_ask_from_what_it_cannot() {
        let env: EnvReader = Arc::new(|_: &str| None);
        let resolved = system_resolver(Arc::new(MemStore::default()), env)();
        let kinds: Vec<ProviderKind> = resolved.providers.iter().map(|p| p.kind()).collect();
        assert_eq!(
            kinds,
            vec![ProviderKind::Local],
            "no credentials saved, so only local resolves"
        );
        assert_eq!(
            resolved.unconfigured,
            vec![
                ProviderKind::DigitalOcean,
                ProviderKind::Aws,
                ProviderKind::Gcp
            ]
        );
    }

    /// A provider nobody could ask is not an all-clear, and neither is one
    /// with no key here. Both keep the outcome from reading as accounted for.
    #[test]
    fn coverage_gaps_are_never_reported_as_clean() {
        let clean = SweepOutcome::new(&SweepReport::default(), Ok(Vec::new()), Vec::new());
        assert!(clean.accounted_for());
        assert!(clean.warnings().is_empty());
        assert_eq!(clean.summary(), "Nothing tagged jamstream was running.");

        let unconfigured = SweepOutcome::new(
            &SweepReport::default(),
            Ok(Vec::new()),
            vec![ProviderKind::Aws],
        );
        assert!(!unconfigured.accounted_for());
        assert_eq!(unconfigured.warnings().len(), 1);
        assert!(unconfigured.warnings()[0].contains("aws"));
    }

    /// Reconcile failing is a bookkeeping failure, not a sweep failure: the
    /// machines are gone either way and the line has to say which is which.
    #[test]
    fn a_record_that_could_not_be_written_is_its_own_line() {
        let outcome = SweepOutcome::new(
            &SweepReport::default(),
            Err("permission denied".to_owned()),
            Vec::new(),
        );
        assert!(outcome.closed.is_empty());
        assert!(
            outcome.warnings()[0].contains("records could not be updated"),
            "{:?}",
            outcome.warnings()
        );
    }
}
