//! How long a session recording lives, and how that promise is kept.
//!
//! # The choices, and why these four
//!
//! [`Retention`] offers exactly four options: keep forever, or delete after
//! 7, 30, or 90 days. The set is deliberately closed.
//!
//! * They map onto how a band actually thinks about a rehearsal recording:
//!   *until next practice* (a week), *until we get around to mixing it* (a
//!   month), *until the next gig cycle* (a quarter), and *this one is a
//!   keeper*.
//! * Each one is a single provider-native lifecycle rule with a `days`
//!   number, so the promise is enforced by the object store rather than by
//!   JamStream remembering to come back. Nothing in the design needs a
//!   scheduler, a daemon, or the host's laptop to be online.
//! * A free-form day count was rejected on purpose. It doubles the test
//!   surface, invites off-by-one mistakes in a destructive operation, and
//!   buys nothing: the difference between 30 and 45 days does not change a
//!   decision, and the difference between 1 and 30 is a foot-gun.
//! * Four options fit on one line of UI next to the recording toggle, which
//!   is what "transparent and simple" has to mean in practice: the cost
//!   preview and the retention choice are visible in the same glance.
//!
//! The default is [`Retention::Days30`]. This is a deliberate product call
//! rather than a safe-looking one. Recording is opt-in per session, and the
//! thing being recorded is a rehearsal, not a master; the failure mode that
//! actually costs people money is a bucket that quietly accumulates 1.4 GB
//! per hour of jamming forever. A month is long enough to notice a recording
//! you want and act on it, and [`Retention::KeepForever`] sits right next to
//! it in the same picker for the takes that matter. The chosen retention is
//! shown, priced, and applied before the first byte is uploaded, so nobody
//! discovers the rule after the fact.
//!
//! # Enforcement
//!
//! [`crate::storage::ObjectStore::set_retention`] applies the choice
//! server-side wherever the provider has a lifecycle API, which is all three
//! clouds JamStream supports:
//!
//! | Provider | Mechanism | Enforced by |
//! |---|---|---|
//! | AWS S3 | `PUT /{bucket}?lifecycle` with [`s3_lifecycle_xml`] | S3 |
//! | DigitalOcean Spaces | the same call, [`LifecycleDialect::SpacesV1`] XML | Spaces |
//! | GCS | `PATCH /storage/v1/b/{bucket}` with [`gcs_lifecycle_patch`] | GCS |
//!
//! Where a target has no lifecycle API at all — a local session writing to
//! the host's own disk, or an S3-compatible endpoint that rejects the call —
//! the store returns [`RetentionEnforcement::Manual`] carrying
//! [`manual_note`], and the caller is expected to show that text instead of
//! implying a rule exists. An unenforceable retention choice must never be
//! reported as enforced.
//!
//! # Scoping
//!
//! Every rule is filtered to the JamStream key prefix. The bucket belongs to
//! the host and may hold anything; an expiration rule with no prefix filter
//! would be a data-loss bug, not a feature. This is why
//! [`crate::storage::stem_key`] sanitizes member names: a key that escaped
//! the prefix would also escape the retention rule.
//!
//! Each rule set additionally carries an abort-incomplete-multipart-upload
//! action ([`ABORT_INCOMPLETE_DAYS`]). A recording upload that dies with the
//! VM leaves orphaned parts that bill as storage forever and are invisible
//! to `list`; the upload path aborts them explicitly (see
//! [`crate::storage`]), and this rule is the backstop for the case where the
//! VM is gone before it can. It is present even under
//! [`Retention::KeepForever`], because "keep my recording" never meant "keep
//! the wreckage of a failed upload".

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::provider::{ProviderError, Result};
use crate::types::ProviderKind;

/// Id carried by the lifecycle rule JamStream writes, so a host can see at a
/// glance which rule is ours and so re-applying a choice replaces it instead
/// of stacking up duplicates.
pub const RULE_ID: &str = "jamstream-recording-retention";

/// Days after which an incomplete multipart upload is abandoned by the
/// provider. One day is plenty: a recording upload either finishes minutes
/// after the session or never, because the VM is gone.
pub const ABORT_INCOMPLETE_DAYS: u32 = 1;

/// Days per month used for prorating storage cost. Providers bill GB-month
/// by the hour, so this only affects how the preview is phrased.
pub const DAYS_PER_MONTH: u32 = 30;

