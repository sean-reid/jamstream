//! Thin binary entry point: parse arguments, resolve providers, dispatch
//! into the library. All behavior lives in jamstream_cli so integration
//! tests exercise the same code.

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use jamstream_cli::cli::{Cli, Command};
use jamstream_cli::{CliError, end, host, join, providers, status, sweep};

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let mut out = std::io::stdout();
    match runtime.block_on(dispatch(cli, &mut out)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch<W: Write>(cli: Cli, out: &mut W) -> Result<(), CliError> {
    match cli.command {
        Command::Host(args) => {
            let provider = providers::resolve_for_port(&args.provider, args.port)?;
            host::run(&args, provider.as_ref(), out).await
        }
        Command::Status(args) => status::run(&args, out),
        Command::End(args) => {
            let (path, session) = end::select(&args)?;
            let provider = end::resolve_provider(&session)?;
            end::run(&path, session, provider.as_ref(), out).await
        }
        Command::Sweep(args) => {
            let providers = match &args.provider {
                Some(name) => vec![providers::resolve(name)?],
                None => providers::resolve_all(),
            };
            sweep::run(&providers, args.dry_run, out).await
        }
        Command::Join(args) => join::run(&args, out).await,
    }
}
