//! A line the session server writes, all the way to a file on the host's
//! machine.
//!
//! Both halves of #438 in one run, with nothing standing in for anything: the
//! server's log subscriber, the redactor, the ring, `ServerCore`, the sealed
//! control link, `ClientCore`, the app's subscriber, and the file it writes.
//! The only thing this stands in for is the line's author, because a test
//! cannot make a real pusher fail on demand, and `tracing::warn!` is exactly
//! what the code that reports one does.
//!
//! The two processes are one process here, which is why the server's layer
//! carries a target filter: without it the line the host's client reports would
//! be captured by the server's own subscriber and sent back around forever.

use std::net::SocketAddr;
use std::path::PathBuf;

use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_session::client::{ClientCore, ClientState};
use jamstream_session::{ServerConfig, ServerCore};
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::util::SubscriberInitExt as _;

/// The target the session server's own log statements land on in this test.
const SERVER_STDERR: &str = "session_server";

/// A stream key, in the shape ffmpeg prints it: inside the ingest URL.
const KEY: &str = "live_1234_abcdefSECRET";

/// One host talking to one session server, no sockets and no clock.
struct Session {
    server: ServerCore,
    client: ClientCore,
    addr: SocketAddr,
    now_ms: u64,
}

impl Session {
    fn start() -> (Session, SessionId) {
        let kp = generate_keypair();
        let issuer = Issuer::generate();
        let session_id = SessionId::generate();
        let addr: SocketAddr = "10.0.0.10:5000".parse().expect("address");
        let server = ServerCore::new(ServerConfig::new(
            session_id,
            kp.private.to_vec(),
            kp.public,
            issuer.public_key(),
        ));
        let invite: Invite = issuer.mint(
            session_id,
            vec!["10.0.0.1:43210".parse().expect("address")],
            kp.public,
            Token {
                // The host holds member 0, and the log goes to the host alone.
                member_id: MemberId(0),
                role: Role::Musician,
                name_hint: None,
                expires_unix: u64::MAX,
                jti: TokenId::generate(),
            },
        );
        let (client, init) = ClientCore::connect(&invite, 0).expect("connect");
        let mut session = Session {
            server,
            client,
            addr,
            now_ms: 0,
        };
        session.deliver(vec![init]);
        session.run(200);
        assert_eq!(*session.client.state(), ClientState::Joined);
        (session, session_id)
    }

    /// Hands datagrams to the server and everything it answers back to the
    /// client, until neither has anything more to say.
    fn deliver(&mut self, mut from_client: Vec<Vec<u8>>) {
        while !from_client.is_empty() {
            let mut to_client = Vec::new();
            for dg in from_client.drain(..) {
                to_client.extend(
                    self.server
                        .handle_datagram(self.now_ms, 1_000, self.addr, &dg),
                );
            }
            for (_, dg) in to_client {
                from_client.extend(self.client.handle_datagram(self.now_ms, &dg));
            }
        }
    }

    fn run(&mut self, ms: u64) {
        for _ in 0..(ms * 2 / 5) {
            self.now_ms += 2;
            let ticked = self.server.tick(self.now_ms);
            let mut from_client = self.client.poll(self.now_ms);
            for (_, dg) in ticked {
                from_client.extend(self.client.handle_datagram(self.now_ms, &dg));
            }
            self.deliver(from_client);
            let _ = self.client.events();
        }
    }
}

/// A directory of this test's own, standing in for the host's state directory.
fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jamstream-server-log-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // SAFETY: nextest gives every test its own process, and this runs before
    // anything resolves the state directory.
    unsafe { std::env::set_var(jamstream_cli::state::STATE_DIR_ENV, &dir) };
    dir
}

#[test]
fn a_failure_on_the_session_machine_ends_up_readable_on_the_hosts() {
    let dir = scratch();
    jamstream_server::logtail::layer::<tracing_subscriber::Registry>()
        .expect("install the ring")
        .with_filter(Targets::new().with_target(SERVER_STDERR, LevelFilter::TRACE))
        .and_then(jamstream_client::server_log::layer())
        .with_subscriber(tracing_subscriber::registry())
        .init();

    let (mut session, session_id) = Session::start();
    // What the pusher's stderr reader logs when a broadcast fails, which is
    // the failure this whole path exists for.
    tracing::warn!(
        target: SERVER_STDERR,
        child = "youtube",
        "[flv @ 0x1] Failed to connect to rtmps://a.rtmp.youtube.com/live2/{KEY}: \
         Connection refused"
    );
    tracing::info!(target: SERVER_STDERR, "pusher exited with status 145");
    session.run(500);

    let path = dir.join("logs").join(format!("{}.log", session_id.hex()));
    let text = std::fs::read_to_string(&path).expect("the host has no copy of the server's log");
    // The diagnosis, which is the entire reason to keep the line.
    assert!(
        text.contains("Failed to connect to rtmps://<redacted>"),
        "{text}"
    );
    assert!(text.contains("Connection refused"), "{text}");
    assert!(text.contains("pusher exited with status 145"), "{text}");
    // And not the key, nor the ingest URL it was embedded in.
    assert!(!text.contains(KEY), "{text}");
    assert!(!text.contains("SECRET"), "{text}");
    assert!(!text.contains("rtmp.youtube.com"), "{text}");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).expect("metadata").permissions();
        assert_eq!(mode.mode() & 0o777, 0o600, "the host's copy is readable");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