/// How long a recording is kept before the object store deletes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Retention {
    /// No expiration rule; the recording stays until the host deletes it.
    KeepForever,
    Days7,
    Days30,
    Days90,
}

/// Delete after 30 days: see the module docs for the reasoning.
impl Default for Retention {
    fn default() -> Self {
        Retention::Days30
    }
}

impl Retention {
    /// Every choice, in the order they should be offered.
    pub const ALL: [Retention; 4] = [
        Retention::Days7,
        Retention::Days30,
        Retention::Days90,
        Retention::KeepForever,
    ];

    /// Stable wire/config token: `7d`, `30d`, `90d`, `forever`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Retention::KeepForever => "forever",
            Retention::Days7 => "7d",
            Retention::Days30 => "30d",
            Retention::Days90 => "90d",
        }
    }

    /// Human label for a picker or a cost line.
    pub fn label(&self) -> &'static str {
        match self {
            Retention::KeepForever => "Keep forever",
            Retention::Days7 => "Delete after 7 days",
            Retention::Days30 => "Delete after 30 days",
            Retention::Days90 => "Delete after 90 days",
        }
    }

    /// Days until deletion, or None when the recording is kept.
    pub fn days(&self) -> Option<u32> {
        match self {
            Retention::KeepForever => None,
            Retention::Days7 => Some(7),
            Retention::Days30 => Some(30),
            Retention::Days90 => Some(90),
        }
    }

    /// Days of storage a cost estimate should bill for. "Keep forever" has
    /// no end date, so it is quoted as one month and flagged as recurring by
    /// [`Retention::is_recurring`].
    pub fn billed_days(&self) -> u32 {
        self.days().unwrap_or(DAYS_PER_MONTH)
    }

    /// True when the storage charge repeats rather than ending.
    pub fn is_recurring(&self) -> bool {
        self.days().is_none()
    }
}

impl fmt::Display for Retention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Retention {
    type Err = ProviderError;

    fn from_str(s: &str) -> Result<Self> {
        let normalized = s.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "forever" | "keep" | "keepforever" | "keep-forever" => Ok(Retention::KeepForever),
            "7d" | "7" | "week" => Ok(Retention::Days7),
            "30d" | "30" | "month" => Ok(Retention::Days30),
            "90d" | "90" | "quarter" => Ok(Retention::Days90),
            other => Err(ProviderError::Other(format!(
                "unknown retention {other:?}; choose one of 7d, 30d, 90d, forever"
            ))),
        }
    }
}

/// Which flavor of the S3 lifecycle document to emit.
///
/// AWS accepts both the current `<Filter><Prefix>` form and the deprecated
/// bare `<Prefix>`; DigitalOcean Spaces implements the older shape and
/// rejects documents built around `<Filter>`. The two clouds share the whole
/// S3 code path except for this, so it is a parameter rather than a fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleDialect {
    /// `<Filter><Prefix>…</Prefix></Filter>`, the current S3 form.
    S3v2,
    /// Bare `<Prefix>…</Prefix>` inside the rule, which is what Spaces
    /// implements.
    SpacesV1,
}

/// The lifecycle document for `PUT /{bucket}?lifecycle`.
///
/// One rule, scoped to `prefix`, carrying an expiration when the choice has
/// one and always carrying the incomplete-multipart cleanup.
pub fn s3_lifecycle_xml(prefix: &str, retention: Retention, dialect: LifecycleDialect) -> String {
    let filter = match dialect {
        LifecycleDialect::S3v2 => {
            format!("<Filter><Prefix>{}</Prefix></Filter>", xml_escape(prefix))
        }
        LifecycleDialect::SpacesV1 => format!("<Prefix>{}</Prefix>", xml_escape(prefix)),
    };
    let expiration = match retention.days() {
        Some(days) => format!("<Expiration><Days>{days}</Days></Expiration>"),
        None => String::new(),
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<LifecycleConfiguration>\
<Rule>\
<ID>{RULE_ID}</ID>\
{filter}\
<Status>Enabled</Status>\
{expiration}\
<AbortIncompleteMultipartUpload><DaysAfterInitiation>{ABORT_INCOMPLETE_DAYS}</DaysAfterInitiation></AbortIncompleteMultipartUpload>\
</Rule>\
</LifecycleConfiguration>"
    )
}

