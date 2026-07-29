//! The Recording tab: where takes go, and the key that writes them.
//!
//! Always present, because a bucket and a key are set up once per computer
//! rather than once per session, and a host who has to configure storage in the
//! middle of a launch is configuring it at the worst moment.
//!
//! The key is checked when it is saved and never when Record is pressed: the
//! Check button writes a probe object under a recording prefix and deletes it,
//! through the same call `jamstream host --bucket` makes before a machine is
//! paid for. Only a passing check writes the keychain, so a bucket that refuses
//! fails while the host is pasting rather than mid-song.

use std::path::PathBuf;
use std::sync::Arc;

use data_encoding::HEXLOWER;
use egui::{RichText, TextEdit, Ui};
use jamstream_cloud::cloudinit::{RecordingStorage, StorageCredential};
use jamstream_cloud::{ProviderKind, RegionId, Retention};
use jamstream_protocol::ids::SessionId;
use zeroize::Zeroize;

use crate::creds::{self, CredStore, EnvReader};
use crate::exec::{Executor, Job};
use crate::prefs::RecordingPrefs;
use crate::theme;
use crate::widgets::{pick_row, row_cell};

/// The providers a bucket can live on, in the wizard's own order. Local is
/// absent: a session on this computer records to this computer's disk.
pub const STORAGE_PROVIDERS: [ProviderKind; 3] = [
    ProviderKind::DigitalOcean,
    ProviderKind::Aws,
    ProviderKind::Gcp,
];

/// Field width inside the 340 px drawer, with room for the label above it.
const FIELD_W: f32 = 300.0;

/// What a session records, once a host turns it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecordingChoice {
    /// Nothing is captured. The default, always.
    #[default]
    Off,
    /// The broadcast mix listeners hear.
    MixOnly,
    /// The mix plus one stereo stem per musician, about five times the size.
    MixAndStems,
}

impl RecordingChoice {
    pub const ALL: [RecordingChoice; 3] = [
        RecordingChoice::Off,
        RecordingChoice::MixOnly,
        RecordingChoice::MixAndStems,
    ];

    pub fn label(self) -> &'static str {
        match self {
            RecordingChoice::Off => "off",
            RecordingChoice::MixOnly => "mix only",
            RecordingChoice::MixAndStems => "mix and stems",
        }
    }

    pub fn is_on(self) -> bool {
        self != RecordingChoice::Off
    }

    pub fn stems(self) -> bool {
        self == RecordingChoice::MixAndStems
    }
}

/// What this computer can record to, as plain data.
///
/// The wizard is handed one of these every frame rather than a reference to the
/// tab, so there is one place the answer is computed and no second copy of it to
/// go stale. A fixture builds one directly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordingSetup {
    /// The bucket for the provider being launched, if one is configured.
    pub bucket: Option<crate::prefs::Bucket>,
    /// A key for that provider is on this computer.
    pub has_key: bool,
    pub retention: Retention,
}

impl RecordingSetup {
    /// Whether a cloud session on this provider can be armed at all, and why
    /// not when it cannot. The reason points at the tab that fixes it.
    pub fn refusal(&self) -> Option<String> {
        match (&self.bucket, self.has_key) {
            (Some(_), true) => None,
            (Some(_), false) => Some(
                "No storage key on this computer. The Recording tab in Settings takes one."
                    .to_owned(),
            ),
            (None, true) => Some(
                "No bucket set for this provider. The Recording tab in Settings takes one."
                    .to_owned(),
            ),
            (None, false) => Some(
                "Recording needs a bucket and a storage key. Set both in the Recording tab \
                 in Settings."
                    .to_owned(),
            ),
        }
    }
}

