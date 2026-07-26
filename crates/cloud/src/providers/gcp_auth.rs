//! Native GCP service-account authentication. Parses the JSON key file
//! that `GOOGLE_APPLICATION_CREDENTIALS` points at, signs an RS256 JWT
//! with aws-lc-rs (already in the tree as rustls's crypto provider), and
//! exchanges it at the key's `token_uri` for an OAuth2 access token. No
//! gcloud subprocess is involved.
//!
//! [`ServiceAccountTokenSource`] implements
//! [`TokenSource`](super::gcp::TokenSource) and caches the minted token,
//! refreshing once it is within five minutes of expiry. Tokens are
//! secrets: they are never logged and the `Debug` impl redacts both the
//! private key and any cached token.
//!
//! The `iat`/`exp` claims and the cache clock come from an injectable
//! [`Clock`], and the token endpoint can be overridden, so tests are
//! deterministic and never talk to Google.

use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use data_encoding::{BASE64, BASE64URL_NOPAD};
use serde::Deserialize;
use tokio::sync::Mutex;

use super::gcp::TokenSource;
use crate::http::{client, send_retrying};
use crate::provider::{ProviderError, Result};

/// OAuth2 scope requested for every token: full Compute Engine access,
/// which is everything the provider needs (insert, get, list, delete).
const SCOPE: &str = "https://www.googleapis.com/auth/compute";
/// JWT grant type for the service-account assertion flow.
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
/// Requested assertion lifetime; Google caps service-account JWTs at 1h.
const JWT_LIFETIME_SECS: u64 = 3600;
/// A cached token is refreshed once it is this close to expiry.
const REFRESH_MARGIN_SECS: u64 = 300;

/// Seconds since the Unix epoch. Injectable so JWT `iat`/`exp` and cache
/// expiry are deterministic in tests.
pub type Clock = Box<dyn Fn() -> u64 + Send + Sync>;

fn system_clock() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn auth_err(msg: impl Into<String>) -> ProviderError {
    ProviderError::Auth(msg.into())
}

/// The subset of the service-account key JSON this module needs. Real key
/// files carry more fields (`private_key_id`, `client_id`, ...); unknown
/// fields are ignored.
#[derive(Deserialize)]
struct KeyFile {
    #[serde(rename = "type")]
    key_type: String,
    private_key: String,
    client_email: String,
    token_uri: String,
    #[serde(default)]
    project_id: Option<String>,
}

/// A parsed and validated service-account key: identity fields plus the
/// ready-to-sign RSA key pair.
struct ParsedKey {
    key_pair: RsaKeyPair,
    client_email: String,
    token_uri: String,
    project_id: Option<String>,
}

impl ParsedKey {
    fn from_json(json: &str) -> Result<Self> {
        let key: KeyFile = serde_json::from_str(json).map_err(|e| {
            auth_err(format!(
                "service account key JSON is malformed: {e} (expected the \
                 GOOGLE_APPLICATION_CREDENTIALS file downloaded from GCP, with type, \
                 client_email, private_key, and token_uri fields)"
            ))
        })?;
        if key.key_type != "service_account" {
            return Err(auth_err(format!(
                "credentials file has type {:?}, expected \"service_account\" (authorized-user \
                 and external-account credentials are not supported; download a service account \
                 key instead)",
                key.key_type
            )));
        }
        let der = pem_to_der(&key.private_key)?;
        let key_pair = RsaKeyPair::from_pkcs8(&der).map_err(|e| {
            auth_err(format!(
                "service account private_key is not a usable RSA key: {e} (GCP issues 2048-bit \
                 RSA keys; an EC or otherwise non-RSA key cannot sign the RS256 assertion)"
            ))
        })?;
        Ok(ParsedKey {
            key_pair,
            client_email: key.client_email,
            token_uri: key.token_uri,
            project_id: key.project_id,
        })
    }

