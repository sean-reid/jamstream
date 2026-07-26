//! Argument surface. Doc comments double as --help text, so they stay
//! plain and specific.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "jamstream",
    version,
    about = "Host and join JamStream sessions."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Provision a session server and mint invites.
    Host(HostArgs),
    /// List known sessions with elapsed time and accrued cost.
    Status(StatusArgs),
    /// Destroy a session's server and mark the session ended.
    End(EndArgs),
    /// Find and destroy orphaned jamstream instances.
    Sweep(SweepArgs),
    /// Join a session as a headless client.
    Join(JoinArgs),
}

#[derive(Debug, Args)]
pub struct HostArgs {
    /// Cloud provider to host on.
    #[arg(long, default_value = "mock")]
    pub provider: String,

    /// Region id to use, skipping the latency ranking.
    #[arg(long)]
    pub region: Option<String>,

    /// Musician invites to mint, not counting the host.
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u8).range(1..=10))]
    pub musicians: u8,

    /// Listener invites to mint.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=20))]
    pub listeners: u8,

    /// Expected session length in hours, for the cost preview.
    #[arg(long, default_value_t = 3.0)]
    pub hours: f32,

    /// Stream destination count, for the egress estimate.
    #[arg(long, default_value_t = 0)]
    pub destinations: u8,

    /// UDP port the session server listens on.
    #[arg(long, default_value_t = 43210)]
    pub port: u16,

    /// Minutes without musicians before the server shuts itself down.
    #[arg(long = "idle-min", default_value_t = 10)]
    pub idle_min: u32,

    /// Hard cap on session length in hours. Invites expire at the cap.
    #[arg(long = "max-hours", default_value_t = 12)]
    pub max_hours: u32,

    /// URL of the jamstreamd artifact the VM downloads at boot.
    #[arg(long = "artifact-url")]
    pub artifact_url: Option<String>,

    /// Expected sha256 of the jamstreamd artifact.
    #[arg(long = "artifact-sha256")]
    pub artifact_sha256: Option<String>,

    /// Skip the launch confirmation.
    #[arg(long)]
    pub yes: bool,

    /// Emit one JSON object instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Hours to project the total cost over.
    #[arg(long, default_value_t = 3.0)]
    pub hours: f32,

    /// Emit a JSON array instead of a table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct EndArgs {
    /// Session id prefix of the session to end.
    #[arg(required_unless_present = "last", conflicts_with = "last")]
    pub session: Option<String>,

    /// End the most recently created running session.
    #[arg(long)]
    pub last: bool,
}

#[derive(Debug, Args)]
pub struct SweepArgs {
    /// Report what would be destroyed without destroying anything.
    #[arg(long = "dry-run")]
    pub dry_run: bool,

    /// Sweep one provider instead of every configured provider.
    #[arg(long)]
    pub provider: Option<String>,
}

#[derive(Debug, Args)]
pub struct JoinArgs {
    /// Invite string, with or without the jamstream://join/ prefix.
    pub invite: String,

    /// Run without a UI. Required; the desktop app is the interactive client.
    #[arg(long)]
    pub headless: bool,

    /// 48 kHz mono or stereo WAV sent as the capture signal. Silence after
    /// the file ends.
    #[arg(long)]
    pub input: PathBuf,

    /// Output WAV path for the received stereo mix.
    #[arg(long)]
    pub output: PathBuf,

    /// Seconds to stay in the session after joining.
    #[arg(long = "duration-secs")]
    pub duration_secs: u64,

    /// Chat message to send once after joining.
    #[arg(long)]
    pub chat: Option<String>,

    /// Display name to request. Not sent yet; names come from the invite.
    #[arg(long)]
    pub name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use clap::Parser;

    #[test]
    fn command_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn host_defaults() {
        let cli = Cli::parse_from(["jamstream", "host"]);
        let Command::Host(args) = cli.command else {
            panic!("expected host");
        };
        assert_eq!(args.provider, "mock");
        assert_eq!(args.musicians, 4);
        assert_eq!(args.listeners, 0);
        assert_eq!(args.hours, 3.0);
        assert_eq!(args.destinations, 0);
        assert_eq!(args.port, 43210);
        assert_eq!(args.idle_min, 10);
        assert_eq!(args.max_hours, 12);
        assert!(!args.yes);
        assert!(!args.json);
    }

    #[test]
    fn host_flags_parse() {
        let cli = Cli::parse_from([
            "jamstream",
            "host",
            "--provider",
            "mock",
            "--region",
            "mock-west",
            "--musicians",
            "3",
            "--listeners",
            "5",
            "--hours",
            "1.5",
            "--destinations",
            "2",
            "--port",
            "50000",
            "--idle-min",
            "5",
            "--max-hours",
            "6",
            "--artifact-url",
            "https://example.com/jamstreamd",
            "--artifact-sha256",
            "abc",
            "--yes",
            "--json",
        ]);
        let Command::Host(args) = cli.command else {
            panic!("expected host");
        };
        assert_eq!(args.region.as_deref(), Some("mock-west"));
        assert_eq!(args.musicians, 3);
        assert_eq!(args.listeners, 5);
        assert_eq!(args.hours, 1.5);
        assert_eq!(args.destinations, 2);
        assert_eq!(args.port, 50000);
        assert_eq!(args.idle_min, 5);
        assert_eq!(args.max_hours, 6);
        assert!(args.yes && args.json);
    }

    #[test]
    fn musician_count_is_capped() {
        assert!(Cli::try_parse_from(["jamstream", "host", "--musicians", "11"]).is_err());
        assert!(Cli::try_parse_from(["jamstream", "host", "--listeners", "21"]).is_err());
    }

    #[test]
    fn end_requires_prefix_or_last() {
        assert!(Cli::try_parse_from(["jamstream", "end"]).is_err());
        assert!(Cli::try_parse_from(["jamstream", "end", "abcd", "--last"]).is_err());
        let cli = Cli::parse_from(["jamstream", "end", "--last"]);
        let Command::End(args) = cli.command else {
            panic!("expected end");
        };
        assert!(args.last);
        assert!(args.session.is_none());
    }

    #[test]
    fn sweep_and_status_parse() {
        let cli = Cli::parse_from(["jamstream", "sweep", "--dry-run", "--provider", "mock"]);
        let Command::Sweep(args) = cli.command else {
            panic!("expected sweep");
        };
        assert!(args.dry_run);
        assert_eq!(args.provider.as_deref(), Some("mock"));

        let cli = Cli::parse_from(["jamstream", "status", "--hours", "2", "--json"]);
        let Command::Status(args) = cli.command else {
            panic!("expected status");
        };
        assert_eq!(args.hours, 2.0);
        assert!(args.json);
    }

    #[test]
    fn join_parses() {
        let cli = Cli::parse_from([
            "jamstream",
            "join",
            "jamstream://join/blob",
            "--headless",
            "--input",
            "in.wav",
            "--output",
            "out.wav",
            "--duration-secs",
            "3",
            "--chat",
            "hi",
            "--name",
            "ana",
        ]);
        let Command::Join(args) = cli.command else {
            panic!("expected join");
        };
        assert!(args.headless);
        assert_eq!(args.duration_secs, 3);
        assert_eq!(args.chat.as_deref(), Some("hi"));
        assert_eq!(args.name.as_deref(), Some("ana"));
    }
}