/// The Recording tab's state. Typed key values live here until a check passes;
/// after that they live in the keychain and nowhere else.
pub struct RecordingPanel {
    /// Which provider's bucket is being configured.
    pub provider: ProviderKind,
    pub bucket: String,
    pub region: String,
    key_id: String,
    secret: String,
    /// The fields render masked; this is the explicit reveal, as the provider
    /// setup panes have.
    pub reveal: bool,
    pub retention: Retention,
    /// What the last check said, verbatim on failure.
    pub check_result: Option<Result<(), String>>,
    /// Why the preferences file could not be read or written, if it could not.
    pub error: Option<String>,
    check_job: Option<Job<Result<(), String>>>,
    /// Which providers have a key on this computer, in
    /// [`STORAGE_PROVIDERS`] order.
    ///
    /// Cached because reading it is an operating system call: the tab asks the
    /// question of three providers, and the wizard asks it of one, on every
    /// frame either is on screen. Refreshed when the answer can have changed,
    /// which is a save, a forget, and construction.
    saved: [bool; STORAGE_PROVIDERS.len()],
    prefs: RecordingPrefs,
    /// Where the preferences are kept, or None when they last only as long as
    /// this process: a test or a fixture must not read the bucket the developer
    /// running it happens to have, and must certainly not write theirs.
    prefs_path: Option<PathBuf>,
    creds: Arc<dyn CredStore>,
    env: EnvReader,
    exec: Arc<Executor>,
}

/// Neither half of a key pair reaches a formatter, the way
/// [`StorageCredential`] and the self-destruct token already do not. A panel in
/// a debug line is how a secret ends up in a log or a snapshot.
impl std::fmt::Debug for RecordingPanel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingPanel")
            .field("provider", &self.provider)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("key_id", &"<redacted>")
            .field("secret", &"<redacted>")
            .field("retention", &self.retention)
            .finish()
    }
}

impl Drop for RecordingPanel {
    fn drop(&mut self) {
        self.key_id.zeroize();
        self.secret.zeroize();
    }
}

impl RecordingPanel {
    /// The panel over the preferences at `prefs_path`, or over preferences that
    /// last only as long as this process when it is None. A file that will not
    /// decode leaves the fields empty and puts its reason on screen.
    pub fn new(
        creds: Arc<dyn CredStore>,
        env: EnvReader,
        exec: Arc<Executor>,
        prefs_path: Option<PathBuf>,
    ) -> RecordingPanel {
        let (prefs, error) = match prefs_path.as_deref().map(RecordingPrefs::load_from) {
            Some(Ok(prefs)) => (prefs, None),
            Some(Err(err)) => (RecordingPrefs::default(), Some(err)),
            None => (RecordingPrefs::default(), None),
        };
        let provider = STORAGE_PROVIDERS
            .iter()
            .copied()
            .find(|p| prefs.bucket(p.as_str()).is_some())
            .unwrap_or(ProviderKind::DigitalOcean);
        let mut panel = RecordingPanel {
            provider,
            bucket: String::new(),
            region: String::new(),
            key_id: String::new(),
            secret: String::new(),
            reveal: false,
            retention: prefs.retention(),
            check_result: None,
            error,
            check_job: None,
            saved: [false; STORAGE_PROVIDERS.len()],
            prefs,
            prefs_path,
            creds,
            env,
            exec,
        };
        panel.load_fields();
        panel.refresh_saved();
        panel
    }

    /// Asks the keychain and the environment which providers have a key. The
    /// only place either is read for that answer, and public so a key that
    /// arrived from outside this panel can be noticed on demand rather than on
    /// the next frame.
    pub fn refresh_saved(&mut self) {
        for (slot, provider) in self.saved.iter_mut().zip(STORAGE_PROVIDERS) {
            *slot = creds::has_storage_credential(self.creds.as_ref(), &self.env, provider);
        }
    }

    /// Pulls the selected provider's saved bucket into the fields.
    fn load_fields(&mut self) {
        let bucket = self.prefs.bucket(self.provider.as_str());
        self.bucket = bucket.map(|b| b.name.clone()).unwrap_or_default();
        self.region = bucket.map(|b| b.region.clone()).unwrap_or_default();
    }

