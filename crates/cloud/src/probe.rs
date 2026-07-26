//! TCP connect timing against per-region endpoints. Verified 2026-07-25:
//! every catalog host resolves and accepts on 443. The GCP regional
//! googleapis hosts terminate on anycast Google Front Ends, so their connect
//! time only approximates region latency; they carry approx=true and should
//! move to gcping-style HTTP timing when the client grows an HTTP probe.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::task::JoinSet;

use crate::types::{ProviderKind, RegionId};

const PROBES_JSON: &str = include_str!("../data/probes.json");

pub const PROBE_ATTEMPTS: usize = 3;
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProbeTarget {
    pub provider: ProviderKind,
    #[serde(rename = "region")]
    pub region: RegionId,
    #[serde(rename = "host")]
    pub url_host: String,
    pub port: u16,
    /// True where the endpoint only approximates the region (anycast fronts,
    /// nearest-neighbor mappings).
    #[serde(default)]
    pub approx: bool,
}

/// The embedded catalog. Panics only if the checked-in JSON is malformed,
/// which the tests catch.
pub fn probe_catalog() -> Vec<ProbeTarget> {
    serde_json::from_str(PROBES_JSON).expect("data/probes.json is invalid")
}

/// Minimum elapsed time over `PROBE_ATTEMPTS` TCP connects, or None if every
/// attempt failed or timed out.
pub async fn probe_target(host: &str, port: u16) -> Option<f32> {
    let mut best: Option<f32> = None;
    for _ in 0..PROBE_ATTEMPTS {
        let start = Instant::now();
        let attempt = tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect((host, port))).await;
        if let Ok(Ok(_stream)) = attempt {
            let ms = start.elapsed().as_secs_f32() * 1000.0;
            best = Some(best.map_or(ms, |b: f32| b.min(ms)));
        }
    }
    best
}

/// Probes all targets concurrently. Regions that never connect are absent
/// from the result; the solver treats absence as a missing probe.
pub async fn probe_all(targets: &[ProbeTarget]) -> HashMap<RegionId, f32> {
    let mut set = JoinSet::new();
    for t in targets {
        let region = t.region.clone();
        let host = t.url_host.clone();
        let port = t.port;
        set.spawn(async move { (region, probe_target(&host, port).await) });
    }
    let mut out = HashMap::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((region, Some(rtt))) = joined {
            out.insert(region, rtt);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn catalog_parses_and_covers_all_providers() {
        let catalog = probe_catalog();
        assert_eq!(
            catalog
                .iter()
                .filter(|t| t.provider == ProviderKind::Aws)
                .count(),
            8
        );
        assert_eq!(
            catalog
                .iter()
                .filter(|t| t.provider == ProviderKind::DigitalOcean)
                .count(),
            9
        );
        let gcp: Vec<_> = catalog
            .iter()
            .filter(|t| t.provider == ProviderKind::Gcp)
            .collect();
        assert_eq!(gcp.len(), 9);
        // Anycast GFE endpoints must be flagged as approximations.
        assert!(gcp.iter().all(|t| t.approx));
        // Region ids must be globally unique: the probe result map is keyed
        // by RegionId alone.
        let mut ids: Vec<&str> = catalog.iter().map(|t| t.region.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), catalog.len());
        assert!(catalog.iter().all(|t| t.port == 443));
    }

    #[tokio::test]
    async fn probe_local_listener_measures_rtt() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let _ = listener.accept().await;
            }
        });
        let rtt = probe_target("127.0.0.1", port).await.unwrap();
        assert!((0.0..1000.0).contains(&rtt), "loopback rtt was {rtt} ms");
    }

    #[tokio::test]
    async fn probe_closed_port_returns_none() {
        // Bind then drop to get a port that is very likely closed.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert_eq!(probe_target("127.0.0.1", port).await, None);
    }

    #[tokio::test]
    async fn probe_all_mixes_reachable_and_unreachable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open_port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let _ = listener.accept().await;
            }
        });
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_port = closed.local_addr().unwrap().port();
        drop(closed);

        let targets = vec![
            ProbeTarget {
                provider: ProviderKind::Aws,
                region: RegionId::new("up"),
                url_host: "127.0.0.1".to_owned(),
                port: open_port,
                approx: false,
            },
            ProbeTarget {
                provider: ProviderKind::Aws,
                region: RegionId::new("down"),
                url_host: "127.0.0.1".to_owned(),
                port: closed_port,
                approx: false,
            },
        ];
        let results = probe_all(&targets).await;
        assert!(results.contains_key(&RegionId::new("up")));
        assert!(!results.contains_key(&RegionId::new("down")));
    }
}
