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
//! # Scoping, and why the document is merged rather than written
//!
//! Every rule is filtered to the JamStream key prefix. The bucket belongs to
//! the host and may hold anything; an expiration rule with no prefix filter
//! would be a data-loss bug, not a feature. This is why
//! [`crate::storage::sanitize_component`] sanitizes the parts of a key: a key
//! that escaped the prefix would also escape the retention rule.
//!
//! Retention is a per-session choice, so the rule is per session too: its id
//! is [`rule_id`] of that session's prefix. Both provider calls replace the
//! bucket's whole rule list, so applying one session's choice by writing one
//! rule deleted every other rule on the bucket, JamStream's and the host's
//! alike. The second recorded session in a bucket silently removed the
//! first's expiry rule and those takes then lived forever, billing. So
//! [`merge_s3_lifecycle`] and [`merge_gcs_lifecycle`] read the document that
//! is there, keep every rule that is not this session's verbatim, and add
//! ours. Reading is a separate permission from writing on S3
//! (`s3:GetLifecycleConfiguration`); without it nothing is written, because a
//! blind write would take the host's own rules with it.
//!
//! Two consequences worth knowing:
//!
//! * Two hosts applying retention to one bucket at the same instant can lose
//!   one of the two rules, because neither API has a compare-and-set. The
//!   window is one round trip and the loss is a rule, never an object: the
//!   takes are kept and keep billing until someone re-arms recording.
//! * Providers cap the rule list ([`S3_MAX_LIFECYCLE_RULES`],
//!   [`GCS_MAX_LIFECYCLE_RULES`]). At the cap the merge evicts JamStream's
//!   own oldest rule to make room, which is almost always a rule whose
//!   objects the provider already deleted, and never a rule of the host's. A
//!   bucket filled to the cap with the host's own rules gets
//!   [`RetentionEnforcement::Manual`] and a note saying so.
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
use crate::providers::aws::{take_tag, xml_unescape, xml_value};
use crate::types::ProviderKind;

/// Id prefix every lifecycle rule JamStream writes carries, so a host can see
/// at a glance which rules are ours and so the merge knows which rules it may
/// evict.
pub const RULE_ID: &str = "jamstream-recording-retention";

/// S3 and Spaces cap a bucket's lifecycle configuration at 1000 rules.
pub const S3_MAX_LIFECYCLE_RULES: usize = 1000;

/// Cloud Storage caps a bucket's lifecycle configuration at 100 rules.
pub const GCS_MAX_LIFECYCLE_RULES: usize = 100;

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

/// The id of JamStream's rule for one key prefix. Per session, because the
/// retention choice is: `jamstream-recording-retention-<session>`, and plain
/// [`RULE_ID`] for the recordings prefix as a whole.
pub fn rule_id(prefix: &str) -> String {
    let tail = prefix
        .strip_prefix(crate::storage::RECORDING_PREFIX)
        .unwrap_or(prefix)
        .trim_matches('/');
    if tail.is_empty() {
        return RULE_ID.to_owned();
    }
    format!("{RULE_ID}-{}", crate::storage::sanitize_component(tail))
}

/// True for a rule id JamStream wrote, which is the only kind the merge may
/// drop.
fn is_ours(id: Option<&str>) -> bool {
    id.is_some_and(|id| id == RULE_ID || id.starts_with(&format!("{RULE_ID}-")))
}

/// One `<Rule>` element, scoped to `prefix`, carrying an expiration when the
/// choice has one and always carrying the incomplete-multipart cleanup.
fn s3_rule(prefix: &str, retention: Retention, dialect: LifecycleDialect) -> String {
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
        "<Rule>\
<ID>{id}</ID>\
{filter}\
<Status>Enabled</Status>\
{expiration}\
<AbortIncompleteMultipartUpload><DaysAfterInitiation>{ABORT_INCOMPLETE_DAYS}</DaysAfterInitiation></AbortIncompleteMultipartUpload>\
</Rule>",
        id = xml_escape(&rule_id(prefix))
    )
}

/// The lifecycle document for a bucket that has none: JamStream's rule for
/// `prefix` and nothing else.
pub fn s3_lifecycle_xml(prefix: &str, retention: Retention, dialect: LifecycleDialect) -> String {
    merge_s3_lifecycle("", prefix, retention, dialect, S3_MAX_LIFECYCLE_RULES)
        .expect("one rule always fits in an empty document")
}

