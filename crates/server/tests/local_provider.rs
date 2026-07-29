//! End-to-end for local session mode: LocalProvider spawns the real
//! jamstreamd binary (CARGO_BIN_EXE_jamstreamd), a genuine ClientCore joins
//! it over loopback UDP, destroy kills it, and the on-disk registry
//! survives a provider restart (the sweeper story). Plus the two self-exit
//! windows: an unjoined server with --idle-exit-min set exits on its own,
//! and a server with --max-duration-min set exits at the cap even with a
//! connected, actively sending musician.

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use common::{BIND, ChildGuard, ReservedPort, budget, scratch_dir, server_binary};

use jamstream_cloud::providers::local::LocalProvider;
use jamstream_cloud::{BootConfig, InstanceClass, LaunchSpec, Provider, SelfDestruct, session_tag};
use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Issuer, Token};
use jamstream_protocol::transport::generate_keypair;
use jamstream_session::client::{ClientCore, ClientEvent};
use tokio::net::UdpSocket;

struct SessionMaterial {
    issuer: Issuer,
    server_public: [u8; 32],
    session_id: SessionId,
    reserved: ReservedPort,
    port: u16,
    flat_config: String,
}

fn session_material(idle_shutdown_min: u32) -> SessionMaterial {
    let issuer = Issuer::generate();
    let server_keys = generate_keypair();
    let session_id = SessionId::generate();
    let reserved = ReservedPort::reserve();
    let port = reserved.port;
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
        // Local sessions record to disk, never to a bucket.
        recording: None,
    };
    SessionMaterial {
        server_public: server_keys.public,
        flat_config: cfg.render_flat_config(),
        issuer,
        session_id,
        reserved,
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

/// Joins a real ClientCore to a locally spawned server over loopback and
/// returns it once Joined, together with its socket and the clock epoch
/// its `now_ms` values are measured from.
async fn join_musician(mat: &SessionMaterial, name: &str) -> (ClientCore, UdpSocket, Instant) {
    let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), mat.port);
    let invite = mat.issuer.mint(
        mat.session_id,
        vec![server_addr],
        mat.server_public,
        Token {
            member_id: MemberId(1),
            role: Role::Musician,
            name_hint: Some(name.into()),
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
    let deadline = Instant::now() + budget(Duration::from_secs(5));
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
    (client, socket, start)
}

#[tokio::test]
async fn launch_join_destroy_end_to_end() {
    let dir = scratch_dir("localmode-e2e");
    let provider = LocalProvider::new(dir.clone())
        .with_server_binary(server_binary())
        .with_bind(IpAddr::V4(BIND));
    let mut mat = session_material(10);

    mat.reserved.release();
    let instance = provider
        .launch(launch_spec(&provider, &mat, "e2e-session"))
        .await
        .expect("launch");
    assert!(instance.public_ip.is_some(), "instance must carry an ip");
    assert_eq!(instance.session_id(), Some("e2e-session"));

    // The provider must forward both self-exit windows from the flat
    // config to the spawned server's command line (session_material sets
    // idle_shutdown_min = 10 and max_duration_min = 720).
    #[cfg(unix)]
    {
        let out = std::process::Command::new("ps")
            .args(["-p", &instance.id, "-o", "command="])
            .output()
            .expect("ps");
        let cmdline = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(
            cmdline.contains("--idle-exit-min 10"),
            "idle window not forwarded: {cmdline}"
        );
        assert!(
            cmdline.contains("--max-duration-min 720"),
            "max duration not forwarded: {cmdline}"
        );
    }

    // A real client joins through loopback; the invite address is local
    // regardless of the LAN ip the instance advertises.
    let (mut client, socket, start) = join_musician(&mat, "local-e2e").await;
    let now = || start.elapsed().as_millis() as u64;

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
        killed_at.elapsed() < budget(Duration::from_secs(5)),
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
    let dir = scratch_dir("localmode-sweeper");
    let mut mat = session_material(10);
    mat.reserved.release();
    let instance = {
        let provider = LocalProvider::new(dir.clone())
            .with_server_binary(server_binary())
            .with_bind(IpAddr::V4(BIND));
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
    let dir = scratch_dir("localmode-idle");
    let mut mat = session_material(10);
    let config_path = dir.join("config");
    std::fs::write(&config_path, &mat.flat_config).unwrap();

    mat.reserved.release();
    let mut child = ChildGuard(
        std::process::Command::new(server_binary())
            .arg("--config")
            .arg(&config_path)
            .arg("--bind")
            .arg(BIND.to_string())
            .arg("--activity-file")
            .arg(dir.join("last-active"))
            .arg("--idle-exit-min")
            .arg("0.05")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn jamstreamd"),
    );

    let deadline = Instant::now() + budget(Duration::from_secs(15));
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "server never idle-exited within 15 s"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(status.success(), "idle exit must be a clean exit: {status}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The whole-session cap, unlike idle-exit, fires with musicians present:
/// a server spawned with --max-duration-min 0.05 (3 s) and a joined,
/// actively sending musician must still exit cleanly on its own, and the
/// client must learn the session is over. The config's 10 minute idle window
/// cannot be the cause, and no --idle-exit-min is passed.
///
/// The client now learns by Bye on the way out rather than by timeout ten
/// seconds later, so the event is watched for from the moment the cap could
/// fire; either way of finding out counts.
#[tokio::test]
async fn max_duration_ends_an_occupied_session() {
    let dir = scratch_dir("localmode-maxdur");
    let mut mat = session_material(10);
    let config_path = dir.join("config");
    std::fs::write(&config_path, &mat.flat_config).unwrap();

    mat.reserved.release();
    let mut child = ChildGuard(
        std::process::Command::new(server_binary())
            .arg("--config")
            .arg(&config_path)
            .arg("--bind")
            .arg(BIND.to_string())
            .arg("--activity-file")
            .arg(dir.join("last-active"))
            .arg("--max-duration-min")
            .arg("0.05")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn jamstreamd"),
    );

    let (mut client, socket, start) = join_musician(&mat, "capped").await;
    let now = || start.elapsed().as_millis() as u64;

    // Keep the musician pumping while the cap runs out; the server must
    // exit anyway, which is exactly what idle-exit would never do here.
    let mut buf = [0u8; 2048];
    let mut told = false;
    let deadline = Instant::now() + budget(Duration::from_secs(15));
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "server never hit the max-duration cap within 15 s"
        );
        for pkt in client.poll(now()) {
            let _ = socket.send(&pkt).await;
        }
        if let Ok(Ok(len)) =
            tokio::time::timeout(Duration::from_millis(20), socket.recv(&mut buf)).await
        {
            for pkt in client.handle_datagram(now(), &buf[..len]) {
                let _ = socket.send(&pkt).await;
            }
        }
        told |= client
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::TimedOut | ClientEvent::Ejected { .. }));
    };
    assert!(
        status.success(),
        "max-duration exit must be a clean exit: {status}"
    );

    // The Bye may still be in the socket buffer, and a client that missed it
    // falls back to its 10 s connection timeout.
    let deadline = Instant::now() + budget(Duration::from_secs(20));
    while Instant::now() < deadline && !told {
        for pkt in client.poll(now()) {
            let _ = socket.send(&pkt).await;
        }
        if let Ok(Ok(len)) =
            tokio::time::timeout(Duration::from_millis(20), socket.recv(&mut buf)).await
        {
            let _ = client.handle_datagram(now(), &buf[..len]);
        }
        told |= client
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::TimedOut | ClientEvent::Ejected { .. }));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(told, "client never observed the capped server going away");
    let _ = std::fs::remove_dir_all(&dir);
}
