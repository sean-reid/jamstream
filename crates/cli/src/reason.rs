//! One sentence per refusal, for every surface that shows one.
//!
//! A bucket or an API that says no answers with its own document. For S3
//! that is up to four kilobytes of XML naming the AWS account number, the
//! IAM ARN, a RequestId and a HostId; none of it helps whoever is looking at
//! the screen, and the screen showing it is the one most likely to be
//! screenshotted, because it is where something went wrong. So the raw error
//! goes to the log, where diagnosis happens, and the surface gets a sentence
//! with the remedy in it.
//!
//! Only the error's class and its code are ever read, never the body, so
//! nothing in a body can leak: extraction is not a filter someone has to
//! maintain.

use jamstream_cloud::{AWS_QUOTE, ProviderError, ProviderKind, http};

use crate::CliError;

/// What the call was doing when the provider refused, which is what decides
/// the remedy: a 403 listing takes and a 403 launching a machine are the
/// same status, a different key, and different acts to fix them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    /// Listing or downloading a session's takes.
    Takes,
    /// Proving a storage key can write the recordings prefix.
    Probe,
    /// The provider's machine API: pricing, listing instances, the
    /// preflight, and the launch itself.
    Machines,
}

impl Attempt {
    /// What this surface calls trying again, so a remedy ends on the control
    /// the reader is looking at.
    fn again(self) -> &'static str {
        match self {
            Attempt::Takes => "refresh",
            Attempt::Probe => "check again",
            Attempt::Machines => "try again",
        }
    }

    /// What refused, as the reader thinks of it.
    fn subject(self) -> &'static str {
        match self {
            Attempt::Takes | Attempt::Probe => "The bucket",
            Attempt::Machines => "The provider",
        }
    }
}

/// One sentence for the surface; the whole error for the log.
///
/// `provider` is the provider that refused, because the remedy for a refusal
/// is a different act on each of them; a provider this build does not know
/// gets the remedy that holds everywhere.
pub fn error_sentence(
    doing: &str,
    attempt: Attempt,
    provider: Option<ProviderKind>,
    err: &CliError,
) -> String {
    tracing::warn!("{doing}: {err}");
    match err {
        CliError::Provider(p) => provider_sentence(attempt, provider, p),
        // Everything else is already in our own words: the keychain's
        // pointer at the Recording tab, the traversal guard, the size check.
        other => other.to_string(),
    }
}

/// The sentence for each way a provider says no.
pub fn provider_sentence(
    attempt: Attempt,
    provider: Option<ProviderKind>,
    err: &ProviderError,
) -> String {
    let code = http::error_code(err);
    match err {
        ProviderError::Auth(m) => match code.as_deref() {
            Some("ExpiredToken" | "TokenRefreshRequired" | "InvalidToken") => expired(attempt),
            Some("InvalidAccessKeyId" | "SignatureDoesNotMatch") => rejected(attempt),
            Some(_) => denied(attempt, provider),
            None => {
                our_words(attempt, provider, err, m).unwrap_or_else(|| denied(attempt, provider))
            }
        },
        ProviderError::NotFound(_) => match (attempt, code.as_deref()) {
            (Attempt::Takes, Some("NoSuchKey")) => {
                "That take is no longer in the bucket. Refresh the list.".to_owned()
            }
            (Attempt::Takes, _) => "The bucket was not found. It may have been deleted, or it \
                  may live in a different region than this session recorded."
                .to_owned(),
            (Attempt::Probe, _) => {
                "The bucket was not found. Check its name and its region, then check again."
                    .to_owned()
            }
            (Attempt::Machines, _) => refused(attempt, code.as_deref()),
        },
        ProviderError::RateLimited { .. } => format!(
            "The provider is rate limiting requests. Wait a minute, then {}.",
            attempt.again()
        ),
        ProviderError::Transient(_) => format!(
            "{} could not be reached. Check your connection and {}.",
            attempt.subject(),
            attempt.again()
        ),
        // An account limit is not a failure of the key, and the way out is
        // the provider's console or a different region.
        ProviderError::QuotaExceeded(_) if attempt == Attempt::Machines => match code {
            Some(code) => format!(
                "The account is at a provider limit ({code}). Raise it with the provider, \
                 or try another region."
            ),
            None => "The account is at a provider limit. Raise it with the provider, or try \
                     another region."
                .to_owned(),
        },
        // A quota refusal cannot come off the storage path, but the match
        // has to hold if one ever does: same rule as any other unclassified
        // failure.
        ProviderError::QuotaExceeded(m) | ProviderError::Other(m) => match code.as_deref() {
            // A body is the provider's, not ours to draw.
            Some(_) => refused(attempt, code.as_deref()),
            None => our_words(attempt, provider, err, m)
                .unwrap_or_else(|| refused(attempt, code.as_deref())),
        },
    }
}

