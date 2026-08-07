//! Whether the broadcast relay is actually there.
//!
//! The encoder publishes to a MediaMTX instance on this machine and every
//! pusher reads from it, so the relay is the one process a broadcast cannot do
//! without. Nothing ever checked it. Its unit is `Type=simple`, and systemd
//! calls one of those started the moment it forks, so `Started
//! mediamtx.service` in a console log says nothing about a relay that died a
//! second later, and everything after boot goes to a journal no host can
//! reach.
//!
//! The probe lives here, in the session server, rather than in cloud-init, for
//! three reasons. A boot-time check cannot see a relay that dies in the fortieth
//! minute of a three hour session, and this process is running for all of it.
//! This is the only process that can tell the host: cloud-init's one output
//! channel is the console log, which is the thing that could not be read. And
//! this is the process whose children publish to the relay, so its reachability
//! from here is the question that matters; asked from anywhere else it is a
//! different question with the same shape.
//!
//! What cloud-init contributes is the half a probe cannot know: when the tooling
//! never downloaded, it leaves the reason in a note file, and that sentence is
//! what the host is told instead of a generic absence. On a session hosted on
//! someone's own machine nothing downloads anything, so the equivalent half is
//! the relay binary itself: a machine that does not have it is told so by name.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jamstream_protocol::control::{BroadcastReadiness, fit_stream_reason};
use jamstream_stream::tools;

/// How often the relay is probed. A loopback connect costs microseconds, so
/// the interval is about noise in the log rather than about cost.
const PROBE_PERIOD: Duration = Duration::from_secs(5);

/// How long a connect may take before it counts as nothing listening. Loopback
/// answers immediately or refuses immediately; the timeout is for the case
/// where the relay's accept queue is wedged, which is also not a relay a
/// broadcast can use.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the relay gets to appear before its absence is reported.
///
/// It is downloaded and started after the session server, so at the first
/// probe there is legitimately nothing there: two archives of about a hundred
/// megabytes, plus up to thirty seconds of retry pauses if a mirror is having
/// a bad day. Claiming a session cannot broadcast while its relay is still
/// arriving would be wrong in the direction that costs a host a broadcast they
/// have.
///
/// Only the first sighting waits. Once the relay has answered, a later absence
/// is reported at the next probe, because that is a relay that has died and
/// nothing is going to bring it back inside a window.
const FIRST_SIGHTING_GRACE: Duration = Duration::from_secs(180);

/// The TCP address a relay URL names, when it names one on this machine.
///
/// Deliberately strict. It answers "which loopback port should something be
/// listening on", and anything else is a target this cannot honestly probe: a
/// plain file path (which is what the encode tests publish to), a hostname, or
/// an address off this machine. Those read as no answer rather than as a
/// failure, so a surface never dims Go Live over a target nobody checked.
pub fn relay_addr(target: &str) -> Option<SocketAddr> {
    let after_scheme = target.split_once("://").map(|(_, rest)| rest)?;
    let authority = after_scheme.split('/').next()?;
    let addr: SocketAddr = authority.parse().ok()?;
    addr.ip().is_loopback().then_some(addr)
}

/// The relay probe, as the server's heartbeat drives it.
pub struct RelayWatch {
    addr: SocketAddr,
    /// Where cloud-init leaves the reason the tooling never arrived. Read on
    /// each failed probe, not cached: the file appears after this process
    /// starts, because the fetch runs after the session server is enabled.
    note: Option<PathBuf>,
    /// Uptime at which the relay was first seen, if it ever was.
    seen_ms: Option<u64>,
    next_probe_ms: u64,
    grace: Duration,
}

impl RelayWatch {
    /// A watch on whatever `target` names, or None when it names nothing this
    /// can probe.
    pub fn new(target: &str, note: Option<PathBuf>) -> Option<RelayWatch> {
        Some(RelayWatch {
            addr: relay_addr(target)?,
            note,
            seen_ms: None,
            next_probe_ms: 0,
            grace: FIRST_SIGHTING_GRACE,
        })
    }

