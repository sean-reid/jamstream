use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use jamstream_cloud::cloudinit::{
    ACTIVITY_FILE, BROADCAST_NOTE_FILE, RECORDING_CONFIG_PATH, RecordingStorage, SERVER_CONFIG_PATH,
};
use jamstream_server::config::Config;
use jamstream_server::revocations::Revocations;
use jamstream_server::runtime::{Options, RecordingOptions, Server};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Both paths come from the crate that writes them into the VM. The unit
/// passes `--config`, but nothing passes `--activity-file`, so this default is
/// the only thing that puts the file jamstreamd touches and the file the guard
/// stats in the same place. Two spellings is a dead man's switch that destroys
/// a live session: the guard reads an mtime that never changes.
const DEFAULT_CONFIG: &str = SERVER_CONFIG_PATH;
const DEFAULT_ACTIVITY: &str = ACTIVITY_FILE;
/// Revoked token ids, reloaded at startup. On the session VM this is tmpfs,
/// which is what the restart this defends against needs: `Restart=on-failure`
/// with `RestartSec=2` brings the process back, not the machine. A host who
/// wants revocation to survive a reboot too points `--revoked-file` at a real
/// filesystem.
const DEFAULT_REVOKED: &str = "/run/jamstream/revoked";

fn main() -> ExitCode {
    // The VM bootstrap runs `jamstreamd --version` once before enabling the
    // unit, proving the binary executes on this machine at all; answer
    // before anything touches the config so it works on a bare box.
    if version_requested(std::env::args()) {
        println!("jamstreamd {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(log_filter(std::env::var("RUST_LOG").ok().as_deref()))
        .init();
    install_panic_hook();

    let config_path =
        arg_value("--config").map_or_else(|| PathBuf::from(DEFAULT_CONFIG), PathBuf::from);
    let cfg = match Config::load(&config_path) {
        Ok(cfg) => cfg,
        Err(err) => {
            tracing::error!(%err, "config rejected");
            return ExitCode::FAILURE;
        }
    };

    // Both self-exit windows take fractional minutes (0.05 = 3 s) so tests
    // and impatient hosts get short windows; 0 (the default) disables.
    // Local mode passes these; cloud deployments rely on the external
    // guard instead.
    let idle_exit = match minutes_arg("--idle-exit-min") {
        Ok(window) => window,
        Err(err) => {
            tracing::error!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let max_duration = match minutes_arg("--max-duration-min") {
        Ok(window) => window,
        Err(err) => {
            tracing::error!("{err}");
            return ExitCode::FAILURE;
        }
    };

    let bind = match bind_arg(cfg.port) {
        Ok(bind) => bind,
        Err(err) => {
            tracing::error!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let opts = Options {
        bind,
        activity_path: Some(
            arg_value("--activity-file")
                .map_or_else(|| PathBuf::from(DEFAULT_ACTIVITY), PathBuf::from),
        ),
        // Recording is off unless a launcher configured it; a record
        // request without configuration fails visibly in the session.
        // Local mode names a directory with --record-dir; a cloud launch
        // writes the storage config beside the server config, and its
        // presence is what turns cloud recording on.
        recording: match recording_options() {
            Ok(recording) => recording,
            Err(err) => {
                tracing::error!(%err, "recording config rejected");
                return ExitCode::FAILURE;
            }
        },
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            tracing::error!(%err, "tokio runtime failed");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(async {
        let server = match Server::bind(&cfg, opts).await {
            Ok(server) => {
                let mut server = server
                    .with_idle_exit(idle_exit)
                    .with_max_duration(max_duration)
                    // Unconditional: a local session simply has no such file,
                    // and the relay probe answers on its own there.
                    .with_broadcast_note(PathBuf::from(BROADCAST_NOTE_FILE))
                    .with_revocations(Revocations::new(
                        arg_value("--revoked-file")
                            .map_or_else(|| PathBuf::from(DEFAULT_REVOKED), PathBuf::from),
                    ));
                // Only when asked: the sentinel is how the local provider
                // stops a server on Windows, and creating the marker for it
                // on a cloud VM would be a file nobody reads.
                if let Some(path) = arg_value("--shutdown-file") {
                    server = server.with_shutdown_file(PathBuf::from(path));
                }
                server
            }
            Err(err) => {
                tracing::error!(%err, address = %bind, hint = bind_hint(&err), "cannot listen");
                return ExitCode::FAILURE;
            }
        };
        tracing::info!(address = %bind, "session server up");
        match server.run(shutdown_signal()).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                tracing::error!(%err, "server error");
                ExitCode::FAILURE
            }
        }
    })
}

/// `RUST_LOG` target and level directives, or info when the variable is
/// unset or does not parse. `Targets` rather than `EnvFilter`: same syntax
/// for everything an operator writes here, without the regex engine that
/// only EnvFilter's unused span-field grammar needs (#298).
fn log_filter(rust_log: Option<&str>) -> Targets {
    rust_log
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| Targets::new().with_default(LevelFilter::INFO))
}

/// Routes panics through tracing so they land in the journal with the same
/// structure as everything else. Worth doing because the release profile sets
/// `strip = true`, which leaves a production backtrace close to useless: the
/// payload and the source location are what a bug report has to work from.
/// The hook runs before the unwind, so it fires even for the panics the
/// runtime catches and recovers from.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map_or_else(|| "unknown".to_owned(), ToString::to_string);
        tracing::error!(location = %location, payload = panic_payload(info), "panic");
    }));
}

