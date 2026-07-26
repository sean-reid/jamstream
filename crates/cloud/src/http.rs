//! Shared HTTP layer for every provider implementation. Politeness is not
//! optional: bounded retries with exponential backoff, Retry-After honored
//! on 429, and no retry at all on auth failures.

use std::time::Duration;

use reqwest::{RequestBuilder, Response, StatusCode};

use crate::provider::ProviderError;

const MAX_ATTEMPTS: u32 = 4;
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(4);
/// A Retry-After beyond this means the service wants us gone for a while;
/// surface the error instead of stalling a session launch.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Error bodies are carried on the error for diagnosis; anything past this
/// is noise (providers put the useful code/message up front).
const ERROR_BODY_CAP: usize = 4096;

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("jamstream/", env!("CARGO_PKG_VERSION")))
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("reqwest client")
}

/// Sends a request built fresh per attempt (bodies are not replayable
/// otherwise). Retries transient failures and rate limits within the
/// attempt budget; auth and not-found return immediately. Non-2xx
/// responses have up to 4 KB of body read and embedded in the returned
/// error; [`error_body`] recovers the raw snippet.
pub async fn send_retrying(build: impl Fn() -> RequestBuilder) -> Result<Response, ProviderError> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let outcome = match build().send().await {
            Ok(resp) => classify(resp).await,
            Err(err) if err.is_connect() || err.is_timeout() => {
                Outcome::Retry(ProviderError::Transient(err.to_string()), None)
            }
            Err(err) => return Err(ProviderError::Other(err.to_string())),
        };
        match outcome {
            Outcome::Ok(resp) => return Ok(resp),
            Outcome::Fatal(err) => return Err(err),
            Outcome::Retry(err, retry_after) => {
                if attempt >= MAX_ATTEMPTS {
                    return Err(err);
                }
                let backoff = BACKOFF_BASE
                    .saturating_mul(1 << (attempt - 1).min(4))
                    .min(BACKOFF_CAP);
                let wait = match retry_after {
                    Some(ra) if ra > RETRY_AFTER_CAP => return Err(err),
                    Some(ra) => ra.max(backoff),
                    None => backoff,
                };
                tokio::time::sleep(wait).await;
            }
        }
    }
}

enum Outcome {
    Ok(Response),
    Fatal(ProviderError),
    Retry(ProviderError, Option<Duration>),
}

async fn classify(resp: Response) -> Outcome {
    let status = resp.status();
    if status.is_success() {
        return Outcome::Ok(resp);
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs);
        return Outcome::Retry(ProviderError::RateLimited { retry_after }, retry_after);
    }
    // Non-2xx bodies carry the provider's error detail (EC2 XML codes, DO
    // JSON messages); read a bounded snippet and carry it on the error.
    let msg = error_message(status, &error_snippet(resp).await);
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            Outcome::Fatal(ProviderError::Auth(msg))
        }
        StatusCode::NOT_FOUND | StatusCode::GONE => Outcome::Fatal(ProviderError::NotFound(msg)),
        s if s.is_server_error() => Outcome::Retry(ProviderError::Transient(msg), None),
        _ => Outcome::Fatal(ProviderError::Other(msg)),
    }
}

/// At most `ERROR_BODY_CAP` bytes of the response body, lossily decoded
/// and trimmed. Read failures degrade to an empty snippet: the status
/// classification must survive a half-delivered error body.
async fn error_snippet(mut resp: Response) -> String {
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < ERROR_BODY_CAP {
        match resp.chunk().await {
            Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
            Ok(None) | Err(_) => break,
        }
    }
    buf.truncate(ERROR_BODY_CAP);
    String::from_utf8_lossy(&buf).trim().to_owned()
}

/// The message format `error_body` parses back: "http {status}" with the
/// body snippet appended after ": " when there is one.
fn error_message(status: StatusCode, snippet: &str) -> String {
    if snippet.is_empty() {
        format!("http {status}")
    } else {
        format!("http {status}: {snippet}")
    }
}

