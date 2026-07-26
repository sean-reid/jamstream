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

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("jamstream/", env!("CARGO_PKG_VERSION")))
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("reqwest client")
}

/// Sends a request built fresh per attempt (bodies are not replayable
/// otherwise). Retries transient failures and rate limits within the
/// attempt budget; auth and not-found return immediately.
pub async fn send_retrying(build: impl Fn() -> RequestBuilder) -> Result<Response, ProviderError> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let outcome = match build().send().await {
            Ok(resp) => classify(resp),
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

fn classify(resp: Response) -> Outcome {
    let status = resp.status();
    if status.is_success() {
        return Outcome::Ok(resp);
    }
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            Outcome::Fatal(ProviderError::Auth(format!("http {status}")))
        }
        StatusCode::NOT_FOUND | StatusCode::GONE => {
            Outcome::Fatal(ProviderError::NotFound(format!("http {status}")))
        }
        StatusCode::TOO_MANY_REQUESTS => {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(Duration::from_secs);
            Outcome::Retry(ProviderError::RateLimited { retry_after }, retry_after)
        }
        s if s.is_server_error() => {
            Outcome::Retry(ProviderError::Transient(format!("http {status}")), None)
        }
        s => Outcome::Fatal(ProviderError::Other(format!("http {s}"))),
    }
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