/// The bucket patch body for `PATCH /storage/v1/b/{bucket}`.
///
/// GCS lifecycle is a whole-bucket field, so the patch replaces the rule
/// list. `matchesPrefix` keeps the rule off everything the host stores
/// outside the JamStream prefix. "Keep forever" sends an empty rule list,
/// which clears a previously applied expiration rather than leaving a stale
/// one behind.
///
/// GCS has no incomplete-multipart lifecycle action; it garbage-collects
/// abandoned resumable upload sessions itself after a week, which is why the
/// document has one rule where S3's has two actions.
pub fn gcs_lifecycle_patch(prefix: &str, retention: Retention) -> Value {
    let rules = match retention.days() {
        Some(days) => vec![json!({
            "action": { "type": "Delete" },
            "condition": { "age": days, "matchesPrefix": [prefix] },
        })],
        None => Vec::new(),
    };
    json!({ "lifecycle": { "rule": rules } })
}

/// What actually happened when a retention choice was applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionEnforcement {
    /// The provider now enforces the choice itself. `rule` is the document
    /// that was accepted, for display and for support requests.
    ServerSide {
        provider: ProviderKind,
        retention: Retention,
        rule: String,
    },
    /// No lifecycle API on this target. Nothing is enforced; `note` is the
    /// text a caller must show instead of claiming a rule exists.
    Manual { retention: Retention, note: String },
}

impl RetentionEnforcement {
    /// True only when a provider is keeping the promise for us.
    pub fn is_server_side(&self) -> bool {
        matches!(self, RetentionEnforcement::ServerSide { .. })
    }

    pub fn retention(&self) -> Retention {
        match self {
            RetentionEnforcement::ServerSide { retention, .. }
            | RetentionEnforcement::Manual { retention, .. } => *retention,
        }
    }

    /// One line fit to show a host after a recording uploads.
    pub fn describe(&self) -> String {
        match self {
            RetentionEnforcement::ServerSide {
                provider,
                retention: r,
                ..
            } => match r.days() {
                Some(days) => format!(
                    "{provider} will delete this recording {days} days from now (lifecycle rule {RULE_ID})"
                ),
                None => format!(
                    "kept until you delete it; no expiration rule set on {provider} (rule {RULE_ID} only cleans up failed uploads)"
                ),
            },
            RetentionEnforcement::Manual { note, .. } => note.clone(),
        }
    }
}

/// The note to show when nothing server-side can be arranged.
pub fn manual_note(retention: Retention) -> String {
    match retention.days() {
        Some(days) => format!(
            "This storage target has no lifecycle API, so \"{}\" cannot be enforced for you: \
             nothing will delete the recording after {days} days unless you do. Delete it by hand, \
             or point the recording at a bucket on AWS S3, DigitalOcean Spaces, or Google Cloud \
             Storage, where JamStream sets a real server-side rule.",
            retention.label()
        ),
        None => "This storage target has no lifecycle API, which is what \"Keep forever\" wanted \
                 anyway: the recording stays until you delete it."
            .to_owned(),
    }
}