    pub fn select_provider(&mut self, provider: ProviderKind) {
        if self.provider == provider {
            return;
        }
        self.provider = provider;
        self.check_result = None;
        self.key_id.zeroize();
        self.key_id.clear();
        self.secret.zeroize();
        self.secret.clear();
        self.load_fields();
    }

    /// True when a key for the selected provider is on this computer already,
    /// whether it was pasted here or set in the environment for the CLI.
    pub fn key_saved(&self) -> bool {
        self.has_key(self.provider)
    }

    fn has_key(&self, provider: ProviderKind) -> bool {
        STORAGE_PROVIDERS
            .iter()
            .position(|p| *p == provider)
            .is_some_and(|i| self.saved[i])
    }

    pub fn busy(&self) -> bool {
        self.check_job.is_some()
    }

    pub fn retention(&self) -> Retention {
        self.retention
    }

    /// What this computer can record a session on `provider` to. `None` for a
    /// provider that has no bucket at all, which is what local is.
    pub fn setup(&self, provider: Option<ProviderKind>) -> RecordingSetup {
        let Some(provider) = provider.filter(|p| STORAGE_PROVIDERS.contains(p)) else {
            return RecordingSetup {
                retention: self.retention,
                ..RecordingSetup::default()
            };
        };
        RecordingSetup {
            bucket: self.prefs.bucket(provider.as_str()).cloned(),
            has_key: self.has_key(provider),
            retention: self.retention,
        }
    }

    /// Sets one provider's bucket without going through a check. The app has no
    /// path to this: it exists so a test or a fixture can hold a configured
    /// computer without a network round trip.
    #[doc(hidden)]
    pub fn remember_bucket(&mut self, provider: ProviderKind, name: &str, region: &str) {
        self.prefs.set_bucket(provider.as_str(), name, region);
        if self.provider == provider {
            self.load_fields();
        }
    }

    /// Puts a key pair in the fields, as a host who has just pasted one has. The
    /// app reaches this only through typing; it is here so a snapshot can hold
    /// the one surface where a storage key is present at all and prove it is
    /// masked.
    #[doc(hidden)]
    pub fn type_key(&mut self, access_key_id: &str, secret_access_key: &str) {
        self.key_id = access_key_id.to_owned();
        self.secret = secret_access_key.to_owned();
    }

    /// The config the Check button proves, from the typed fields when there are
    /// any and from the saved key otherwise, so an existing setup can be
    /// rechecked without retyping the pair.
    fn storage_to_check(&self) -> Result<RecordingStorage, String> {
        // The fields in the order they are on screen, so the first thing a host
        // is told is the first thing they can fix.
        if self.bucket.trim().is_empty() {
            return Err("name the bucket takes should go to".to_owned());
        }
        if self.region.trim().is_empty() {
            return Err("name the region the bucket is in".to_owned());
        }
        let (id, secret) = (self.key_id.trim(), self.secret.trim());
        let credential = if id.is_empty() && secret.is_empty() {
            creds::storage_credential(self.creds.as_ref(), &self.env, self.provider)?
        } else if id.is_empty() || secret.is_empty() {
            return Err("paste both the access key id and the secret".to_owned());
        } else {
            StorageCredential::KeyPair {
                access_key_id: id.to_owned(),
                secret_access_key: secret.to_owned(),
            }
        };
        jamstream_cli::storage::storage_for_launch(
            self.provider,
            &self.bucket,
            &RegionId::new(self.region.trim()),
            self.retention,
            || Ok(credential),
            false,
        )
        .map_err(|e| e.to_string())
    }

    /// Runs the check on the executor. A field error surfaces immediately
    /// without spawning anything.
    pub fn begin_check(&mut self) -> bool {
        if self.check_job.is_some() {
            return false;
        }
        match self.storage_to_check() {
            Ok(storage) => {
                self.check_result = None;
                // A prefix of its own, from a session id nothing else will
                // ever hold, so a check writes where a take would and touches
                // no session's objects.
                let probe_session = hex(&SessionId::generate().0);
                self.check_job = Some(self.exec.run(async move {
                    jamstream_cli::host::probe_bucket(&storage, &probe_session)
                        .await
                        .map_err(|e| e.to_string())
                }));
                true
            }
            Err(err) => {
                self.check_result = Some(Err(err));
                false
            }
        }
    }