/// The document for `PUT /{bucket}?lifecycle`: JamStream's rule for `prefix`
/// merged into `existing`, which is whatever `GET /{bucket}?lifecycle`
/// returned (empty when the bucket has no configuration).
///
/// Every other rule survives verbatim, JamStream's rules for other sessions
/// included, because the call replaces the bucket's whole configuration. None
/// when `max_rules` is already full of rules that are not ours; see the module
/// docs.
pub fn merge_s3_lifecycle(
    existing: &str,
    prefix: &str,
    retention: Retention,
    dialect: LifecycleDialect,
    max_rules: usize,
) -> Option<String> {
    let id = rule_id(prefix);
    let mut kept: Vec<&str> = Vec::new();
    let mut rest = existing;
    while let Some((rule, after)) = take_tag(rest, "Rule") {
        rest = after;
        let rule_id_value = xml_value(rule, "ID").map(xml_unescape);
        // This session's rule is the one being replaced. The bare id is
        // matched by prefix too, because that is what a JamStream older than
        // per-session ids wrote for this same prefix.
        let replaced = rule_id_value.as_deref() == Some(id.as_str())
            || (is_ours(rule_id_value.as_deref())
                && xml_value(rule, "Prefix").map(xml_unescape).as_deref() == Some(prefix));
        if !replaced {
            kept.push(rule);
        }
    }
    // Room for ours. Only ever at the expense of one of ours, oldest first:
    // by the time a bucket holds a thousand rules, the objects the earliest
    // ones named were expired by those same rules long ago.
    while kept.len() >= max_rules {
        // No rule of ours left to drop means no room, and a rule of the host's
        // is not ours to take: None, and the caller says so.
        let index = kept
            .iter()
            .position(|rule| is_ours(xml_value(rule, "ID").map(xml_unescape).as_deref()))?;
        let dropped = kept.remove(index);
        tracing::warn!(
            rule = xml_value(dropped, "ID").unwrap_or("<no id>"),
            max_rules,
            "the bucket is at its lifecycle rule limit; dropping JamStream's oldest \
             expiry rule to make room for this session's"
        );
    }
    let mut document =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><LifecycleConfiguration>");
    for rule in kept {
        document.push_str("<Rule>");
        document.push_str(rule);
        document.push_str("</Rule>");
    }
    document.push_str(&s3_rule(prefix, retention, dialect));
    document.push_str("</LifecycleConfiguration>");
    Some(document)
}

/// The bucket patch body for `PATCH /storage/v1/b/{bucket}` for a bucket with
/// no lifecycle rules yet.
pub fn gcs_lifecycle_patch(prefix: &str, retention: Retention) -> Value {
    merge_gcs_lifecycle(&Value::Null, prefix, retention, GCS_MAX_LIFECYCLE_RULES)
        .expect("one rule always fits in an empty rule list")
}

