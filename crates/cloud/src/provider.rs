use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;

use crate::types::{Instance, LaunchSpec, Price, ProviderKind, Region, RegionId};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProviderError {
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("transient failure: {0}")]
    Transient(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, ProviderError>;

/// Injectable sleep so backoff loops are testable without real time.
#[async_trait]
pub trait Sleeper: Send + Sync {
    async fn sleep(&self, d: Duration);
}

pub struct TokioSleeper;

#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

/// Boxed so `wait_reachable` stays object safe on `dyn Provider`.
pub type ProbeFuture = Pin<Box<dyn Future<Output = bool> + Send>>;

#[derive(Debug, Clone, Copy)]
pub struct WaitOpts {
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub total_timeout: Duration,
}

impl Default for WaitOpts {
    fn default() -> Self {
        WaitOpts {
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(8),
            total_timeout: Duration::from_secs(180),
        }
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    /// Static region catalog; must not touch the network.
    fn regions(&self) -> Vec<Region>;

    async fn price(&self, region: &RegionId) -> Result<Price>;

    async fn launch(&self, spec: LaunchSpec) -> Result<Instance>;

    async fn destroy(&self, region: &RegionId, id: &str) -> Result<()>;

    /// Lists jamstream-tagged instances only. `session_tag` narrows to one
    /// session; `None` returns every jamstream-tagged instance.
    async fn list_tagged(&self, session_tag: Option<&str>) -> Result<Vec<Instance>>;

    /// Polls `probe` with capped exponential backoff until it returns true
    /// or the accumulated backoff exceeds `opts.total_timeout`. Elapsed time
    /// is accounted from requested sleeps, so a virtual `Sleeper` makes the
    /// whole wait instantaneous in tests.
    async fn wait_reachable(
        &self,
        probe: &(dyn Fn() -> ProbeFuture + Send + Sync),
        sleeper: &dyn Sleeper,
        opts: WaitOpts,
    ) -> Result<()> {
        let mut backoff = opts.initial_backoff;
        let mut elapsed = Duration::ZERO;
        loop {
            if probe().await {
                return Ok(());
            }
            if elapsed + backoff > opts.total_timeout {
                return Err(ProviderError::Transient(format!(
                    "instance not reachable within {:?}",
                    opts.total_timeout
                )));
            }
            sleeper.sleep(backoff).await;
            elapsed += backoff;
            backoff = (backoff * 2).min(opts.max_backoff);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::mock::MockProvider;

    /// Records requested sleeps and returns immediately.
    struct RecordingSleeper {
        slept: Mutex<Vec<Duration>>,
    }

    #[async_trait]
    impl Sleeper for RecordingSleeper {
        async fn sleep(&self, d: Duration) {
            self.slept.lock().unwrap().push(d);
        }
    }

    fn opts() -> WaitOpts {
        WaitOpts {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(400),
            total_timeout: Duration::from_millis(1500),
        }
    }

    #[tokio::test]
    async fn wait_reachable_succeeds_after_retries() {
        let provider = MockProvider::new(ProviderKind::Aws);
        let sleeper = RecordingSleeper {
            slept: Mutex::new(Vec::new()),
        };
        let attempts = AtomicU32::new(0);
        let probe = move || -> ProbeFuture {
            let n = attempts.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { n >= 3 })
        };
        provider
            .wait_reachable(&probe, &sleeper, opts())
            .await
            .unwrap();
        // Three failures before success: 100, 200, 400 ms backoffs.
        let slept = sleeper.slept.lock().unwrap().clone();
        assert_eq!(
            slept,
            vec![
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(400),
            ]
        );
    }

    #[tokio::test]
    async fn wait_reachable_times_out_with_capped_backoff() {
        let provider = MockProvider::new(ProviderKind::Gcp);
        let sleeper = RecordingSleeper {
            slept: Mutex::new(Vec::new()),
        };
        let probe = || -> ProbeFuture { Box::pin(async { false }) };
        let err = provider
            .wait_reachable(&probe, &sleeper, opts())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Transient(_)));
        let slept = sleeper.slept.lock().unwrap().clone();
        // 100 + 200 + 400 + 400 + 400 = 1500 lands exactly on the timeout;
        // one more 400 would exceed it, so the wait fails there.
        assert_eq!(
            slept,
            vec![
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(400),
                Duration::from_millis(400),
                Duration::from_millis(400),
            ]
        );
    }
}