/// The panic message, for the two payload types `panic!` actually produces.
fn panic_payload<'a>(info: &'a std::panic::PanicHookInfo<'_>) -> &'a str {
    let payload = info.payload();
    if let Some(s) = payload.downcast_ref::<&str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s
    } else {
        "non-string panic payload"
    }
}

/// Resolves on the first signal that means stop. SIGTERM matters more than
/// SIGINT here: systemd stop sends it, local-mode teardown sends it before
/// SIGKILL five seconds later, and the cloud self-destruct sends it. Its
/// default disposition is to terminate the process outright, so without this
/// the shutdown future never resolved and every member found out by timeout.
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(err) => {
            tracing::warn!(%err, "cannot listen for SIGTERM");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM"),
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT"),
    }
}

/// Windows has no cross-process SIGTERM for a console process, which is why
/// the local provider stops a server there with `--shutdown-file` instead.
/// See the comment on `request_graceful_shutdown` in the cloud crate.
#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Which address to listen on, from `--bind`. The port always comes from the
/// config: it is the provisioning contract, and the invites already name it.
///
/// The default is every interface, and it has to stay that way. A session is
/// reachable from the LAN and from the internet, so a cloud VM that bound
/// loopback would serve nobody.
///
/// The flag exists for the other case. The macOS Application Firewall
/// filters incoming connections per binary and does not govern loopback, so
/// a freshly built jamstreamd binding every interface raises a dialog, and
/// on a managed Mac that dialog cannot be pre-answered from the command
/// line. Every rebuild is a new binary and therefore a new dialog. Tests
/// that spawn a real server pass `--bind 127.0.0.1` and never meet it; a
/// host jamming alone on a locked-down laptop can do the same.
fn bind_arg(port: u16) -> Result<SocketAddr, String> {
    parse_bind(arg_value("--bind").as_deref(), port)
}

fn parse_bind(value: Option<&str>, port: u16) -> Result<SocketAddr, String> {
    let Some(value) = value else {
        return Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port));
    };
    value
        .parse::<IpAddr>()
        .map(|ip| SocketAddr::new(ip, port))
        .map_err(|_| format!("--bind must be an IP address, not {value:?}"))
}

/// What a host can do about a bind that failed. Worth saying out loud: the
/// symptom everyone else sees is a handshake that never completes, which
/// names the wrong layer entirely.
fn bind_hint(err: &std::io::Error) -> &'static str {
    match err.kind() {
        std::io::ErrorKind::AddrInUse => {
            "another process already holds this port; a leftover jamstreamd is the usual one, \
             so check for one and end its session or pick another port"
        }
        std::io::ErrorKind::AddrNotAvailable => {
            "no interface on this machine has that address; --bind takes one of this machine's \
             own addresses, or 0.0.0.0 for all of them"
        }
        std::io::ErrorKind::PermissionDenied => {
            "the OS refused the port; ports below 1024 need privileges, and a local firewall \
             can refuse one outright"
        }
        _ => "the socket could not be opened; nothing will reach this session until it can",
    }
}

/// Parses a fractional-minutes window flag; an absent flag or 0 means
/// disabled (Duration::ZERO).
fn minutes_arg(flag: &str) -> Result<Duration, String> {
    match arg_value(flag).map(|v| v.parse::<f64>()) {
        None => Ok(Duration::ZERO),
        Some(Ok(min)) if min.is_finite() && min >= 0.0 => Ok(Duration::from_secs_f64(min * 60.0)),
        Some(_) => Err(format!("{flag} must be a nonnegative number of minutes")),
    }
}