    /// Builds and signs the `header.claims.signature` JWT assertion.
    fn signed_jwt(&self, iat: u64) -> Result<String> {
        let header = BASE64URL_NOPAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = serde_json::json!({
            "iss": self.client_email,
            "scope": SCOPE,
            "aud": self.token_uri,
            "iat": iat,
            "exp": iat + JWT_LIFETIME_SECS,
        });
        let claims = BASE64URL_NOPAD.encode(claims.to_string().as_bytes());
        let signing_input = format!("{header}.{claims}");
        let mut signature = vec![0u8; self.key_pair.public_modulus_len()];
        self.key_pair
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|e| auth_err(format!("RS256 signing failed: {e}")))?;
        Ok(format!(
            "{signing_input}.{}",
            BASE64URL_NOPAD.encode(&signature)
        ))
    }
}

/// PKCS#8 PEM ("BEGIN PRIVATE KEY") to DER: strip the markers and
/// base64-decode the body, tolerating arbitrary line wrapping.
fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    const BEGIN: &str = "-----BEGIN PRIVATE KEY-----";
    const END: &str = "-----END PRIVATE KEY-----";
    let trimmed = pem.trim();
    let body = trimmed
        .strip_prefix(BEGIN)
        .and_then(|rest| rest.strip_suffix(END))
        .ok_or_else(|| {
            auth_err(
                "service account private_key is not a PKCS#8 PEM: expected \
                 \"-----BEGIN PRIVATE KEY-----\" ... \"-----END PRIVATE KEY-----\" (a \
                 \"BEGIN RSA PRIVATE KEY\" PKCS#1 block or a non-key PEM is not supported; \
                 GCP-issued key files are already PKCS#8)",
            )
        })?;
    let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    BASE64.decode(compact.as_bytes()).map_err(|e| {
        auth_err(format!(
            "service account private_key PEM body is not base64: {e}"
        ))
    })
}

struct CachedToken {
    token: String,
    /// Unix seconds at which the token expires, per the endpoint's
    /// `expires_in` measured from our request time.
    expires_at: u64,
}

/// [`TokenSource`] backed by a GCP service-account key: signs an RS256 JWT
/// natively and exchanges it for an access token, cached until five
/// minutes before expiry. See the module docs.
pub struct ServiceAccountTokenSource {
    key: ParsedKey,
    /// Where the assertion is POSTed. Defaults to the key's `token_uri`
    /// (which is also the JWT `aud` claim regardless of this override);
    /// tests point it at a mock server.
    token_endpoint: String,
    clock: Clock,
    http: reqwest::Client,
    cached: Mutex<Option<CachedToken>>,
}

impl ServiceAccountTokenSource {
    /// Parses a service-account key from its JSON text.
    pub fn from_json(json: &str) -> Result<Self> {
        let key = ParsedKey::from_json(json)?;
        let token_endpoint = key.token_uri.clone();
        Ok(ServiceAccountTokenSource {
            key,
            token_endpoint,
            clock: Box::new(system_clock),
            http: client(),
            cached: Mutex::new(None),
        })
    }

    /// Reads and parses a service-account key file (the path
    /// `GOOGLE_APPLICATION_CREDENTIALS` points at).
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let json = std::fs::read_to_string(path).map_err(|e| {
            auth_err(format!(
                "cannot read service account key file {}: {e}",
                path.display()
            ))
        })?;
        Self::from_json(&json)
    }

    /// The key's `project_id` field, when present. `from_env` uses it as
    /// the fallback when `GOOGLE_CLOUD_PROJECT` is unset.
    pub fn project_id(&self) -> Option<&str> {
        self.key.project_id.as_deref()
    }

    /// Overrides where the signed assertion is POSTed (tests point this at
    /// a mock server). The JWT `aud` claim stays the key's `token_uri`.
    pub fn with_token_endpoint(mut self, url: impl Into<String>) -> Self {
        self.token_endpoint = url.into();
        self
    }

    /// Overrides the clock used for JWT `iat`/`exp` and cache expiry, so
    /// tests are deterministic.
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Signs a fresh assertion and exchanges it. Callers hold the cache
    /// lock, so concurrent token requests collapse into one exchange.
    async fn exchange(&self, now: u64) -> Result<CachedToken> {
        let assertion = self.key.signed_jwt(now)?;
        let params = [
            ("grant_type", GRANT_TYPE),
            ("assertion", assertion.as_str()),
        ];
        let resp = send_retrying(|| self.http.post(&self.token_endpoint).form(&params)).await?;
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            expires_in: u64,
        }
        let body: TokenResponse = resp.json().await.map_err(|e| {
            auth_err(format!(
                "gcp token endpoint returned an unparseable response: {e}"
            ))
        })?;
        Ok(CachedToken {
            token: body.access_token,
            expires_at: now + body.expires_in,
        })
    }
}

