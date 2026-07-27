use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use jamstream_server::config::Config;
use jamstream_server::revocations::Revocations;
use jamstream_server::runtime::{Options, Server};
use jamstream_stream::pipeline::StreamConfig;

#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

const DEFAULT_CONFIG: &str = "/etc/jamstream/config";
const DEFAULT_ACTIVITY: &str = "/run/jamstream/last-active";
/// Revoked token ids, reloaded at startup. On the session VM this is tmpfs,
/// which is what the restart this defends against needs: `Restart=on-failure`
/// with `RestartSec=2` brings the process back, not the machine. A host who
/// wants revocation to survive a reboot too points `--revoked-file` at a real
/// filesystem.
const DEFAULT_REVOKED: &str = "/run/jamstream/revoked";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
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

    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), cfg.port);
    let opts = Options {
        bind,
        activity_path: Some(
            arg_value("--activity-file")
                .map_or_else(|| PathBuf::from(DEFAULT_ACTIVITY), PathBuf::from),
        ),
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
                // The broadcast card's title. A flag rather than a config key:
                // the wire protocol has no session name, and /etc/jamstream/config
                // is the provisioning contract, which no released host writes it
                // into yet.
                match arg_value("--session-name") {
                    Some(name) => server.with_stream_config(StreamConfig::new(name)),
                    None => server,
                }
            }
            Err(err) => {
                tracing::error!(%err, "bind failed");
                return ExitCode::FAILURE;
            }
        };
        tracing::info!(port = cfg.port, "session server up");
        match server.run(shutdown_signal()).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                tracing::error!(%err, "server error");
                ExitCode::FAILURE
            }
        }
    })
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

/// Parses a fractional-minutes window flag; an absent flag or 0 means
/// disabled (Duration::ZERO).
fn minutes_arg(flag: &str) -> Result<Duration, String> {
    match arg_value(flag).map(|v| v.parse::<f64>()) {
        None => Ok(Duration::ZERO),
        Some(Ok(min)) if min.is_finite() && min >= 0.0 => Ok(Duration::from_secs_f64(min * 60.0)),
        Some(_) => Err(format!("{flag} must be a nonnegative number of minutes")),
    }
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