    /// A passing check saves the bucket and the key pair; a failing one saves
    /// nothing and shows the provider's reason verbatim.
    pub fn apply_check_result(&mut self, result: Result<(), String>) {
        if result.is_err() {
            self.check_result = Some(result);
            return;
        }
        let (id, secret) = (self.key_id.trim().to_owned(), self.secret.trim().to_owned());
        if !id.is_empty() {
            let saved =
                creds::save_storage_credential(self.creds.as_ref(), self.provider, &id, &secret);
            self.refresh_saved();
            if let Err(err) = saved {
                self.check_result = Some(Err(format!(
                    "the key works but saving it on this computer failed: {err}"
                )));
                return;
            }
        }
        // Kept nowhere but the keychain from here on.
        self.key_id.zeroize();
        self.key_id.clear();
        self.secret.zeroize();
        self.secret.clear();
        self.check_result = Some(match self.save_prefs() {
            Ok(()) => Ok(()),
            Err(err) => Err(format!(
                "the bucket works but remembering it on this computer failed: {err}"
            )),
        });
    }

    /// Writes the bucket and the retention choice to the preferences file.
    fn save_prefs(&mut self) -> Result<(), String> {
        self.prefs
            .set_bucket(self.provider.as_str(), &self.bucket, &self.region);
        self.prefs.set_retention(self.retention);
        self.write_prefs()
    }

    fn write_prefs(&self) -> Result<(), String> {
        match &self.prefs_path {
            Some(path) => self.prefs.save_to(path),
            None => Ok(()),
        }
    }

    /// Forgets the key and the bucket for the selected provider.
    pub fn forget(&mut self) {
        creds::forget_storage_credential(self.creds.as_ref(), self.provider);
        self.refresh_saved();
        self.prefs.set_bucket(self.provider.as_str(), "", "");
        self.bucket.clear();
        self.region.clear();
        self.check_result = None;
        self.error = self.write_prefs().err();
    }

    /// Applies a finished check. Called once per frame from the tab.
    pub fn poll(&mut self) {
        if let Some(job) = &mut self.check_job
            && let Some(result) = job.poll()
        {
            self.check_job = None;
            self.apply_check_result(result);
        }
    }
}

// Rendering. Laid out for the 340 px drawer: every label sits above its field
// rather than beside it, because a bucket name and a key are both longer than
// the space a label would leave.

impl RecordingPanel {
    pub fn ui(&mut self, ui: &mut Ui) {
        self.poll();
        ui.label(theme::title(ui, "Recording"));
        note(
            ui,
            "Where a cloud session's takes go. Recording stays off until you turn it on \
             for a session.",
        );
        ui.add_space(theme::SPACE_MD);
        self.provider_rows(ui);
        ui.add_space(theme::SPACE_MD);
        self.bucket_fields(ui);
        ui.add_space(theme::SPACE_MD);
        self.key_fields(ui);
        ui.add_space(theme::SPACE_MD);
        self.actions(ui);
        ui.add_space(theme::SPACE_LG);
        self.retention_rows(ui);
        if let Some(err) = self.error.clone() {
            let p = theme::palette_of(ui);
            ui.add_space(theme::SPACE_SM);
            ui.add(egui::Label::new(RichText::new(err).color(p.danger)).wrap());
        }
        ui.add_space(theme::SPACE_MD);
        note(
            ui,
            "A session on this computer records to this computer's disk and needs no bucket.",
        );
    }

