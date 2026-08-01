//! Thin binary entry point: parse arguments, resolve providers, dispatch
//! into the library. All behavior lives in jamstream_cli so integration
//! tests exercise the same code.

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use jamstream_cli::cli::{Cli, Command, RecordingsCommand};
use jamstream_cli::storage::EnvStores;
use jamstream_cli::{CliError, end, host, join, providers, recordings, status, sweep};

fn main() -> ExitCode {
    // Warnings by default: with RUST_LOG unset, from_default_env dropped
    // every warn!, including the security-relevant Windows degradations
    // (icacls failed, directory ACL left inherited). RUST_LOG still wins.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
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

async fn dispatch<W: Write + Send>(cli: Cli, out: &mut W) -> Result<(), CliError> {
    match cli.command {
        Command::Host(args) => {
            let provider = providers::resolve_for_host(&args)?;
            host::run(&args, provider.as_ref(), out).await
        }
        Command::Status(args) => status::run(&args, providers::resolve, out).await,
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
        Command::Recordings(args) => match args.command {
            Some(RecordingsCommand::Get(get)) => {
                let mut ask = |out: &mut W| recordings::ask(out);
                let mut prompt = recordings::Prompt::stdin(&mut ask);
                recordings::get(&get, &EnvStores, &mut prompt, out).await
            }
            None => recordings::list(&args.list, &EnvStores, out).await,
        },
        Command::Completions(args) => {
            use clap::CommandFactory;
            clap_complete::generate(args.shell, &mut Cli::command(), "jamstream", out);
            Ok(())
        }
    }
}