/// The patch body for `PATCH /storage/v1/b/{bucket}`: JamStream's rule for
/// `prefix` merged into `bucket`, the JSON `GET /storage/v1/b/{bucket}`
/// returned.
///
/// GCS lifecycle is a whole-bucket field, so the patch replaces the rule list
/// and everything not this prefix's rule has to be carried across. GCS rules
/// have no id, so ours are recognized by their `matchesPrefix`.
///
/// "Keep forever" contributes no rule at all: GCS has no
/// incomplete-multipart lifecycle action (it collects abandoned resumable
/// sessions itself after a week), so there is nothing left to say once the
/// expiration is gone.
pub fn merge_gcs_lifecycle(
    bucket: &Value,
    prefix: &str,
    retention: Retention,
    max_rules: usize,
) -> Option<Value> {
    let existing = bucket
        .get("lifecycle")
        .and_then(|l| l.get("rule"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut kept: Vec<Value> = existing
        .iter()
        .filter(|rule| !gcs_rule_matches_prefix(rule, prefix))
        .cloned()
        .collect();
    let mine = retention.days().map(|days| {
        json!({
            "action": { "type": "Delete" },
            "condition": { "age": days, "matchesPrefix": [prefix] },
        })
    });
    if mine.is_some() {
        while kept.len() >= max_rules {
            // No rule of ours left to drop means no room, and a rule of the
            // host's is not ours to take: None, and the caller says so.
            let index = kept.iter().position(gcs_rule_is_ours)?;
            kept.remove(index);
            tracing::warn!(
                max_rules,
                "the bucket is at its lifecycle rule limit; dropping JamStream's oldest \
                 expiry rule to make room for this session's"
            );
        }
    }
    kept.extend(mine);
    Some(json!({ "lifecycle": { "rule": kept } }))
}

/// True for a GCS rule that is JamStream's rule for exactly `prefix`.
fn gcs_rule_matches_prefix(rule: &Value, prefix: &str) -> bool {
    gcs_rule_is_ours(rule)
        && rule["condition"]["matchesPrefix"]
            .as_array()
            .is_some_and(|prefixes| prefixes.iter().all(|p| p.as_str() == Some(prefix)))
}

/// True for a GCS rule JamStream wrote: a Delete keyed on prefixes that are
/// all inside the recordings prefix. Nothing else may be evicted or replaced.
fn gcs_rule_is_ours(rule: &Value) -> bool {
    let prefixes = rule["condition"]["matchesPrefix"].as_array();
    rule["action"]["type"].as_str() == Some("Delete")
        && prefixes.is_some_and(|prefixes| {
            !prefixes.is_empty()
                && prefixes.iter().all(|p| {
                    p.as_str()
                        .is_some_and(|p| p.starts_with(crate::storage::RECORDING_PREFIX))
                })
        })
}

/// What actually happened when a retention choice was applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionEnforcement {
    /// The provider now enforces the choice itself.
    ServerSide {
        provider: ProviderKind,
        retention: Retention,
        /// Id of the rule carrying this session's choice, so a host can find
        /// it among their own.
        rule_id: String,
        /// The whole document that was accepted: this session's rule plus
        /// every rule the merge preserved. For display and support requests.
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
                rule_id,
                ..
            } => match r.days() {
                Some(days) => format!(
                    "{provider} will delete this recording {days} days from now (lifecycle rule {rule_id})"
                ),
                None => format!(
                    "kept until you delete it; no expiration rule set on {provider} (rule {rule_id} only cleans up failed uploads)"
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

/// The note to show when the bucket's existing rules could not be read, so
/// nothing was written.
///
/// `permission` is the provider's name for the missing one. Writing without
/// reading would replace the host's own lifecycle rules with ours, which is
/// data loss on a bucket JamStream does not own, so this path writes nothing.
pub fn unreadable_note(retention: Retention, permission: &str) -> String {
    let consequence = match retention.days() {
        Some(days) => format!(
            "\"{}\" was not applied and nothing will delete the recording after {days} days \
             unless you do",
            retention.label()
        ),
        None => "no cleanup rule for failed uploads was applied".to_owned(),
    };
    format!(
        "JamStream could not read this bucket's lifecycle rules ({permission} was refused). \
         Setting a rule replaces the whole list, so writing one without reading first would \
         delete any rule you set yourself: {consequence}. Grant {permission} on the bucket and \
         arm recording again."
    )
}

/// The note to show when the bucket is at the provider's lifecycle rule limit
/// and every rule there belongs to the host.
pub fn at_capacity_note(retention: Retention, max_rules: usize) -> String {
    let consequence = match retention.days() {
        Some(days) => format!(
            "so \"{}\" is not enforced: nothing will delete the recording after {days} days \
             unless you do",
            retention.label()
        ),
        None => "so failed uploads will not be cleaned up for you".to_owned(),
    };
    format!(
        "This bucket already has {max_rules} lifecycle rules, which is the provider's limit, and \
         none of them are JamStream's, {consequence}. Remove a rule you no longer need, or point \
         recording at another bucket."
    )
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
            rule_id: rule_id("jamstream/recordings/s1/"),
            rule: "<xml/>".to_owned(),
        };
        assert!(server.is_server_side());
        assert!(
            server
                .describe()
                .contains("aws will delete this recording 30 days")
        );
        // The id names the session, so a host with several can tell the rules
        // apart in their own console.
        assert!(
            server
                .describe()
                .contains("jamstream-recording-retention-s1"),
            "{}",
            server.describe()
        );

        let forever = RetentionEnforcement::ServerSide {
            provider: ProviderKind::Gcp,
            retention: Retention::KeepForever,
            rule_id: RULE_ID.to_owned(),
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

    // ---- Merging into a document that is already there ----

    #[test]
    fn a_rule_id_names_the_session_and_the_whole_prefix_keeps_the_bare_id() {
        assert_eq!(
            rule_id("jamstream/recordings/deadbeef/"),
            "jamstream-recording-retention-deadbeef"
        );
        assert_eq!(rule_id("jamstream/recordings/"), RULE_ID);
        assert_eq!(rule_id(""), RULE_ID);
        // Two sessions, two ids: the bug this fixes was one id for all of them.
        assert_ne!(
            rule_id("jamstream/recordings/s1/"),
            rule_id("jamstream/recordings/s2/")
        );
        // A prefix outside the recordings tree still gets a usable id, and
        // nothing in an id can break the XML around it.
        assert_eq!(
            rule_id("other/place/"),
            "jamstream-recording-retention-other-place"
        );
        assert!(!rule_id("<>&/").contains('<'));
    }

    /// The document a JamStream one version older wrote for session s1, plus a
    /// rule of the host's own.
    fn existing_document() -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><LifecycleConfiguration>\
<Rule><ID>archive-my-masters</ID><Filter><Prefix>masters/</Prefix></Filter>\
<Status>Enabled</Status><Transition><Days>30</Days><StorageClass>GLACIER</StorageClass></Transition></Rule>\
{}</LifecycleConfiguration>",
            s3_rule(
                "jamstream/recordings/s1/",
                Retention::Days90,
                LifecycleDialect::S3v2
            )
        )
    }

    #[test]
    fn a_second_session_leaves_the_first_sessions_rule_alone() {
        let merged = merge_s3_lifecycle(
            &existing_document(),
            "jamstream/recordings/s2/",
            Retention::Days7,
            LifecycleDialect::S3v2,
            S3_MAX_LIFECYCLE_RULES,
        )
        .expect("two rules fit");
        // The defect: session two's document replaced session one's rule, and
        // session one's takes then lived forever.
        assert!(
            merged.contains("<ID>jamstream-recording-retention-s1</ID>"),
            "{merged}"
        );
        assert!(merged.contains("<Prefix>jamstream/recordings/s1/</Prefix>"));
        assert!(merged.contains("<Expiration><Days>90</Days></Expiration>"));
        // And the host's own rule, which is not ours to touch either.
        assert!(merged.contains("<ID>archive-my-masters</ID>"), "{merged}");
        assert!(merged.contains("<StorageClass>GLACIER</StorageClass>"));
        // Session two's own rule is there once.
        assert_eq!(
            merged
                .matches("<ID>jamstream-recording-retention-s2</ID>")
                .count(),
            1
        );
        assert!(merged.contains("<Expiration><Days>7</Days></Expiration>"));
        assert_eq!(merged.matches("<Rule>").count(), 3);
    }

    #[test]
    fn re_applying_a_session_replaces_its_own_rule_rather_than_stacking() {
        let merged = merge_s3_lifecycle(
            &existing_document(),
            "jamstream/recordings/s1/",
            Retention::Days7,
            LifecycleDialect::S3v2,
            S3_MAX_LIFECYCLE_RULES,
        )
        .expect("one replacement fits");
        assert_eq!(merged.matches("<Rule>").count(), 2, "{merged}");
        assert_eq!(
            merged
                .matches("<ID>jamstream-recording-retention-s1</ID>")
                .count(),
            1
        );
        assert!(merged.contains("<Expiration><Days>7</Days></Expiration>"));
        assert!(
            !merged.contains("<Days>90</Days>"),
            "the old choice survived: {merged}"
        );
    }

    #[test]
    fn the_shared_rule_an_older_jamstream_wrote_for_this_prefix_is_replaced() {
        // Before per-session ids there was one rule id for every session, and
        // it named whichever prefix was recorded last. Leaving it in place
        // would keep expiring this session's takes on the old schedule.
        let legacy = format!(
            "<LifecycleConfiguration><Rule><ID>{RULE_ID}</ID>\
<Filter><Prefix>jamstream/recordings/s1/</Prefix></Filter><Status>Enabled</Status>\
<Expiration><Days>7</Days></Expiration></Rule></LifecycleConfiguration>"
        );
        let merged = merge_s3_lifecycle(
            &legacy,
            "jamstream/recordings/s1/",
            Retention::Days90,
            LifecycleDialect::S3v2,
            S3_MAX_LIFECYCLE_RULES,
        )
        .unwrap();
        assert_eq!(merged.matches("<Rule>").count(), 1, "{merged}");
        assert!(merged.contains("<Days>90</Days>"));
        assert!(!merged.contains("<Days>7</Days>"), "{merged}");
        // The same legacy rule scoped to another session's prefix is that
        // session's choice and stays.
        let merged = merge_s3_lifecycle(
            &legacy,
            "jamstream/recordings/s2/",
            Retention::Days90,
            LifecycleDialect::S3v2,
            S3_MAX_LIFECYCLE_RULES,
        )
        .unwrap();
        assert_eq!(merged.matches("<Rule>").count(), 2, "{merged}");
        assert!(merged.contains("<Days>7</Days>"));
    }

    #[test]
    fn an_empty_or_junk_document_still_yields_our_rule() {
        for existing in ["", "<LifecycleConfiguration/>", "not xml at all", "<Rule>"] {
            let merged = merge_s3_lifecycle(
                existing,
                "jamstream/recordings/s1/",
                Retention::Days30,
                LifecycleDialect::S3v2,
                S3_MAX_LIFECYCLE_RULES,
            )
            .unwrap_or_else(|| panic!("{existing:?} produced no document"));
            assert_eq!(merged.matches("<Rule>").count(), 1, "{merged}");
            assert!(merged.contains("<ID>jamstream-recording-retention-s1</ID>"));
        }
    }

    #[test]
    fn at_the_rule_cap_only_our_own_rules_are_evicted() {
        let ours = |n: usize| {
            s3_rule(
                &format!("jamstream/recordings/s{n}/"),
                Retention::Days7,
                LifecycleDialect::S3v2,
            )
        };
        // A bucket at the cap, all of them ours: the oldest goes.
        let full: String = (0..S3_MAX_LIFECYCLE_RULES).map(ours).collect();
        let merged = merge_s3_lifecycle(
            &full,
            "jamstream/recordings/new/",
            Retention::Days30,
            LifecycleDialect::S3v2,
            S3_MAX_LIFECYCLE_RULES,
        )
        .expect("room is made by dropping ours");
        assert_eq!(merged.matches("<Rule>").count(), S3_MAX_LIFECYCLE_RULES);
        assert!(merged.contains("<ID>jamstream-recording-retention-new</ID>"));
        assert!(
            !merged.contains("<ID>jamstream-recording-retention-s0</ID>"),
            "the oldest of ours should have been the one dropped"
        );
        assert!(merged.contains("<ID>jamstream-recording-retention-s1</ID>"));

        // A bucket at the cap with nothing of ours: no rule of the host's is
        // ever touched, so there is no rule to write and the caller has to say
        // so.
        let theirs: String = (0..S3_MAX_LIFECYCLE_RULES)
            .map(|n| format!("<Rule><ID>host-rule-{n}</ID><Status>Enabled</Status></Rule>"))
            .collect();
        assert!(
            merge_s3_lifecycle(
                &theirs,
                "jamstream/recordings/new/",
                Retention::Days30,
                LifecycleDialect::S3v2,
                S3_MAX_LIFECYCLE_RULES,
            )
            .is_none()
        );
        let note = at_capacity_note(Retention::Days30, S3_MAX_LIFECYCLE_RULES);
        assert!(note.contains("1000 lifecycle rules"), "{note}");
        assert!(note.contains("nothing will delete the recording"), "{note}");
    }

    #[test]
    fn the_unreadable_note_says_nothing_was_written_and_why() {
        let note = unreadable_note(Retention::Days7, "s3:GetLifecycleConfiguration");
        assert!(note.contains("s3:GetLifecycleConfiguration"), "{note}");
        assert!(note.contains("was not applied"), "{note}");
        assert!(note.contains("7 days unless you do"), "{note}");
        // It must not claim a rule exists, and it must say why writing blind
        // is not an option.
        assert!(note.contains("delete any rule you set yourself"), "{note}");
    }

    #[test]
    fn gcs_merge_keeps_other_sessions_and_the_hosts_own_rules() {
        let bucket = json!({
            "lifecycle": { "rule": [
                // The host's own: not a Delete, and outside our prefix.
                { "action": { "type": "SetStorageClass", "storageClass": "NEARLINE" },
                  "condition": { "age": 10, "matchesPrefix": ["masters/"] } },
                { "action": { "type": "Delete" },
                  "condition": { "age": 400, "matchesPrefix": ["scratch/"] } },
                // Session one's.
                { "action": { "type": "Delete" },
                  "condition": { "age": 90, "matchesPrefix": ["jamstream/recordings/s1/"] } },
            ] }
        });
        let patch = merge_gcs_lifecycle(
            &bucket,
            "jamstream/recordings/s2/",
            Retention::Days7,
            GCS_MAX_LIFECYCLE_RULES,
        )
        .unwrap();
        let rules = patch["lifecycle"]["rule"].as_array().unwrap();
        assert_eq!(rules.len(), 4, "{patch}");
        assert_eq!(rules[0]["action"]["type"], "SetStorageClass");
        assert_eq!(rules[1]["condition"]["matchesPrefix"][0], "scratch/");
        assert_eq!(rules[2]["condition"]["age"], 90);
        assert_eq!(rules[3]["condition"]["age"], 7);
        assert_eq!(
            rules[3]["condition"]["matchesPrefix"][0],
            "jamstream/recordings/s2/"
        );

        // Re-applying replaces that session's rule and nothing else.
        let patch = merge_gcs_lifecycle(
            &bucket,
            "jamstream/recordings/s1/",
            Retention::Days30,
            GCS_MAX_LIFECYCLE_RULES,
        )
        .unwrap();
        let rules = patch["lifecycle"]["rule"].as_array().unwrap();
        assert_eq!(rules.len(), 3, "{patch}");
        assert_eq!(rules[2]["condition"]["age"], 30);

        // Keep forever drops this session's rule and leaves the rest, where
        // the old code sent an empty list and cleared the whole bucket.
        let patch = merge_gcs_lifecycle(
            &bucket,
            "jamstream/recordings/s1/",
            Retention::KeepForever,
            GCS_MAX_LIFECYCLE_RULES,
        )
        .unwrap();
        let rules = patch["lifecycle"]["rule"].as_array().unwrap();
        assert_eq!(rules.len(), 2, "{patch}");
        assert!(
            !patch.to_string().contains("jamstream/recordings/s1/"),
            "{patch}"
        );
    }

    #[test]
    fn gcs_merge_evicts_only_our_rules_at_the_cap() {
        let ours = |n: usize| {
            json!({ "action": { "type": "Delete" },
                    "condition": { "age": 7, "matchesPrefix": [format!("jamstream/recordings/s{n}/")] } })
        };
        let bucket = json!({ "lifecycle": { "rule": (0..GCS_MAX_LIFECYCLE_RULES).map(ours).collect::<Vec<_>>() } });
        let patch = merge_gcs_lifecycle(
            &bucket,
            "jamstream/recordings/new/",
            Retention::Days30,
            GCS_MAX_LIFECYCLE_RULES,
        )
        .unwrap();
        let rules = patch["lifecycle"]["rule"].as_array().unwrap();
        assert_eq!(rules.len(), GCS_MAX_LIFECYCLE_RULES);
        assert_eq!(
            rules[0]["condition"]["matchesPrefix"][0], "jamstream/recordings/s1/",
            "the oldest of ours should have been the one dropped"
        );

        let theirs = json!({ "lifecycle": { "rule": (0..GCS_MAX_LIFECYCLE_RULES)
            .map(|n| json!({ "action": { "type": "Delete" },
                             "condition": { "age": 1, "matchesPrefix": [format!("host/{n}/")] } }))
            .collect::<Vec<_>>() } });
        assert!(
            merge_gcs_lifecycle(
                &theirs,
                "jamstream/recordings/new/",
                Retention::Days30,
                GCS_MAX_LIFECYCLE_RULES,
            )
            .is_none()
        );
        // Keep forever adds no rule, so a full bucket is no obstacle to it.
        assert!(
            merge_gcs_lifecycle(
                &theirs,
                "jamstream/recordings/new/",
                Retention::KeepForever,
                GCS_MAX_LIFECYCLE_RULES,
            )
            .is_some()
        );
    }

    #[test]
    fn gcs_merge_tolerates_a_bucket_with_no_lifecycle_field() {
        for bucket in [json!({}), Value::Null, json!({ "lifecycle": {} })] {
            let patch = merge_gcs_lifecycle(
                &bucket,
                "jamstream/recordings/s1/",
                Retention::Days30,
                GCS_MAX_LIFECYCLE_RULES,
            )
            .unwrap();
            assert_eq!(patch["lifecycle"]["rule"].as_array().unwrap().len(), 1);
        }
    }
}