/// A refusal whose class says no more than that it was refused. The
/// provider's own code is named when there is one, because that is the word
/// to search for; the body it came out of is not, because that is the
/// document with the account number in it.
fn refused(attempt: Attempt, code: Option<&str>) -> String {
    match code {
        Some(code) => format!("{} refused the request ({code}).", attempt.subject()),
        None => format!("{} refused the request.", attempt.subject()),
    }
}

/// The message itself, when it is one of ours rather than something a
/// provider said.
///
/// A message the http layer classified carries the response body, and a body
/// never reaches a surface. What is left is ours, with one exception: the
/// AWS provider rewrites EC2 error bodies into "{code}: {message}" without
/// the http prefix, so on the machine API an AWS message that looks like
/// ours can be EC2's. The one sentence of ours that survives that rewrite
/// names the missing IAM action and then quotes EC2, marked; see
/// [`AWS_QUOTE`].
fn our_words(
    attempt: Attempt,
    provider: Option<ProviderKind>,
    err: &ProviderError,
    message: &str,
) -> Option<String> {
    if let Some((head, _)) = message.split_once(AWS_QUOTE) {
        return Some(head.trim_end().to_owned());
    }
    if http::error_body(err).is_some() {
        return None;
    }
    let rewritten = attempt == Attempt::Machines && provider == Some(ProviderKind::Aws);
    (!rewritten).then(|| message.to_owned())
}

/// A credential the provider says is past its date.
fn expired(attempt: Attempt) -> String {
    match attempt {
        Attempt::Takes => "The storage key has expired. Save a fresh key in the \
                           Recording tab, then refresh."
            .to_owned(),
        Attempt::Probe => {
            "The storage key has expired. Paste a fresh key, then check again.".to_owned()
        }
        Attempt::Machines => {
            "The credentials have expired. Paste a fresh key, then try again.".to_owned()
        }
    }
}

/// A credential the provider does not recognise, which is a typo far more
/// often than it is a permission.
fn rejected(attempt: Attempt) -> String {
    match attempt {
        Attempt::Takes => "The bucket did not accept the storage key. Check the key \
                           and its secret in the Recording tab."
            .to_owned(),
        Attempt::Probe => {
            "The bucket did not accept the storage key. Check the key and its secret.".to_owned()
        }
        Attempt::Machines => {
            "The provider did not accept these credentials. Check them and try again.".to_owned()
        }
    }
}