/// Minimal XML text escaping for values interpolated into a lifecycle
/// document. Prefixes come from us, but a bucket-level prefix override
/// arrives from a host's config, and an unescaped `&` there would produce a
/// malformed document rather than an obvious error.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trip_for_every_choice() {
        for r in Retention::ALL {
            assert_eq!(r.as_str().parse::<Retention>().unwrap(), r);
            assert!(!r.label().is_empty());
        }
        assert_eq!(Retention::ALL.len(), 4);
    }

    #[test]
    fn parse_accepts_friendly_spellings_and_rejects_junk() {
        assert_eq!("30".parse::<Retention>().unwrap(), Retention::Days30);
        assert_eq!(" MONTH ".parse::<Retention>().unwrap(), Retention::Days30);
        assert_eq!("week".parse::<Retention>().unwrap(), Retention::Days7);
        assert_eq!(
            "keep-forever".parse::<Retention>().unwrap(),
            Retention::KeepForever
        );
        let err = "45d".parse::<Retention>().unwrap_err();
        assert!(err.to_string().contains("7d, 30d, 90d, forever"));
    }

    #[test]
    fn default_is_thirty_days() {
        assert_eq!(Retention::default(), Retention::Days30);
        assert_eq!(Retention::default().days(), Some(30));
        assert!(!Retention::default().is_recurring());
    }

    #[test]
    fn billed_days_quotes_forever_as_one_month() {
        assert_eq!(Retention::Days7.billed_days(), 7);
        assert_eq!(Retention::Days90.billed_days(), 90);
        assert_eq!(Retention::KeepForever.billed_days(), DAYS_PER_MONTH);
        assert!(Retention::KeepForever.is_recurring());
    }

    #[test]
    fn s3_xml_carries_prefix_expiration_and_mpu_cleanup() {
        let xml = s3_lifecycle_xml(
            "jamstream/recordings/",
            Retention::Days30,
            LifecycleDialect::S3v2,
        );
        assert!(
            xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?><LifecycleConfiguration>")
        );
        assert!(xml.contains("<ID>jamstream-recording-retention</ID>"));
        assert!(xml.contains("<Filter><Prefix>jamstream/recordings/</Prefix></Filter>"));
        assert!(xml.contains("<Status>Enabled</Status>"));
        assert!(xml.contains("<Expiration><Days>30</Days></Expiration>"));
        assert!(xml.contains(
            "<AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload>"
        ));
        assert!(xml.ends_with("</LifecycleConfiguration>"));
    }

    #[test]
    fn spaces_dialect_uses_the_bare_prefix_element() {
        let xml = s3_lifecycle_xml("p/", Retention::Days7, LifecycleDialect::SpacesV1);
        assert!(xml.contains("<Prefix>p/</Prefix>"));
        assert!(
            !xml.contains("<Filter>"),
            "Spaces rejects the Filter form: {xml}"
        );
        assert!(xml.contains("<Days>7</Days>"));
    }

    #[test]
    fn keep_forever_omits_expiration_but_keeps_cleanup() {
        for dialect in [LifecycleDialect::S3v2, LifecycleDialect::SpacesV1] {
            let xml = s3_lifecycle_xml("p/", Retention::KeepForever, dialect);
            assert!(!xml.contains("<Expiration>"), "{xml}");
            assert!(xml.contains("<AbortIncompleteMultipartUpload>"), "{xml}");
        }
    }

    #[test]
    fn prefixes_are_xml_escaped() {
        let xml = s3_lifecycle_xml("a&b/<c>/", Retention::Days7, LifecycleDialect::S3v2);
        assert!(xml.contains("a&amp;b/&lt;c&gt;/"));
        assert!(!xml.contains("a&b"));
    }

    #[test]
    fn gcs_patch_scopes_the_rule_to_the_prefix() {
        let patch = gcs_lifecycle_patch("jamstream/recordings/", Retention::Days90);
        assert_eq!(patch["lifecycle"]["rule"][0]["action"]["type"], "Delete");
        assert_eq!(patch["lifecycle"]["rule"][0]["condition"]["age"], 90);
        assert_eq!(
            patch["lifecycle"]["rule"][0]["condition"]["matchesPrefix"][0],
            "jamstream/recordings/"
        );
        assert_eq!(
            patch["lifecycle"]["rule"].as_array().unwrap().len(),
            1,
            "one rule, so re-applying replaces instead of stacking"
        );
    }

    #[test]
    fn gcs_keep_forever_clears_the_rule_list() {
        let patch = gcs_lifecycle_patch("p/", Retention::KeepForever);
        assert_eq!(
            patch["lifecycle"]["rule"].as_array().unwrap().len(),
            0,
            "an empty list clears a previously set expiration"
        );
    }

    #[test]
    fn enforcement_describes_itself_honestly() {
        let server = RetentionEnforcement::ServerSide {
            provider: ProviderKind::Aws,
            retention: Retention::Days30,
            rule: "<xml/>".to_owned(),
        };
        assert!(server.is_server_side());
        assert!(
            server
                .describe()
                .contains("aws will delete this recording 30 days")
        );

        let forever = RetentionEnforcement::ServerSide {
            provider: ProviderKind::Gcp,
            retention: Retention::KeepForever,
            rule: "{}".to_owned(),
        };
        assert!(forever.describe().contains("kept until you delete it"));

        let manual = RetentionEnforcement::Manual {
            retention: Retention::Days30,
            note: manual_note(Retention::Days30),
        };
        assert!(!manual.is_server_side());
        // The manual note must not claim anything will happen on its own.
        assert!(manual.describe().contains("cannot be enforced for you"));
        assert!(manual.describe().contains("30 days unless you do"));
        assert_eq!(manual.retention(), Retention::Days30);
    }

    #[test]
    fn manual_note_for_keep_forever_is_not_a_warning() {
        let note = manual_note(Retention::KeepForever);
        assert!(note.contains("stays until you delete it"));
        assert!(!note.contains("cannot be enforced"));
    }
}
