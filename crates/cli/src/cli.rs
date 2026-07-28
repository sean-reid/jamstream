//! Argument surface. Doc comments double as --help text, so they stay
//! plain and specific.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
// Session shape, defined once for every surface; see
// jamstream_session::limits.
use jamstream_session::{
    DEFAULT_HOURS, DEFAULT_IDLE_MIN, DEFAULT_LISTENERS, DEFAULT_MAX_HOURS, DEFAULT_MUSICIANS,
    MAX_LISTENERS, MAX_MUSICIANS,
};

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
    /// Provider to host on: local, digitalocean, aws, or gcp.
    #[arg(long, default_value = "local")]
    pub provider: String,

    /// Region id to use, skipping the latency ranking.
    #[arg(long)]
    pub region: Option<String>,

    /// Musician seats in the session, counting you: 1 hosts alone, 4 mints
    /// your host invite plus 3 musician invites. The server admits this many
    /// musicians in total.
    #[arg(
        long,
        default_value_t = DEFAULT_MUSICIANS,
        value_parser = clap::value_parser!(u8).range(1..=MAX_MUSICIANS as i64),
    )]
    pub musicians: u8,

    /// Listener seats in the session; one listener invite is minted per seat.
    #[arg(
        long,
        default_value_t = DEFAULT_LISTENERS,
        value_parser = clap::value_parser!(u8).range(0..=MAX_LISTENERS as i64),
    )]
    pub listeners: u8,

    /// Expected session length in hours, for the cost preview.
    #[arg(long, default_value_t = DEFAULT_HOURS)]
    pub hours: f32,

    /// Stream destination count, for the egress estimate.
    #[arg(long, default_value_t = 0)]
    pub destinations: u8,

    /// UDP port the session server listens on.
    #[arg(long, default_value_t = 43210)]
    pub port: u16,

    /// Minutes without musicians before the server shuts itself down.
    #[arg(long = "idle-min", default_value_t = DEFAULT_IDLE_MIN)]
    pub idle_min: u32,

    /// Hard cap on session length in hours. Invites expire at the cap.
    #[arg(long = "max-hours", default_value_t = DEFAULT_MAX_HOURS)]
    pub max_hours: u32,

    /// Override the URL of the jamstreamd artifact the VM downloads at
    /// boot. Release builds pin the release's own server build; without
    /// that pin (a source build) cloud providers require this flag.
    #[arg(long = "artifact-url", requires = "artifact_sha256")]
    pub artifact_url: Option<String>,

    /// Override the expected sha256 of the jamstreamd artifact. Must be
    /// passed together with --artifact-url.
    #[arg(long = "artifact-sha256", requires = "artifact_url")]
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
    #[arg(long, default_value_t = DEFAULT_HOURS)]
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

    /// Test hook, host invite only: another member's invite whose token is
    /// revoked mid-session. Hidden; the desktop app owns interactive
    /// revocation.
    #[arg(long = "revoke-invite", hide = true, requires = "revoke_after_secs")]
    pub revoke_invite: Option<String>,

    /// Test hook: seconds after joining before --revoke-invite fires.
    #[arg(long = "revoke-after-secs", hide = true, requires = "revoke_invite")]
    pub revoke_after_secs: Option<u64>,
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
        // The defaults are the shared ones: the desktop wizard opens on the
        // same session shape.
        assert_eq!(args.provider, "local");
        assert_eq!(args.musicians, DEFAULT_MUSICIANS);
        assert_eq!(args.listeners, DEFAULT_LISTENERS);
        assert_eq!(args.hours, DEFAULT_HOURS);
        assert_eq!(args.destinations, 0);
        assert_eq!(args.port, 43210);
        assert_eq!(args.idle_min, DEFAULT_IDLE_MIN);
        assert_eq!(args.max_hours, DEFAULT_MAX_HOURS);
        assert!(!args.yes);
        assert!(!args.json);
    }

    /// The local provider spawns the server with these windows when the
    /// session config does not carry them, and it cannot see this crate's
    /// constants to check. Two numbers for one policy, so something has to
    /// hold them together, and this is the crate that sees both.
    #[test]
    fn the_local_providers_fallback_windows_match_the_documented_defaults() {
        assert_eq!(
            jamstream_cloud::providers::local::DEFAULT_IDLE_SHUTDOWN_MIN,
            DEFAULT_IDLE_MIN
        );
        assert_eq!(
            jamstream_cloud::providers::local::DEFAULT_MAX_DURATION_MIN,
            DEFAULT_MAX_HOURS * 60
        );
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

    // The mock provider stays accepted for tests but out of user surfaces:
    // help names the real providers and never mentions it.
    #[test]
    fn host_help_lists_local_and_hides_the_mock() {
        let mut cmd = Cli::command();
        let help = cmd
            .find_subcommand_mut("host")
            .expect("host subcommand")
            .render_long_help()
            .to_string();
        assert!(
            help.contains("local, digitalocean, aws, or gcp"),
            "help must name the real providers: {help}"
        );
        assert!(help.contains("[default: local]"), "help was: {help}");
        assert!(!help.contains("mock"), "help must not mention the mock");
    }

    // The artifact overrides only make sense as a pair: a URL with no
    // checksum could never be verified, a checksum with no URL checks
    // nothing.
    #[test]
    fn artifact_overrides_come_as_a_pair() {
        assert!(
            Cli::try_parse_from([
                "jamstream",
                "host",
                "--artifact-url",
                "https://x/jamstreamd"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["jamstream", "host", "--artifact-sha256", "abc"]).is_err());
    }

    // The flag ranges are the capacity the server enforces, not a second
    // opinion about it: one past the cap must be rejected at parse time, and
    // exactly the cap must be accepted.
    #[test]
    fn seat_counts_are_capped_at_the_server_capacity() {
        let over_musicians = (MAX_MUSICIANS + 1).to_string();
        let over_listeners = (MAX_LISTENERS + 1).to_string();
        assert!(
            Cli::try_parse_from(["jamstream", "host", "--musicians", &over_musicians]).is_err()
        );
        assert!(
            Cli::try_parse_from(["jamstream", "host", "--listeners", &over_listeners]).is_err()
        );
        // Zero musicians would be a session with no host in it.
        assert!(Cli::try_parse_from(["jamstream", "host", "--musicians", "0"]).is_err());

        let at_cap = MAX_MUSICIANS.to_string();
        let cli = Cli::parse_from(["jamstream", "host", "--musicians", &at_cap]);
        let Command::Host(args) = cli.command else {
            panic!("expected host");
        };
        assert_eq!(usize::from(args.musicians), MAX_MUSICIANS);
    }

    // --musicians counts the host, which is a change from an earlier build:
    // the help text has to say so, because the same number used to mean
    // guests only.
    #[test]
    fn musicians_help_says_the_host_is_counted() {
        let mut cmd = Cli::command();
        let help = cmd
            .find_subcommand_mut("host")
            .expect("host subcommand")
            .render_long_help()
            .to_string();
        assert!(
            help.contains("counting you"),
            "--musicians help must say the host is counted: {help}"
        );
        assert!(
            help.contains("1 hosts alone"),
            "--musicians help must explain the low end: {help}"
        );
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
        assert!(args.revoke_invite.is_none());
        assert!(args.revoke_after_secs.is_none());
    }

    #[test]
    fn revoke_hooks_come_as_a_pair() {
        let base = [
            "jamstream",
            "join",
            "blob",
            "--headless",
            "--input",
            "in.wav",
            "--output",
            "out.wav",
            "--duration-secs",
            "3",
        ];
        let mut with_both = base.to_vec();
        with_both.extend(["--revoke-invite", "other", "--revoke-after-secs", "2"]);
        let cli = Cli::parse_from(with_both);
        let Command::Join(args) = cli.command else {
            panic!("expected join");
        };
        assert_eq!(args.revoke_invite.as_deref(), Some("other"));
        assert_eq!(args.revoke_after_secs, Some(2));

        let mut only_invite = base.to_vec();
        only_invite.extend(["--revoke-invite", "other"]);
        assert!(Cli::try_parse_from(only_invite).is_err());

        let mut only_delay = base.to_vec();
        only_delay.extend(["--revoke-after-secs", "2"]);
        assert!(Cli::try_parse_from(only_delay).is_err());
    }
}