/// A 403, with the remedy the provider that refused actually has.
///
/// This used to name `s3:ListBucket` to everyone, which is a policy only AWS
/// has: a Spaces key carries no policy at all, and a GCS bucket grants a role
/// to the service account behind the HMAC key. The published screenshot of
/// that row was a DigitalOcean session being told to edit an S3 policy. Each
/// remedy is the step that provider's own setup guide ends on.
fn denied(attempt: Attempt, provider: Option<ProviderKind>) -> String {
    let remedy = match (attempt, provider) {
        (Attempt::Takes, Some(ProviderKind::Aws)) => {
            "Add s3:ListBucket and s3:GetObject for the bucket to the key's policy"
        }
        (Attempt::Probe, Some(ProviderKind::Aws)) => {
            "Add s3:PutObject, s3:DeleteObject and s3:PutLifecycleConfiguration for the \
             bucket to the key's policy"
        }
        (Attempt::Takes | Attempt::Probe, Some(ProviderKind::DigitalOcean)) => {
            "Give the Spaces key full access to this bucket, under Spaces Object Storage, \
             Access Keys"
        }
        (Attempt::Takes | Attempt::Probe, Some(ProviderKind::Gcp)) => {
            "Grant the key's service account the Storage Admin role on the bucket"
        }
        // Local records to a folder and cannot 403, and a name that does not
        // parse has no console to send anyone to.
        (Attempt::Takes, Some(ProviderKind::Local) | None) => {
            "Give the key permission to list the bucket and read what is in it"
        }
        (Attempt::Probe, Some(ProviderKind::Local) | None) => {
            "Give the key permission to write the recordings prefix and set the bucket's \
             lifecycle rule"
        }
        (Attempt::Machines, Some(ProviderKind::Aws)) => {
            "Attach the jamstream-host policy from the provider guide to this key's IAM user"
        }
        (Attempt::Machines, Some(ProviderKind::DigitalOcean)) => {
            "Give the token the droplet, tag and firewall scopes the provider guide lists"
        }
        (Attempt::Machines, Some(ProviderKind::Gcp)) => {
            "Grant the service account the Compute Instance Admin (v1) role"
        }
        (Attempt::Machines, Some(ProviderKind::Local) | None) => {
            "Give the credentials permission to launch, list and delete machines"
        }
    };
    let refusal = match attempt {
        Attempt::Takes => "The storage key cannot list this bucket.",
        Attempt::Probe => "The storage key cannot write to this bucket.",
        Attempt::Machines => "The provider refused these credentials.",
    };
    format!("{refusal} {remedy}, then {}.", attempt.again())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use jamstream_cloud::Provider;
    use jamstream_cloud::providers::aws::AwsProvider;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// A real S3 403, captured whole: the account number, the IAM ARN, the
    /// RequestId and the HostId are the four identifiers a screenshot of
    /// this hands out.
    const S3_DENIED: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
        <Error><Code>AccessDenied</Code><Message>User: \
        arn:aws:iam::887762372032:user/jamstream-recordings is not authorized to \
        perform: s3:ListBucket on resource: \
        \"arn:aws:s3:::our-takes\"</Message>\
        <RequestId>Q0YMR4GFKCH1Y688</RequestId>\
        <HostId>EE3WMENDEauoc0QS4v1XCZK1RcDA4A/kbNvyiXfCbZDbAM3rq3zXBP\
        bfLYNvpk2rAOFP8prkVw=</HostId></Error>";

    /// The identifiers in [`S3_DENIED`] that must never reach a surface.
    const IDENTIFIERS: [&str; 4] = [
        "887762372032",
        "arn:aws:iam",
        "Q0YMR4GFKCH1Y688",
        "EE3WMEND",
    ];

    /// Runs `f` with warnings captured, returning what it made and what it
    /// logged, so a test can hold the screen and the log side by side.
    fn with_captured_log(f: impl FnOnce() -> String) -> (String, String) {
        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("log sink").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer({
                let sink = sink.clone();
                move || sink.clone()
            })
            .finish();
        let shown = tracing::subscriber::with_default(subscriber, f);
        let logged = String::from_utf8(sink.0.lock().expect("log sink").clone()).expect("utf8");
        (shown, logged)
    }

    /// The point of the mapping, on every surface that draws one of these:
    /// the screen gets the remedy, the log gets the document, and the
    /// identifiers people crop out of screenshots appear in exactly one of
    /// the two.
    #[test]
    fn a_real_denial_reads_as_a_remedy_and_logs_whole() {
        let err = CliError::Provider(ProviderError::Auth(format!(
            "http 403 Forbidden: {S3_DENIED}"
        )));
        for attempt in [Attempt::Takes, Attempt::Probe, Attempt::Machines] {
            let (shown, logged) = with_captured_log(|| {
                error_sentence("listing a3f29c41", attempt, Some(ProviderKind::Aws), &err)
            });
            assert!(shown.contains("storage key") || shown.contains("credentials"));
            for identifier in IDENTIFIERS {
                assert!(!shown.contains(identifier), "{identifier} leaked: {shown}");
            }
            // Nor any of the document around them.
            for shape in ["<", "http", "Error>"] {
                assert!(!shown.contains(shape), "{shape} leaked: {shown}");
            }
            for kept in IDENTIFIERS {
                assert!(logged.contains(kept), "{kept} lost from the log: {logged}");
            }
            assert!(logged.contains("AccessDenied"), "{logged}");
        }
        // And the take path still names the exact policy line to add.
        let shown = provider_sentence(
            Attempt::Takes,
            Some(ProviderKind::Aws),
            &ProviderError::Auth(format!("http 403 Forbidden: {S3_DENIED}")),
        );
        assert!(shown.contains("s3:ListBucket"), "{shown}");
    }

    /// Each failure class reads as a sentence naming what to do, and our own
    /// messages pass through as themselves.
    #[test]
    fn every_failure_class_reads_as_a_sentence() {
        let auth = |code: &str| {
            ProviderError::Auth(format!(
                "http 403 Forbidden: <Error><Code>{code}</Code>\
                 <Message>x</Message></Error>"
            ))
        };
        let aws = Some(ProviderKind::Aws);
        let takes = |err: &ProviderError| provider_sentence(Attempt::Takes, aws, err);
        assert!(takes(&auth("ExpiredToken")).contains("expired"));
        assert!(takes(&auth("SignatureDoesNotMatch")).contains("did not accept"));
        assert!(takes(&auth("AccessDenied")).contains("s3:ListBucket"));

        let missing = ProviderError::NotFound(
            "http 404 Not Found: <Error><Code>NoSuchBucket</Code></Error>".to_owned(),
        );
        assert!(takes(&missing).contains("bucket was not found"));
        let gone = ProviderError::NotFound(
            "http 404 Not Found: <Error><Code>NoSuchKey</Code></Error>".to_owned(),
        );
        assert!(takes(&gone).contains("no longer in the bucket"));

        let offline = ProviderError::Transient("error sending request: connect error".to_owned());
        assert!(takes(&offline).contains("connection"));
        assert!(takes(&ProviderError::RateLimited { retry_after: None }).contains("rate limiting"));

        // Our own words survive: this is the size check's sentence, not a
        // provider body.
        let truncated = ProviderError::Other(
            "download of mix.flac is truncated: content-length promised 9 bytes, 4 arrived"
                .to_owned(),
        );
        assert_eq!(
            takes(&truncated),
            "download of mix.flac is truncated: content-length promised 9 bytes, 4 arrived"
        );
        // A body on an unclassified status is a provider's, so only its code
        // is shown.
        let refused = ProviderError::Other(
            "http 400 Bad Request: <Error><Code>InvalidRequest</Code>\
             <Message>secret detail</Message></Error>"
                .to_owned(),
        );
        assert_eq!(
            takes(&refused),
            "The bucket refused the request (InvalidRequest)."
        );
    }

    /// A 403 sends each provider to its own console, because the act that
    /// fixes it is different on each: only AWS has a policy with
    /// `s3:ListBucket` in it, and telling a Spaces or GCS host to edit one
    /// sends them looking for a screen their provider does not have.
    #[test]
    fn the_denied_remedy_is_the_one_this_provider_actually_has() {
        let denied = CliError::Provider(ProviderError::Auth(
            "http 403 Forbidden: <Error><Code>AccessDenied</Code></Error>".to_owned(),
        ));
        let sentence = |name: &str| {
            error_sentence("listing", Attempt::Takes, name.parse().ok(), &denied).to_lowercase()
        };
        let aws = sentence("aws");
        assert!(aws.contains("s3:listbucket"), "{aws}");

        let spaces = sentence("digitalocean");
        assert!(spaces.contains("spaces key"), "{spaces}");
        assert!(!spaces.contains("s3:"), "{spaces}");
        assert!(!spaces.contains("policy"), "{spaces}");

        let gcs = sentence("gcp");
        assert!(gcs.contains("service account"), "{gcs}");
        assert!(!gcs.contains("s3:"), "{gcs}");

        // A name from a record this build does not know still gets an act.
        let unknown = sentence("azure");
        assert!(unknown.contains("list the bucket"), "{unknown}");
        assert!(!unknown.contains("s3:"), "{unknown}");

        // And a launch refusal sends each of them somewhere different again:
        // none of these three remedies is about a bucket.
        for name in ["aws", "digitalocean", "gcp"] {
            let launching =
                error_sentence("launching", Attempt::Machines, name.parse().ok(), &denied);
            assert!(!launching.to_lowercase().contains("bucket"), "{launching}");
            assert!(launching.ends_with("then try again."), "{launching}");
        }
    }

    /// The one provider message a machine-API refusal keeps, and the reason
    /// it cannot be kept whole. `jamstream_cloud`'s AWS provider turns a 403
    /// UnauthorizedOperation into a sentence of its own naming the missing
    /// IAM action, then quotes EC2, whose words carry the account number and
    /// the ARN. Driven through the real provider against a real 403 body, so
    /// this fails if either side stops marking the quote.
    #[tokio::test]
    async fn a_denied_launch_keeps_the_missing_action_and_drops_the_arn() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_string(
                "<Response><Errors><Error><Code>UnauthorizedOperation</Code><Message>\
                 You are not authorized to perform this operation. User: \
                 arn:aws:iam::887762372032:user/jamstream is not authorized to perform: \
                 ec2:DescribeSecurityGroups on resource: \
                 arn:aws:ec2:us-west-2:887762372032:security-group/* because no \
                 identity-based policy allows the ec2:DescribeSecurityGroups action.\
                 </Message></Error></Errors><RequestID>test-req</RequestID></Response>",
            ))
            .mount(&server)
            .await;
        let provider = AwsProvider::new("AKIDTEST".to_owned(), "test-secret".to_owned())
            .with_base_url(server.uri());
        let err = CliError::Provider(
            provider
                .preflight()
                .await
                .expect_err("a policy without DescribeSecurityGroups cannot preflight"),
        );
        let (shown, logged) = with_captured_log(|| {
            error_sentence(
                "checking the credentials",
                Attempt::Machines,
                Some(ProviderKind::Aws),
                &err,
            )
        });
        // The half that is ours: the action to add, and where to add it.
        assert!(shown.contains("ec2:DescribeSecurityGroups"), "{shown}");
        assert!(shown.contains("jamstream-host policy"), "{shown}");
        // The half that is EC2's, which is the half with the account in it.
        for identifier in ["887762372032", "arn:aws:iam", "You are not authorized"] {
            assert!(!shown.contains(identifier), "{identifier} leaked: {shown}");
        }
        assert!(logged.contains("887762372032"), "the log keeps it all");
    }

    /// An AWS machine-API failure the provider rewrote out of an EC2 body
    /// has no http prefix and still is not ours to draw.
    #[test]
    fn a_rewritten_ec2_body_is_not_mistaken_for_one_of_our_sentences() {
        let rewritten = ProviderError::Auth(
            "AuthFailure: AWS was not able to validate the provided access credentials \
             for arn:aws:iam::887762372032:user/jamstream"
                .to_owned(),
        );
        let shown = provider_sentence(Attempt::Machines, Some(ProviderKind::Aws), &rewritten);
        assert!(!shown.contains("arn:"), "{shown}");
        assert!(shown.contains("jamstream-host policy"), "{shown}");
        // The same message from a provider that does no such rewriting is
        // one of ours and is shown: this is the mock's own refusal, and the
        // region step has always reported it verbatim.
        let ours = ProviderError::Auth("token rejected".to_owned());
        assert_eq!(
            provider_sentence(Attempt::Machines, Some(ProviderKind::DigitalOcean), &ours),
            "token rejected"
        );
    }
}
