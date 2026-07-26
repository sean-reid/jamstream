//! Socket, tick, and lifecycle driver for `ServerCore`. All session logic
//! lives in jamstream-session; this file owns exactly the things the core
//! must not: time, UDP, and the activity file the dead man's switch reads.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ed25519_dalek::VerifyingKey;
use jamstream_session::server::{ServerConfig, ServerCore, ServerEvent};
use tokio::net::UdpSocket;
use tokio::time::MissedTickBehavior;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::config::Config;

const TICK: Duration = Duration::from_micros(2_500);
const ACTIVITY_PERIOD: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct Options {
    pub bind: SocketAddr,
    /// Touched once a second while musicians are connected; the systemd
    /// guard treats staleness as idleness. None disables (tests, dev).
    pub activity_path: Option<PathBuf>,
}

pub struct Server {
    core: ServerCore,
    socket: UdpSocket,
    activity_path: Option<PathBuf>,
}

impl Server {
    pub async fn bind(cfg: &Config, opts: Options) -> io::Result<Server> {
        let private: [u8; 32] = cfg.server_private_key.as_slice().try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "server key must be 32 bytes")
        })?;
        let secret = StaticSecret::from(private);
        let server_public = PublicKey::from(&secret).to_bytes();
        let issuer_pk = VerifyingKey::from_bytes(&cfg.issuer_public_key)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "issuer key is not valid"))?;

        let core = ServerCore::new(ServerConfig {
            session_id: cfg.session_id,
            server_private: private.to_vec(),
            server_public,
            issuer_pk,
            max_musicians: 10,
            max_listeners: 20,
            member_timeout_ms: 10_000,
        });
        let socket = UdpSocket::bind(opts.bind).await?;
        Ok(Server {
            core,
            socket,
            activity_path: opts.activity_path,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Runs until `shutdown` resolves. Datagram handling, the 2.5 ms mix
    /// tick, and the activity heartbeat share one task; the core is not
    /// thread-safe and does not need to be.
    pub async fn run(mut self, shutdown: impl Future<Output = ()>) -> io::Result<()> {
        let start = Instant::now();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Burst);
        let mut heartbeat = tokio::time::interval(ACTIVITY_PERIOD);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut buf = [0u8; 2048];
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => break,
                _ = tick.tick() => {
                    let now_ms = start.elapsed().as_millis() as u64;
                    for (addr, pkt) in self.core.tick(now_ms) {
                        let _ = self.socket.send_to(&pkt, addr).await;
                    }
                    for event in self.core.events() {
                        log_event(&event);
                    }
                }
                _ = heartbeat.tick() => {
                    if self.core.musicians_connected() > 0 {
                        touch(self.activity_path.as_deref());
                    }
                }
                received = self.socket.recv_from(&mut buf) => {
                    let (len, src) = match received {
                        Ok(ok) => ok,
                        // Transient per-datagram errors (ICMP unreachable
                        // surfacing on some platforms) must not kill the
                        // session.
                        Err(err) => {
                            tracing::debug!(error = %err, "recv_from failed");
                            continue;
                        }
                    };
                    let now_ms = start.elapsed().as_millis() as u64;
                    let now_unix = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    for (addr, pkt) in self.core.handle_datagram(now_ms, now_unix, src, &buf[..len]) {
                        let _ = self.socket.send_to(&pkt, addr).await;
                    }
                }
            }
        }
        tracing::info!("shutting down");
        Ok(())
    }
}

fn log_event(event: &ServerEvent) {
    match event {
        ServerEvent::MusicianCountChanged(n) => tracing::info!(musicians = n, "occupancy"),
        ServerEvent::MemberJoined { id, name } => {
            tracing::info!(member = id.0, name = %name, "joined");
        }
        ServerEvent::MemberDisconnected { id } => tracing::info!(member = id.0, "disconnected"),
        ServerEvent::MemberRevoked { id } => tracing::info!(member = id.0, "revoked"),
        ServerEvent::ProtocolViolation { id, what } => {
            tracing::warn!(member = id.0, what, "protocol violation");
        }
    }
}

fn touch(path: Option<&std::path::Path>) {
    let Some(path) = path else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(err) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(SystemTime::now()))
    {
        tracing::warn!(error = %err, "cannot touch activity file");
    }
}
