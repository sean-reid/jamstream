use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;

use jamstream_server::config::Config;
use jamstream_server::runtime::{Options, Server};

#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

const DEFAULT_CONFIG: &str = "/etc/jamstream/config";
const DEFAULT_ACTIVITY: &str = "/run/jamstream/last-active";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config_path =
        arg_value("--config").map_or_else(|| PathBuf::from(DEFAULT_CONFIG), PathBuf::from);
    let cfg = match Config::load(&config_path) {
        Ok(cfg) => cfg,
        Err(err) => {
            tracing::error!(%err, "config rejected");
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
            Ok(server) => server,
            Err(err) => {
                tracing::error!(%err, "bind failed");
                return ExitCode::FAILURE;
            }
        };
        tracing::info!(port = cfg.port, "session server up");
        let shutdown = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        match server.run(shutdown).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                tracing::error!(%err, "server error");
                ExitCode::FAILURE
            }
        }
    })
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