    fn provider_rows(&mut self, ui: &mut Ui) {
        let mut pick = None;
        for provider in STORAGE_PROVIDERS {
            let saved = self.has_key(provider);
            let bucket = self.prefs.bucket(provider.as_str()).map(|b| b.name.clone());
            let response = pick_row(
                ui,
                provider.as_str(),
                self.provider == provider,
                true,
                |ui| {
                    row_cell(ui, 110.0, |ui| {
                        ui.label(provider.as_str());
                    });
                    let word = match (&bucket, saved) {
                        (Some(name), true) => name.clone(),
                        (Some(name), false) => format!("{name}, no key"),
                        (None, true) => "key, no bucket".to_owned(),
                        (None, false) => "not set up".to_owned(),
                    };
                    ui.add(egui::Label::new(theme::muted(ui, word).small()).truncate());
                },
            );
            if response.clicked() {
                pick = Some(provider);
            }
        }
        if let Some(provider) = pick {
            self.select_provider(provider);
        }
    }

    fn bucket_fields(&mut self, ui: &mut Ui) {
        ui.label(theme::muted(ui, "bucket"));
        ui.add(
            TextEdit::singleline(&mut self.bucket)
                .desired_width(FIELD_W)
                .hint_text("my-jams"),
        );
        ui.label(theme::muted(ui, "bucket region"));
        ui.add(
            TextEdit::singleline(&mut self.region)
                .desired_width(FIELD_W)
                .hint_text(match self.provider {
                    ProviderKind::DigitalOcean => "nyc3",
                    ProviderKind::Gcp => "europe-west1",
                    _ => "eu-west-1",
                }),
        );
        note(
            ui,
            "Host in the bucket's own region and the upload is free.",
        );
    }

    /// The key pair, masked with a reveal, exactly as the provider setup panes
    /// hold a token. The second credential this product asks for, and the only
    /// one that rides on the machine.
    fn key_fields(&mut self, ui: &mut Ui) {
        ui.label(theme::muted(ui, "storage key"));
        if self.key_saved() {
            note(
                ui,
                "A key for this provider is on this computer. Paste a new pair to replace it.",
            );
        }
        let mask = !self.reveal;
        let id_label = ui.label(theme::muted(ui, "access key id")).id;
        ui.add(
            TextEdit::singleline(&mut self.key_id)
                .desired_width(FIELD_W)
                .password(mask)
                .hint_text("paste the key id"),
        )
        .labelled_by(id_label);
        let secret_label = ui.label(theme::muted(ui, "secret access key")).id;
        ui.add(
            TextEdit::singleline(&mut self.secret)
                .desired_width(FIELD_W)
                .password(mask)
                .hint_text("paste the secret"),
        )
        .labelled_by(secret_label);
        note(
            ui,
            "Never the key that launches machines: this one is written to the session \
             machine, so scope it to writing the recordings prefix of one bucket. Each \
             provider's page makes exactly that key.",
        );
    }

    fn actions(&mut self, ui: &mut Ui) {
        let checking = self.busy();
        let mut check = false;
        let mut reveal = false;
        let mut forget = false;
        ui.horizontal(|ui| {
            check = ui
                .add_enabled(!checking, egui::Button::new("Check"))
                .on_hover_text("writes one small object to the bucket and deletes it")
                .clicked();
            reveal = ui
                .button(if self.reveal { "Hide" } else { "Show" })
                .clicked();
            if self.key_saved() {
                forget = ui
                    .button("Forget")
                    .on_hover_text("deletes the key from this computer's keychain")
                    .clicked();
            }
        });
        if checking {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().color(theme::palette_of(ui).text_muted));
                ui.label(theme::muted(ui, "asking the bucket"));
            });
        }
        match &self.check_result {
            Some(Ok(())) => {
                let p = theme::palette_of(ui);
                ui.add(
                    egui::Label::new(
                        RichText::new("The bucket accepted a write. Saved to your keychain.")
                            .color(p.meter_green),
                    )
                    .wrap(),
                );
            }
            Some(Err(err)) => {
                let p = theme::palette_of(ui);
                ui.add(egui::Label::new(RichText::new(err.clone()).color(p.danger)).wrap());
            }
            None => {}
        }
        if check {
            self.begin_check();
        }
        if reveal {
            self.reveal = !self.reveal;
        }
        if forget {
            self.forget();
        }
    }

    /// The default for new sessions. Saved when it changes, because a host who
    /// picks 90 days once means it for the next session too.
    fn retention_rows(&mut self, ui: &mut Ui) {
        ui.label(theme::muted(ui, "keep takes for"));
        let mut pick = None;
        for retention in Retention::ALL {
            let response = pick_row(
                ui,
                retention.as_str(),
                self.retention == retention,
                true,
                |ui| {
                    row_cell(ui, 60.0, |ui| {
                        ui.label(theme::mono(ui, retention.as_str()));
                    });
                    ui.add(
                        egui::Label::new(theme::muted(ui, retention.label()).small()).truncate(),
                    );
                },
            );
            if response.clicked() {
                pick = Some(retention);
            }
        }
        if let Some(retention) = pick {
            self.retention = retention;
            self.prefs.set_retention(retention);
            self.error = self.write_prefs().err();
        }
        note(
            ui,
            "A rule on the bucket itself, so it keeps being enforced after the machine is \
             gone.",
        );
    }
}