    /// Shortens the wait before a relay that has never answered is reported
    /// missing. Tests use it; on a session VM the relay is still downloading.
    #[must_use]
    pub fn with_grace(mut self, grace: Duration) -> RelayWatch {
        self.grace = grace;
        self
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// One heartbeat's worth of probing: the reading when one is due and
    /// there is an answer to give, None otherwise. The caller hands whatever
    /// comes back to `ServerCore::set_broadcast_readiness`, which fans it out
    /// on change.
    pub async fn observe(&mut self, now_ms: u64) -> Option<BroadcastReadiness> {
        if now_ms < self.next_probe_ms {
            return None;
        }
        self.next_probe_ms = now_ms + PROBE_PERIOD.as_millis() as u64;
        if self.connect().await {
            self.seen_ms.get_or_insert(now_ms);
            return Some(BroadcastReadiness::Ready);
        }
        // Never seen, and not overdue yet: the relay is still arriving.
        if self.seen_ms.is_none() && now_ms < self.grace.as_millis() as u64 {
            return None;
        }
        Some(BroadcastReadiness::Unavailable {
            reason: fit_stream_reason(&self.reason()).to_owned(),
        })
    }

    async fn connect(&self) -> bool {
        let attempt = tokio::net::TcpStream::connect(self.addr);
        matches!(
            tokio::time::timeout(CONNECT_TIMEOUT, attempt).await,
            Ok(Ok(_))
        )
    }

    /// What the host is told. cloud-init's note wins when there is one: it
    /// knows the tooling never downloaded, which this cannot tell from a relay
    /// that died.
    fn reason(&self) -> String {
        self.note
            .as_deref()
            .and_then(read_note)
            .unwrap_or_else(|| absence(self.addr, tools::installed(Path::new(tools::MEDIAMTX))))
    }
}

/// Why nothing is listening, in the two shapes it comes in. A machine without
/// the relay program is the case a host can act on, so it is named instead of
/// being described as a quiet port.
fn absence(addr: SocketAddr, relay_installed: bool) -> String {
    if relay_installed {
        format!("no broadcast relay is listening on {addr}")
    } else {
        tools::missing(Path::new(tools::MEDIAMTX))
    }
}

/// The first nonempty line of the note, trimmed. Anything else (no file, an
/// empty one, bytes that are not text) reads as no note.
fn read_note(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(line.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Whether a reason describes a relay that is not there, in whichever of
    /// its two shapes this machine earns: the port when the relay program is
    /// installed, the program when it is not. Every runner is one or the
    /// other, and `absence` below pins both branches exactly.
    fn names_the_absence(reason: &str, addr: SocketAddr) -> bool {
        reason == absence(addr, true) || reason == absence(addr, false)
    }

    /// A quiet port is worth reporting; a machine that has no relay program at
    /// all is worth naming, because that one a host can fix.
    #[test]
    fn an_absence_names_the_relay_program_when_this_machine_has_none() {
        let addr: SocketAddr = "127.0.0.1:1935".parse().unwrap();
        let listening = absence(addr, true);
        assert!(
            listening.contains("no broadcast relay is listening"),
            "{listening}"
        );
        assert!(listening.contains("127.0.0.1:1935"), "{listening}");

        let uninstalled = absence(addr, false);
        assert!(
            uninstalled.starts_with("mediamtx is not installed"),
            "{uninstalled}"
        );
        assert_eq!(fit_stream_reason(&uninstalled), uninstalled);
    }

    /// The address the pipeline's default actually names, so a change to
    /// either side shows up here rather than as a probe of the wrong port.
    #[test]
    fn the_relay_url_the_pipeline_publishes_to_is_the_one_probed() {
        let cfg = jamstream_stream::pipeline::StreamConfig::default();
        assert_eq!(
            relay_addr(&cfg.encoder_output),
            Some("127.0.0.1:1935".parse().unwrap())
        );
        assert_eq!(
            relay_addr(&cfg.pusher_input),
            relay_addr(&cfg.encoder_output)
        );
    }

    /// Anything this cannot honestly probe has to read as no answer, because
    /// the answer dims the control that puts a room on air.
    #[test]
    fn a_target_that_is_not_a_local_relay_is_not_probed() {
        for target in [
            // What the encode tests publish to.
            "/tmp/session/out.flv",
            "out.flv",
            // Off this machine, or not an address at all.
            "rtmp://10.0.0.4:1935/jamstream",
            "rtmp://relay.example.com:1935/jamstream",
            "rtmp://localhost:1935/jamstream",
            // Loopback with no port: nothing to connect to.
            "rtmp://127.0.0.1/jamstream",
            "",
            "rtmp://",
        ] {
            assert_eq!(relay_addr(target), None, "{target} was taken for a relay");
            assert!(RelayWatch::new(target, None).is_none());
        }
        // And the ones that are.
        assert_eq!(
            relay_addr("rtmp://127.0.0.1:1935/jamstream"),
            Some("127.0.0.1:1935".parse().unwrap())
        );
        assert_eq!(
            relay_addr("rtmp://[::1]:1935/jamstream"),
            Some("[::1]:1935".parse().unwrap())
        );
    }

    /// The case that happened: a relay that is listening, then is not. Driven
    /// against a real listener on a real port, because the whole point of the
    /// probe is that it observes the operating system rather than our opinion.
    #[tokio::test]
    async fn a_relay_that_goes_away_is_reported_missing() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a stand-in relay");
        let addr = listener.local_addr().unwrap();
        let mut watch = RelayWatch::new(&format!("rtmp://{addr}/jamstream"), None)
            .expect("a loopback relay url");

        assert_eq!(watch.observe(0).await, Some(BroadcastReadiness::Ready));
        // Not due again yet: the probe is a heartbeat, not a per-tick cost.
        assert_eq!(watch.observe(1_000).await, None);

        drop(listener);
        match watch.observe(6_000).await {
            Some(BroadcastReadiness::Unavailable { reason }) => {
                assert!(names_the_absence(&reason, addr), "{reason}");
            }
            other => panic!("a dead relay reported {other:?}"),
        }
    }

    /// A relay that never appears is what a failed tooling fetch looks like
    /// from here, and the note is how the host learns which of the two it is.
    #[tokio::test]
    async fn a_relay_that_never_appears_reports_the_note_cloud_init_left() {
        let dir = std::env::temp_dir().join(format!("jamstream-relaynote-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let note = dir.join("broadcast-unavailable");
        // A port nothing is on: bound, read, and dropped, so the number is
        // real and nothing is listening on it.
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let target = format!("rtmp://{addr}/jamstream");

        // No note: the absence is described in this process's own words.
        let mut watch = RelayWatch::new(&target, Some(note.clone()))
            .expect("a loopback relay url")
            .with_grace(Duration::ZERO);
        match watch.observe(0).await {
            Some(BroadcastReadiness::Unavailable { reason }) => {
                assert!(names_the_absence(&reason, addr), "{reason}");
            }
            other => panic!("nothing listening reported {other:?}"),
        }

        // With one, the host gets the sentence that names the cause. Written
        // after the watch was built, which is the real order: the fetch runs
        // after this process is up.
        std::fs::write(&note, "the broadcast tooling could not be downloaded\n").unwrap();
        assert_eq!(
            watch.observe(60_000).await,
            Some(BroadcastReadiness::Unavailable {
                reason: "the broadcast tooling could not be downloaded".to_owned()
            })
        );

        // An empty note is no note, not an empty reason.
        std::fs::write(&note, "\n\n").unwrap();
        match watch.observe(120_000).await {
            Some(BroadcastReadiness::Unavailable { reason }) => assert!(!reason.is_empty()),
            other => panic!("an empty note reported {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The relay is downloaded and started after this process, so its absence
    /// at the first probe is not news. Reporting it would be wrong in the
    /// direction that costs a host a broadcast they actually have.
    ///
    /// Short grace, not the shipped one: every probe is a real connect that
    /// Windows spends `CONNECT_TIMEOUT` refusing, so 180 s costs 36 of them.
    #[tokio::test]
    async fn a_relay_that_has_not_arrived_yet_is_not_reported_missing() {
        const GRACE: Duration = Duration::from_secs(15);
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut watch = RelayWatch::new(&format!("rtmp://{addr}/jamstream"), None)
            .expect("a relay url")
            .with_grace(GRACE);
        let mut now = 0;
        while now < GRACE.as_millis() as u64 {
            assert_eq!(
                watch.observe(now).await,
                None,
                "reported missing at {now} ms"
            );
            now += PROBE_PERIOD.as_millis() as u64;
        }
        // Past the window it is an absence worth reporting.
        assert!(matches!(
            watch.observe(now).await,
            Some(BroadcastReadiness::Unavailable { .. })
        ));
    }

    /// Holds the shipped grace, which the test above overrides.
    #[test]
    fn the_shipped_grace_is_three_minutes() {
        assert_eq!(FIRST_SIGHTING_GRACE, Duration::from_secs(180));
        let watch = RelayWatch::new("rtmp://127.0.0.1:1935/jamstream", None).expect("a relay url");
        assert_eq!(watch.grace, FIRST_SIGHTING_GRACE);
    }

    /// A reason from a note file is a line someone else wrote, so it takes the
    /// same budget every other reason on this wire takes.
    #[tokio::test]
    async fn an_absurd_note_cannot_grow_past_the_reason_budget() {
        use jamstream_protocol::control::STREAM_REASON_BUDGET;

        let dir = std::env::temp_dir().join(format!("jamstream-relaylong-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let note = dir.join("broadcast-unavailable");
        std::fs::write(&note, "x".repeat(4_000)).unwrap();
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut watch = RelayWatch::new(&format!("rtmp://{addr}/jamstream"), Some(note))
            .expect("a relay url")
            .with_grace(Duration::ZERO);
        match watch.observe(0).await {
            Some(BroadcastReadiness::Unavailable { reason }) => {
                assert_eq!(reason.len(), STREAM_REASON_BUDGET);
            }
            other => panic!("expected an unavailable reading, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
