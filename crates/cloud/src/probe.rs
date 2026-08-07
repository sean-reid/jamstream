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
    use tokio::net::{TcpListener, TcpSocket};

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

    /// A port that refuses connections for as long as the returned socket is
    /// held. Bound but never listened on, so a SYN draws RST while the port
    /// stays reserved.
    ///
    /// Binding a listener and dropping it hands the port straight back to the
    /// kernel, and a sibling test in the same binary can be given it before the
    /// probe runs. That is not hypothetical: it reported Some(0.039103) on CI
    /// where the closed port should have been None.
    fn reserved_closed_port() -> (TcpSocket, u16) {
        let socket = TcpSocket::new_v4().expect("v4 socket");
        socket
            .bind("127.0.0.1:0".parse().expect("loopback addr"))
            .expect("bind ephemeral");
        let port = socket.local_addr().expect("local addr").port();
        (socket, port)
    }

    #[tokio::test]
    async fn probe_closed_port_returns_none() {
        let (_held, port) = reserved_closed_port();
        assert_eq!(probe_target("127.0.0.1", port).await, None);
    }

    /// The property the fix rests on, asserted rather than assumed: while the
    /// socket is held the kernel will not hand that port to anyone else, so a
    /// sibling test cannot take it and start answering.
    #[tokio::test]
    async fn a_reserved_closed_port_cannot_be_taken_by_anyone_else() {
        let (_held, port) = reserved_closed_port();
        let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
        assert!(
            TcpListener::bind(addr).await.is_err(),
            "port {port} was rebindable while held, so the probe could race a listener"
        );
    }

    /// Runs the shipped catalog against the real internet from this machine
    /// and prints one line per target: resolved address, per-attempt outcome,
    /// and the value `probe_all` would return. Ignored because it needs the
    /// network and takes seconds; it exists because "every region reads the
    /// same" is a catalog or connectivity question that no mock can answer.
    ///
    /// ```console
    /// $ cargo nextest run -p jamstream-cloud --run-ignored all \
    ///     probe_the_shipped_catalog --no-capture
    /// ```
    #[tokio::test]
    #[ignore = "hits the real network"]
    async fn probe_the_shipped_catalog() {
        let catalog = probe_catalog();
        let results = probe_all(&catalog).await;
        let mut failed = Vec::new();
        for t in &catalog {
            let addrs = tokio::net::lookup_host((t.url_host.as_str(), t.port))
                .await
                .map(|it| it.map(|a| a.to_string()).collect::<Vec<_>>())
                .unwrap_or_else(|e| vec![format!("dns failed: {e}")]);
            match results.get(&t.region) {
                Some(rtt) => println!(
                    "{:>16} {:<40} {:>8.1} ms  {}",
                    t.region.as_str(),
                    t.url_host,
                    rtt,
                    addrs.join(", ")
                ),
                None => {
                    println!(
                        "{:>16} {:<40} {:>11}  {}",
                        t.region.as_str(),
                        t.url_host,
                        "no probe",
                        addrs.join(", ")
                    );
                    failed.push(t.region.as_str().to_owned());
                }
            }
        }
        println!(
            "{} of {} targets answered",
            catalog.len() - failed.len(),
            catalog.len()
        );
        assert!(
            failed.is_empty(),
            "unreachable probe targets: {}",
            failed.join(", ")
        );
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
        let (_closed_held, closed_port) = reserved_closed_port();

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