#[async_trait]
impl TokenSource for ServiceAccountTokenSource {
    async fn access_token(&self) -> Result<String> {
        let now = (self.clock)();
        let mut cached = self.cached.lock().await;
        if let Some(current) = cached.as_ref()
            && now + REFRESH_MARGIN_SECS < current.expires_at
        {
            return Ok(current.token.clone());
        }
        let fresh = self.exchange(now).await?;
        let token = fresh.token.clone();
        *cached = Some(fresh);
        Ok(token)
    }
}

impl fmt::Debug for ServiceAccountTokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceAccountTokenSource")
            .field("client_email", &self.key.client_email)
            .field("token_endpoint", &self.token_endpoint)
            .field("project_id", &self.key.project_id)
            .field("private_key", &"<redacted>")
            .field("cached", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use aws_lc_rs::signature::{KeyPair, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
    use serde_json::Value;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// Throwaway test-only key, generated for this repo and provisioned
    /// nowhere. See the comment field inside the fixture.
    const TEST_KEY_JSON: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/gcp_test_key.json"
    ));

    /// A PKCS#8 PEM that is a valid key but not RSA (throwaway P-256,
    /// generated alongside the RSA fixture, likewise provisioned nowhere).
    const EC_PKCS8_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgYjN4H09P4aDgVp8F\nNbzzXZTd45NXRmjq7GqFPcEcykihRANCAARzHy8vAhniXyL0i4/2zTmDZl5s0A0b\nYJty/o00K4Xs66e/ELpAPW+H6FKdHjZaQT90R0Wv+2R0DJosUXKCmtpt\n-----END PRIVATE KEY-----\n";

    fn fixture_key() -> ParsedKey {
        ParsedKey::from_json(TEST_KEY_JSON).expect("fixture key must parse")
    }

    fn fixed_clock(secs: u64) -> Clock {
        Box::new(move || secs)
    }

    fn token_response(token: &str, expires_in: u64) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": token,
            "expires_in": expires_in,
            "token_type": "Bearer",
        }))
    }

    #[test]
    fn fixture_pem_parses_to_a_2048_bit_rsa_key() {
        let key = fixture_key();
        // 2048-bit modulus signs 256-byte signatures.
        assert_eq!(key.key_pair.public_modulus_len(), 256);
        assert_eq!(
            key.client_email,
            "jamstream-test@jamstream-test-project.iam.gserviceaccount.com"
        );
        assert_eq!(key.token_uri, "https://oauth2.googleapis.com/token");
        assert_eq!(key.project_id.as_deref(), Some("jamstream-test-project"));
    }

    #[test]
    fn pem_to_der_tolerates_line_wraps_and_padding() {
        let key: KeyFile = serde_json::from_str(TEST_KEY_JSON).unwrap();
        let wrapped = pem_to_der(&key.private_key).expect("wrapped PEM");
        // The same body flattened to one long line must decode identically.
        let one_line = key
            .private_key
            .replace("\n", "")
            .replace(
                "-----BEGIN PRIVATE KEY-----",
                "-----BEGIN PRIVATE KEY-----\n",
            )
            .replace("-----END PRIVATE KEY-----", "\n-----END PRIVATE KEY-----");
        let flat = pem_to_der(&one_line).expect("single-line PEM");
        assert_eq!(wrapped, flat);
    }

    #[test]
    fn jwt_segments_decode_to_expected_header_and_claims() {
        let key = fixture_key();
        let jwt = key.signed_jwt(1_700_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);

        let header: Value =
            serde_json::from_slice(&BASE64URL_NOPAD.decode(parts[0].as_bytes()).unwrap()).unwrap();
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["typ"], "JWT");

        let claims: Value =
            serde_json::from_slice(&BASE64URL_NOPAD.decode(parts[1].as_bytes()).unwrap()).unwrap();
        assert_eq!(
            claims["iss"],
            "jamstream-test@jamstream-test-project.iam.gserviceaccount.com"
        );
        assert_eq!(claims["scope"], "https://www.googleapis.com/auth/compute");
        assert_eq!(claims["aud"], "https://oauth2.googleapis.com/token");
        assert_eq!(claims["iat"], 1_700_000_000u64);
        assert_eq!(claims["exp"], 1_700_003_600u64);
    }

    #[test]
    fn jwt_signature_verifies_against_the_public_key() {
        let key = fixture_key();
        let jwt = key.signed_jwt(1_700_000_000).unwrap();
        let (signing_input, sig_b64) = jwt.rsplit_once('.').unwrap();
        let signature = BASE64URL_NOPAD.decode(sig_b64.as_bytes()).unwrap();
        let public = UnparsedPublicKey::new(
            &RSA_PKCS1_2048_8192_SHA256,
            key.key_pair.public_key().as_ref(),
        );
        public
            .verify(signing_input.as_bytes(), &signature)
            .expect("RS256 signature must verify with the key's public half");
        // A tampered payload must not verify.
        public
            .verify(format!("{signing_input}x").as_bytes(), &signature)
            .expect_err("tampered input must fail verification");
    }

    #[tokio::test]
    async fn token_exchange_posts_the_assertion_form_and_returns_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains(
                "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer",
            ))
            .and(body_string_contains("assertion="))
            .respond_with(token_response("minted-token", 3600))
            .expect(1)
            .mount(&server)
            .await;

        let source = ServiceAccountTokenSource::from_json(TEST_KEY_JSON)
            .unwrap()
            .with_token_endpoint(format!("{}/token", server.uri()))
            .with_clock(fixed_clock(1_700_000_000));
        let token = source.access_token().await.unwrap();
        assert_eq!(token, "minted-token");

        // The assertion in the form body is the signed JWT for our clock.
        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8(requests[0].body.clone()).unwrap();
        let assertion = body
            .split('&')
            .find_map(|pair| pair.strip_prefix("assertion="))
            .expect("assertion field");
        // Base64url-nopad JWTs contain no characters that form-encoding
        // escapes except '.', which stays literal.
        let claims_b64 = assertion.split('.').nth(1).unwrap();
        let claims: Value =
            serde_json::from_slice(&BASE64URL_NOPAD.decode(claims_b64.as_bytes()).unwrap())
                .unwrap();
        assert_eq!(claims["iat"], 1_700_000_000u64);
        assert_eq!(claims["aud"], "https://oauth2.googleapis.com/token");
    }

    #[tokio::test]
    async fn token_is_cached_across_calls() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(token_response("cached-token", 3600))
            .expect(1)
            .mount(&server)
            .await;

        let source = ServiceAccountTokenSource::from_json(TEST_KEY_JSON)
            .unwrap()
            .with_token_endpoint(format!("{}/token", server.uri()))
            .with_clock(fixed_clock(1_700_000_000));
        assert_eq!(source.access_token().await.unwrap(), "cached-token");
        assert_eq!(source.access_token().await.unwrap(), "cached-token");
        server.verify().await;
    }

    #[tokio::test]
    async fn token_refreshes_within_five_minutes_of_expiry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(token_response("short-token", 3600))
            .expect(2)
            .mount(&server)
            .await;

        let now = Arc::new(AtomicU64::new(1_700_000_000));
        let clock_now = Arc::clone(&now);
        let source = ServiceAccountTokenSource::from_json(TEST_KEY_JSON)
            .unwrap()
            .with_token_endpoint(format!("{}/token", server.uri()))
            .with_clock(Box::new(move || clock_now.load(Ordering::SeqCst)));

        source.access_token().await.unwrap();
        // Exactly five minutes before the 3600-second expiry: inside the
        // refresh margin, so the cache must not be reused.
        now.store(1_700_000_000 + 3600 - REFRESH_MARGIN_SECS, Ordering::SeqCst);
        source.access_token().await.unwrap();
        server.verify().await;
    }

    #[tokio::test]
    async fn token_just_outside_the_margin_is_reused() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(token_response("long-token", 3600))
            .expect(1)
            .mount(&server)
            .await;

        let now = Arc::new(AtomicU64::new(1_700_000_000));
        let clock_now = Arc::clone(&now);
        let source = ServiceAccountTokenSource::from_json(TEST_KEY_JSON)
            .unwrap()
            .with_token_endpoint(format!("{}/token", server.uri()))
            .with_clock(Box::new(move || clock_now.load(Ordering::SeqCst)));

        source.access_token().await.unwrap();
        // One second earlier than the refresh margin: still cached.
        now.store(
            1_700_000_000 + 3600 - REFRESH_MARGIN_SECS - 1,
            Ordering::SeqCst,
        );
        source.access_token().await.unwrap();
        server.verify().await;
    }

    #[test]
    fn malformed_key_json_is_an_auth_error_with_guidance() {
        for bad in ["not json at all", "{}", r#"{"type":"service_account"}"#] {
            let err = ServiceAccountTokenSource::from_json(bad).unwrap_err();
            match err {
                ProviderError::Auth(msg) => {
                    assert!(
                        msg.contains("malformed") || msg.contains("missing field"),
                        "unhelpful message for {bad:?}: {msg}"
                    );
                }
                other => panic!("expected Auth for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn wrong_credential_type_is_rejected() {
        let mut key: Value = serde_json::from_str(TEST_KEY_JSON).unwrap();
        key["type"] = Value::String("authorized_user".to_owned());
        let err = ServiceAccountTokenSource::from_json(&key.to_string()).unwrap_err();
        match err {
            ProviderError::Auth(msg) => assert!(msg.contains("service_account"), "{msg}"),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn non_pkcs8_pem_is_rejected_with_guidance() {
        let mut key: Value = serde_json::from_str(TEST_KEY_JSON).unwrap();
        key["private_key"] = Value::String(
            "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----\n".to_owned(),
        );
        let err = ServiceAccountTokenSource::from_json(&key.to_string()).unwrap_err();
        match err {
            ProviderError::Auth(msg) => assert!(msg.contains("PKCS#8"), "{msg}"),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn non_rsa_key_is_rejected_with_guidance() {
        let mut key: Value = serde_json::from_str(TEST_KEY_JSON).unwrap();
        key["private_key"] = Value::String(EC_PKCS8_PEM.to_owned());
        let err = ServiceAccountTokenSource::from_json(&key.to_string()).unwrap_err();
        match err {
            ProviderError::Auth(msg) => assert!(msg.contains("RSA"), "{msg}"),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn missing_key_file_is_an_auth_error_naming_the_path() {
        let err = ServiceAccountTokenSource::from_file("/nonexistent/gcp-key.json").unwrap_err();
        match err {
            ProviderError::Auth(msg) => assert!(msg.contains("/nonexistent/gcp-key.json")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn debug_redacts_key_material_and_tokens() {
        let source = ServiceAccountTokenSource::from_json(TEST_KEY_JSON).unwrap();
        let rendered = format!("{source:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("PRIVATE KEY"));
        assert!(rendered.contains("jamstream-test@jamstream-test-project.iam.gserviceaccount.com"));
    }
}