/// One wrapped muted line, which is most of the prose in this tab. Wrapped
/// rather than truncated: at the drawer's width every one of these is two lines
/// or more, and a sentence about a credential is not something to elide.
fn note(ui: &mut Ui, text: impl Into<String>) {
    ui.add(egui::Label::new(theme::muted(ui, text).small()).wrap());
}

fn hex(bytes: &[u8]) -> String {
    HEXLOWER.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::creds::MemStore;

    const SECRET: &str = "0000-fake-storage-secret";

    fn panel() -> (Arc<MemStore>, RecordingPanel) {
        let store = Arc::new(MemStore::default());
        let env: EnvReader = Arc::new(|_| None);
        // No path: nothing here reads or writes the preferences of whoever runs
        // the suite.
        let panel = RecordingPanel::new(store.clone(), env, Arc::new(Executor::new()), None);
        (store, panel)
    }

    #[test]
    fn recording_is_off_by_default_and_stems_are_the_opt_in() {
        assert_eq!(RecordingChoice::default(), RecordingChoice::Off);
        assert!(!RecordingChoice::Off.is_on());
        assert!(!RecordingChoice::MixOnly.stems());
        assert!(RecordingChoice::MixAndStems.stems());
    }

    #[test]
    fn a_passing_check_saves_the_key_and_a_failing_one_saves_nothing() {
        let (store, mut panel) = panel();
        panel.provider = ProviderKind::Aws;
        panel.bucket = "my-jams".to_owned();
        panel.region = "eu-west-1".to_owned();
        panel.key_id = "AKIDSTORAGE".to_owned();
        panel.secret = SECRET.to_owned();

        panel.apply_check_result(Err("cannot write to my-jams: 403".to_owned()));
        let (id, secret) = creds::storage_key_fields(ProviderKind::Aws);
        assert_eq!(store.get(id.0, id.1), None, "a failure must save nothing");
        assert_eq!(store.get(secret.0, secret.1), None);
        assert!(!panel.key_saved());

        panel.apply_check_result(Ok(()));
        assert_eq!(store.get(id.0, id.1).as_deref(), Some("AKIDSTORAGE"));
        assert_eq!(store.get(secret.0, secret.1).as_deref(), Some(SECRET));
        assert!(panel.key_saved());
        // Nothing of the pair is left in the panel once it is in the keychain.
        assert!(panel.key_id.is_empty() && panel.secret.is_empty());
    }

    #[test]
    fn a_check_with_half_a_pair_or_no_region_spawns_nothing() {
        let (_, mut panel) = panel();
        panel.bucket = "my-jams".to_owned();
        panel.region = "nyc3".to_owned();
        panel.key_id = "DO00ID".to_owned();
        assert!(!panel.begin_check());
        assert!(matches!(panel.check_result, Some(Err(ref e)) if e.contains("both")));
        assert!(!panel.busy());

        panel.secret = SECRET.to_owned();
        panel.region.clear();
        assert!(!panel.begin_check());
        assert!(matches!(panel.check_result, Some(Err(ref e)) if e.contains("region")));

        // An empty bucket is a typo, not a bucket.
        panel.region = "nyc3".to_owned();
        panel.bucket.clear();
        assert!(!panel.begin_check());
        assert!(matches!(panel.check_result, Some(Err(_))));
        assert!(!panel.busy());
    }

    /// What the wizard is handed: a bucket, whether there is a key for it, and
    /// the reason when either is missing. Half a setup must not read as armable.
    #[test]
    fn the_setup_the_wizard_reads_needs_both_a_bucket_and_a_key() {
        let (store, mut panel) = panel();
        let refusal = panel
            .setup(Some(ProviderKind::DigitalOcean))
            .refusal()
            .expect("nothing is set up");
        assert!(
            refusal.contains("bucket") && refusal.contains("key"),
            "{refusal}"
        );

        panel.prefs.set_bucket("digitalocean", "our-takes", "nyc3");
        let refusal = panel
            .setup(Some(ProviderKind::DigitalOcean))
            .refusal()
            .expect("a bucket with no key cannot be written");
        assert!(refusal.contains("key"), "{refusal}");
        assert!(refusal.contains("Recording tab"), "{refusal}");

        creds::save_storage_credential(&*store, ProviderKind::DigitalOcean, "DO00ID", SECRET)
            .expect("save");
        // A key that arrived from outside the panel is noticed when it is asked
        // for rather than on every frame; the keychain is an operating system
        // call and the drawer would make five of them per frame.
        panel.refresh_saved();
        let setup = panel.setup(Some(ProviderKind::DigitalOcean));
        assert_eq!(setup.refusal(), None);
        assert_eq!(setup.bucket.expect("a bucket").name, "our-takes");
        assert_eq!(setup.retention, Retention::Days30);

        // Another provider's bucket is not this one's, and neither is local's.
        assert!(panel.setup(Some(ProviderKind::Aws)).refusal().is_some());
        assert!(panel.setup(Some(ProviderKind::Local)).refusal().is_some());
        assert!(panel.setup(None).refusal().is_some());
    }

    /// Neither half of the pair may reach a debug line, which is where a secret
    /// ends up in a log.
    #[test]
    fn debug_redacts_both_halves_of_the_key() {
        let (_, mut panel) = panel();
        panel.key_id = "AKIDSTORAGE".to_owned();
        panel.secret = SECRET.to_owned();
        let text = format!("{panel:?}");
        assert!(!text.contains(SECRET), "{text}");
        assert!(!text.contains("AKIDSTORAGE"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
    }

    #[test]
    fn switching_provider_drops_the_typed_pair() {
        let (_, mut panel) = panel();
        panel.key_id = "AKIDSTORAGE".to_owned();
        panel.secret = SECRET.to_owned();
        panel.check_result = Some(Ok(()));
        panel.select_provider(ProviderKind::Gcp);
        assert!(panel.key_id.is_empty() && panel.secret.is_empty());
        assert!(panel.check_result.is_none());
        assert_eq!(panel.provider, ProviderKind::Gcp);
    }

    /// A saved key can be rechecked without retyping it, which is what a host
    /// does after changing the bucket's permissions.
    #[test]
    fn a_saved_key_is_rechecked_without_retyping_it() {
        let (store, mut panel) = panel();
        panel.bucket = "our-takes".to_owned();
        panel.region = "nyc3".to_owned();
        creds::save_storage_credential(&*store, ProviderKind::DigitalOcean, "DO00ID", SECRET)
            .expect("save");
        let storage = panel.storage_to_check().expect("the saved pair");
        let StorageCredential::KeyPair {
            access_key_id,
            secret_access_key,
        } = storage.credential;
        assert_eq!(access_key_id, "DO00ID");
        assert_eq!(secret_access_key, SECRET);
    }
}
