//! End-to-end for local session mode: LocalProvider spawns the real
//! jamstreamd binary (CARGO_BIN_EXE_jamstreamd), a genuine ClientCore joins
//! it over loopback UDP, destroy kills it, and the on-disk registry
//! survives a provider restart (the sweeper story). Plus the idle-exit dead
//! man's switch: an unjoined server with --idle-exit-min set exits on its
//! own.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use jamstream_cloud::providers::local::LocalProvider;
use jamstream_cloud::{BootConfig, InstanceClass, LaunchSpec, Provider, SelfDestruct, session_tag};
use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_session::client::{ClientCore, ClientEvent};
use tokio::net::UdpSocket;

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jamstream-localmode-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn server_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_jamstreamd"))
}

/// Bind-then-drop; racy in principle, unique enough in practice.
fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct SessionMaterial {
    issuer: Issuer,
    server_public: [u8; 32],
    session_id: SessionId,
    port: u16,
    flat_config: String,
}

fn session_material(idle_shutdown_min: u32) -> SessionMaterial {
    let issuer = Issuer::generate();
    let server_keys = generate_keypair();
    let session_id = SessionId::generate();
    let port = free_udp_port();
    let cfg = BootConfig {
        // Artifact fields feed the cloud bootstrap script only; they do
        // not appear in the flat config the local provider consumes.
        artifact_url: "unused-local".to_owned(),
        artifact_sha256: "unused-local".to_owned(),
        server_private_key_b64: data_encoding::BASE64.encode(&server_keys.private),
        issuer_public_key_b64: data_encoding::BASE64.encode(&issuer.public_key().to_bytes()),
        session_id_hex: data_encoding::HEXLOWER.encode(&session_id.0),
        port,
        idle_shutdown_min,
        max_duration_min: 720,
        self_destruct: SelfDestruct::AwsShutdown,
    };
    SessionMaterial {
        server_public: server_keys.public,
        flat_config: cfg.render_flat_config(),
        issuer,
        session_id,
        port,
    }
}

fn launch_spec(
    provider: &LocalProvider,
    mat: &SessionMaterial,
    session_tag_id: &str,
) -> LaunchSpec {
    LaunchSpec {
        region: provider.regions().remove(0),
        instance_class: InstanceClass::Small,
        user_data: mat.flat_config.clone(),
        tags: vec![session_tag(session_tag_id)],
    }
}

#[tokio::test]
async fn launch_join_destroy_end_to_end() {
    let dir = temp_dir("e2e");
    let provider = LocalProvider::new(dir.clone()).with_server_binary(server_binary());
    let mat = session_material(10);

    let instance = provider
        .launch(launch_spec(&provider, &mat, "e2e-session"))
        .await
        .expect("launch");
    assert!(instance.public_ip.is_some(), "instance must carry an ip");
    assert_eq!(instance.session_id(), Some("e2e-session"));

    // A real client joins through loopback; the invite address is local
    // regardless of the LAN ip the instance advertises.
    let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), mat.port);
    let invite = mat.issuer.mint(
        mat.session_id,
        vec![server_addr],
        mat.server_public,
        Token {
            member_id: MemberId(1),
            role: Role::Musician,
            name_hint: Some("local-e2e".into()),
            expires_unix: u64::MAX,
            jti: TokenId::generate(),
        },
    );

    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    socket.connect(server_addr).await.unwrap();
    let (mut client, first) = ClientCore::connect(&invite, now()).unwrap();
    socket.send(&first).await.unwrap();

    let mut joined = false;
    let mut buf = [0u8; 2048];
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !joined {
        for pkt in client.poll(now()) {
            socket.send(&pkt).await.unwrap();
        }
        if let Ok(Ok(len)) =
            tokio::time::timeout(Duration::from_millis(20), socket.recv(&mut buf)).await
        {
            for pkt in client.handle_datagram(now(), &buf[..len]) {
                socket.send(&pkt).await.unwrap();
            }
        }
        for event in client.events() {
            if matches!(event, ClientEvent::Joined) {
                joined = true;
            }
        }
    }
    assert!(joined, "client never joined the locally spawned server");

    client.leave("local e2e done").unwrap();
    for pkt in client.poll(now()) {
        socket.send(&pkt).await.unwrap();
    }

    let killed_at = Instant::now();
    provider
        .destroy(&instance.region.id, &instance.id)
        .await
        .expect("destroy");
    assert!(
        killed_at.elapsed() < Duration::from_secs(5),
        "destroy took longer than the 5 s budget"
    );
    assert!(
        provider.list_tagged(None).await.unwrap().is_empty(),
        "destroyed session still listed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn registry_survives_provider_restart() {
    let dir = temp_dir("sweeper");
    let mat = session_material(10);
    let instance = {
        let provider = LocalProvider::new(dir.clone()).with_server_binary(server_binary());
        provider
            .launch(launch_spec(&provider, &mat, "sweep-session"))
            .await
            .expect("launch")
        // Provider handle drops here; the child keeps running.
    };

    // A fresh provider on the same state dir sees the session: the
    // registry is on disk, which is exactly what the sweeper relies on.
    let fresh = LocalProvider::new(dir.clone());
    let found = fresh.list_tagged(Some("sweep-session")).await.unwrap();
    assert_eq!(found.len(), 1, "restarted provider lost the session");
    assert_eq!(found[0].id, instance.id);

    fresh
        .destroy(&instance.region.id, &instance.id)
        .await
        .expect("destroy from fresh provider");
    assert!(fresh.list_tagged(None).await.unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// --idle-exit-min accepts fractional minutes precisely so this test does
/// not take a minute: 0.05 min = 3 s. Spawn, never join, and the server
/// must exit cleanly on its own.
#[test]
fn idle_exit_terminates_an_unjoined_server() {
    let dir = temp_dir("idle");
    let mat = session_material(10);
    let config_path = dir.join("config");
    std::fs::write(&config_path, &mat.flat_config).unwrap();

    let mut child = std::process::Command::new(server_binary())
        .arg("--config")
        .arg(&config_path)
        .arg("--activity-file")
        .arg(dir.join("last-active"))
        .arg("--idle-exit-min")
        .arg("0.05")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn jamstreamd");

    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("server never idle-exited within 15 s");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(status.success(), "idle exit must be a clean exit: {status}");
    let _ = std::fs::remove_dir_all(&dir);
}