/// Extracts the response-body snippet embedded by this module in a
/// classified error, if any. The structured escape hatch for providers
/// that want to parse the body (EC2 error XML, DO error JSON) instead of
/// string-matching the rendered message. Returns None for errors that did
/// not come from an HTTP status classification or carried no body.
pub fn error_body(err: &ProviderError) -> Option<&str> {
    let msg = match err {
        ProviderError::Auth(m)
        | ProviderError::QuotaExceeded(m)
        | ProviderError::NotFound(m)
        | ProviderError::Transient(m)
        | ProviderError::Other(m) => m,
        ProviderError::RateLimited { .. } => return None,
    };
    // "http {status}: {body}" where status renders as "400 Bad Request".
    let rest = msg.strip_prefix("http ")?;
    let (status, body) = rest.split_once(": ")?;
    let looks_like_status = status.len() >= 3 && status[..3].bytes().all(|b| b.is_ascii_digit());
    (looks_like_status && !body.is_empty()).then_some(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn success_passes_through() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(ResponseTemplate::new(200).set_body_string("fine"))
            .mount(&server)
            .await;
        let c = client();
        let resp = send_retrying(|| c.get(format!("{}/ok", server.uri())))
            .await
            .unwrap();
        assert_eq!(resp.text().await.unwrap(), "fine");
    }

    #[tokio::test]
    async fn transient_five_hundred_retries_to_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/flaky"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(2)
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/flaky"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let c = client();
        let resp = send_retrying(|| c.get(format!("{}/flaky", server.uri()))).await;
        assert!(resp.is_ok());
    }

    #[tokio::test]
    async fn rate_limit_honors_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/limited"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/limited"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let c = client();
        let started = std::time::Instant::now();
        let resp = send_retrying(|| c.get(format!("{}/limited", server.uri()))).await;
        assert!(resp.is_ok());
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "retry-after was not honored: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn excessive_retry_after_fails_fast() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/blocked"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3600"))
            .mount(&server)
            .await;
        let c = client();
        let started = std::time::Instant::now();
        let err = send_retrying(|| c.get(format!("{}/blocked", server.uri()))).await;
        assert!(matches!(err, Err(ProviderError::RateLimited { .. })));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn error_bodies_are_carried_and_extractable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/reject"))
            .respond_with(ResponseTemplate::new(422).set_body_string(r#"{"message":"bad region"}"#))
            .expect(1)
            .mount(&server)
            .await;
        let c = client();
        let err = send_retrying(|| c.post(format!("{}/reject", server.uri())))
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Other(_)), "got {err:?}");
        assert!(err.to_string().contains(r#"{"message":"bad region"}"#));
        assert_eq!(error_body(&err), Some(r#"{"message":"bad region"}"#));
    }

    #[tokio::test]
    async fn error_body_is_capped_and_absent_when_empty() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/huge"))
            .respond_with(ResponseTemplate::new(400).set_body_string("x".repeat(64 * 1024)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/empty"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&server)
            .await;
        let c = client();

        let err = send_retrying(|| c.get(format!("{}/huge", server.uri())))
            .await
            .unwrap_err();
        let body = error_body(&err).expect("body snippet");
        assert_eq!(body.len(), ERROR_BODY_CAP);

        let err = send_retrying(|| c.get(format!("{}/empty", server.uri())))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "http 400 Bad Request");
        assert_eq!(error_body(&err), None);
    }

    #[test]
    fn error_body_ignores_non_http_errors() {
        assert_eq!(error_body(&ProviderError::Other("boom".to_owned())), None);
        assert_eq!(
            error_body(&ProviderError::NotFound("aws region x: y".to_owned())),
            None
        );
        assert_eq!(
            error_body(&ProviderError::RateLimited { retry_after: None }),
            None
        );
    }

    #[tokio::test]
    async fn auth_failures_never_retry() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/denied"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let c = client();
        let err = send_retrying(|| c.get(format!("{}/denied", server.uri()))).await;
        assert!(matches!(err, Err(ProviderError::Auth(_))));
    }
}