/// True when any argument is `--version`; the flag takes no value.
fn version_requested(mut args: impl Iterator<Item = String>) -> bool {
    args.any(|arg| arg == "--version")
}

fn arg_value(flag: &str) -> Option<String> {
    let mut args = std::env::args();
    while let Some(arg) = args.next() {
        if arg == flag {
            return args.next();
        }
    }
    None
}

fn has_flag(flag: &str) -> bool {
    std::env::args().any(|arg| arg == flag)
}

/// Disk when --record-dir names a directory, cloud when the launch wrote a
/// storage config, off when neither. A config that exists but does not
/// parse is an error, not silently-off: the host paid for recording.
fn recording_options() -> Result<Option<RecordingOptions>, String> {
    if let Some(dir) = arg_value("--record-dir") {
        return Ok(Some(RecordingOptions::Disk {
            dir: PathBuf::from(dir),
            stems: has_flag("--record-stems"),
        }));
    }
    let path = arg_value("--record-config")
        .map_or_else(|| PathBuf::from(RECORDING_CONFIG_PATH), PathBuf::from);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let storage = RecordingStorage::parse_flat_config(&text)?;
    Ok(Some(RecordingOptions::Cloud { storage }))
}

#[cfg(test)]
mod tests {
    use super::{log_filter, parse_bind, version_requested};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use tracing::Level;

    /// #298 moved the filter from `EnvFilter` to `Targets` to drop the
    /// regex engine. `RUST_LOG` target directives and the unset info
    /// default are the contract that must survive the swap.
    #[test]
    fn log_filter_honors_rust_log_and_defaults_to_info() {
        let unset = log_filter(None);
        assert!(unset.would_enable("jamstream_session", &Level::INFO));
        assert!(!unset.would_enable("jamstream_session", &Level::DEBUG));

        let directives = log_filter(Some("jamstream_session=debug,hyper=error"));
        assert!(directives.would_enable("jamstream_session", &Level::DEBUG));
        assert!(!directives.would_enable("hyper", &Level::WARN));

        // A value Targets cannot parse falls back to the default instead
        // of taking the process down before the subscriber exists, which
        // is what EnvFilter::try_from_default_env's error arm did.
        let garbage = log_filter(Some("jamstream_session=notalevel"));
        assert!(garbage.would_enable("jamstream_session", &Level::INFO));
        assert!(!garbage.would_enable("jamstream_session", &Level::DEBUG));
    }

    /// #139: the bootstrap execs `jamstreamd --version` before enabling the
    /// unit, so the flag must be recognized wherever it lands in argv and
    /// must never require a config file. The full no-config run is proven
    /// by the version integration test against the real binary.
    #[test]
    fn version_flag_is_found_anywhere_in_argv() {
        let args = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(version_requested(
            args(&["jamstreamd", "--version"]).into_iter()
        ));
        assert!(version_requested(
            args(&["jamstreamd", "--config", "/tmp/c", "--version"]).into_iter()
        ));
        assert!(!version_requested(args(&["jamstreamd"]).into_iter()));
        assert!(!version_requested(
            args(&["jamstreamd", "--config", "/tmp/c"]).into_iter()
        ));
    }

    /// The default has to keep being every interface: a session VM that
    /// bound loopback would serve nobody, and no released host passes the
    /// flag.
    #[test]
    fn no_flag_still_binds_every_interface() {
        assert_eq!(
            parse_bind(None, 43210).unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 43210)
        );
    }

    /// The reason the flag exists: loopback is the one address the macOS
    /// Application Firewall does not get a vote on.
    #[test]
    fn an_address_is_taken_with_the_config_port() {
        assert_eq!(
            parse_bind(Some("127.0.0.1"), 51205).unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 51205)
        );
        assert_eq!(
            parse_bind(Some("::1"), 51205).unwrap(),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 51205)
        );
        assert_eq!(
            parse_bind(Some("0.0.0.0"), 51205).unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 51205)
        );
    }

    /// The port comes from the config, so a value carrying one is a
    /// misunderstanding worth naming rather than half-honoring.
    #[test]
    fn a_value_that_is_not_an_address_is_refused_by_name() {
        for value in ["127.0.0.1:43210", "localhost", "", "0.0.0.0/0"] {
            let err = parse_bind(Some(value), 43210).unwrap_err();
            assert!(
                err.contains("IP address"),
                "{value:?} was rejected as {err}"
            );
            assert!(err.contains(value) || value.is_empty(), "{err}");
        }
    }
}
