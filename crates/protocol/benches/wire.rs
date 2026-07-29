//! Wire, media, control and AEAD costs. Every number here is paid per
//! packet, and the mix tick sends one packet per connected member, so a
//! microsecond of it is a microsecond off the 2500 us tick budget.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use jamstream_protocol::control::{ControlLink, ControlMsg, MemberInfo};
use jamstream_protocol::ids::{MemberId, Role, SessionId, TokenId};
use jamstream_protocol::invite::{Invite, Issuer, Token};
use jamstream_protocol::media::{FrameDuration, MediaFrame};
use jamstream_protocol::transport::{
    Initiator, Keypair, Responder, Session, Welcome, generate_keypair,
};
use jamstream_protocol::wire::{self, Packet};

/// Typical stereo 2.5 ms Opus payload at 192 kbps: 60 bytes.
const MUSICIAN_PAYLOAD: usize = 60;
/// Stereo 20 ms at 96 kbps: 240 bytes.
const BROADCAST_PAYLOAD: usize = 240;

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + 7) as u8).collect()
}

fn invite() -> (Issuer, Keypair, Invite) {
    let issuer = Issuer::generate();
    let server = generate_keypair();
    let token = Token {
        member_id: MemberId(2),
        role: Role::Musician,
        name_hint: None,
        expires_unix: 4_000_000_000,
        jti: TokenId::generate(),
    };
    let invite = issuer.mint(
        SessionId::generate(),
        vec!["192.0.2.4:43210".parse().unwrap()],
        server.public,
        token,
    );
    (issuer, server, invite)
}

/// A live client/server transport pair. The Noise handshake runs here, in
/// setup, so none of it lands in a measurement: `Session` is long lived on
/// both sides and a member handshakes once per session.
fn session_pair() -> (Session, Session) {
    let (_issuer, server, invite) = invite();
    let (initiator, init_packet) = Initiator::new(&invite).expect("initiator");
    let Ok(Packet::HandshakeInit { version, noise }) = wire::parse(&init_packet) else {
        unreachable!("initiator produced an init packet")
    };
    let (hp, responder) =
        Responder::read_init(&server.private, &invite.session_id, version, noise).expect("read");
    let welcome = Welcome {
        member_id: hp.token.member_id,
        sample_clock: 0,
    };
    let (server_session, resp) = responder.respond(&welcome).expect("respond");
    let Ok(Packet::HandshakeResp { noise }) = wire::parse(&resp) else {
        unreachable!("responder produced a resp packet")
    };
    let (client_session, _) = initiator.finish(noise).expect("finish");
    (client_session, server_session)
}

fn media(c: &mut Criterion) {
    let mut g = c.benchmark_group("media");
    let small = payload(MUSICIAN_PAYLOAD);
    let large = payload(BROADCAST_PAYLOAD);

    let musician = MediaFrame {
        seq: 4_242,
        timestamp: 480_000,
        duration: FrameDuration::Ms2_5,
        stereo: false,
        payload: &small,
        redundant: None,
    };
    let redundant = MediaFrame {
        redundant: Some(&small),
        ..musician
    };
    let broadcast = MediaFrame {
        duration: FrameDuration::Ms20,
        stereo: true,
        payload: &large,
        ..musician
    };

    g.bench_function("build/musician_2_5ms", |b| {
        b.iter(|| black_box(black_box(&musician).encode()))
    });
    g.bench_function("build/musician_2_5ms_redundant", |b| {
        b.iter(|| black_box(black_box(&redundant).encode()))
    });
    g.bench_function("build/broadcast_20ms", |b| {
        b.iter(|| black_box(black_box(&broadcast).encode()))
    });

    let encoded = musician.encode();
    let encoded_redundant = redundant.encode();
    g.bench_function("parse/musician_2_5ms", |b| {
        b.iter(|| black_box(MediaFrame::decode(black_box(&encoded))))
    });
    g.bench_function("parse/musician_2_5ms_redundant", |b| {
        b.iter(|| black_box(MediaFrame::decode(black_box(&encoded_redundant))))
    });
    g.finish();
}

fn wire_bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("wire");
    let ciphertext = payload(MUSICIAN_PAYLOAD + 16);
    let transport = wire::build_transport(MemberId(3), 9_001, &ciphertext);
    let noise = payload(96);
    let init = wire::build_handshake_init(1, &noise);
    // A reject is keyed on the secret one handshake shares with the server,
    // so the key comes from an initiator rather than from an invite.
    let (initiator, _) = Initiator::new(&invite().2).expect("initiator");
    let reject_key = initiator.reject_key().expect("reject key").clone();

    g.bench_function("build/transport", |b| {
        b.iter(|| {
            black_box(wire::build_transport(
                MemberId(3),
                black_box(9_001),
                black_box(&ciphertext),
            ))
        })
    });
    g.bench_function("build/transport_header_into", |b| {
        let mut out = Vec::with_capacity(128);
        b.iter(|| {
            out.clear();
            wire::append_transport_header(MemberId(3), black_box(9_001), &mut out);
            black_box(&out);
        })
    });
    g.bench_function("build/version_reject", |b| {
        b.iter(|| {
            black_box(wire::build_version_reject(
                &reject_key,
                1,
                black_box(2),
                black_box(&init),
            ))
        })
    });
    g.bench_function("parse/transport", |b| {
        b.iter(|| black_box(wire::parse(black_box(&transport))))
    });
    g.bench_function("parse/handshake_init", |b| {
        b.iter(|| black_box(wire::parse(black_box(&init))))
    });
    g.finish();
}

fn roster(n: u16) -> Vec<MemberInfo> {
    (0..n)
        .map(|i| MemberInfo {
            id: MemberId(i),
            name: format!("musician {i}"),
            role: Role::Musician,
            connected: true,
            avatar_hash: Some([i as u8; 32]),
            quiet: false,
        })
        .collect()
}

fn control(c: &mut Criterion) {
    let mut g = c.benchmark_group("control");
    let chat = ControlMsg::Chat {
        from: MemberId(4),
        text: "the bridge again, from the top".to_string(),
    };
    let stats = ControlMsg::Stats {
        uplink_loss_pct: 0.4,
        uplink_jitter_depth: 3,
        uplink_recovered_pct: 0.1,
    };
    let roster_10 = ControlMsg::Roster(roster(10));
    let cases = [
        ("chat", &chat),
        ("stats", &stats),
        ("roster_10", &roster_10),
    ];

    for (name, msg) in cases {
        // poll() is what turns a queued message into bytes, so send plus poll
        // on a fresh link is the whole build path for one control datagram.
        g.bench_function(format!("build/{name}"), |b| {
            b.iter(|| {
                let mut link = ControlLink::new();
                link.send(black_box(msg.clone())).expect("send");
                black_box(link.poll(0))
            })
        });

        // Receive in the steady state: the link has already delivered frame
        // 0, so frame 1 is in order and delivers immediately. The link is
        // rebuilt per iteration because a second receive of the same seq is
        // a duplicate and takes a different, cheaper path.
        let mut sender = ControlLink::new();
        let mut frames = Vec::new();
        for _ in 0..2 {
            sender.send(msg.clone()).expect("send");
            frames.extend(sender.poll(0));
        }
        g.bench_function(format!("parse/{name}"), |b| {
            b.iter_batched_ref(
                || {
                    let mut link = ControlLink::new();
                    link.receive(&frames[0]).expect("first frame");
                    link
                },
                |link| black_box(link.receive(black_box(&frames[1]))),
                criterion::BatchSize::SmallInput,
            )
        });
    }
    g.finish();
}

fn aead(c: &mut Criterion) {
    let mut g = c.benchmark_group("aead");
    let frame = MediaFrame {
        seq: 1,
        timestamp: 0,
        duration: FrameDuration::Ms2_5,
        stereo: true,
        payload: &payload(MUSICIAN_PAYLOAD),
        redundant: None,
    }
    .encode();

    let (mut client, _server) = session_pair();
    g.bench_function("seal/media_frame", |b| {
        b.iter(|| black_box(client.seal(MemberId(2), black_box(&frame))))
    });

    let (mut client, _server) = session_pair();
    g.bench_function("seal_into/media_frame", |b| {
        let mut out = Vec::with_capacity(256);
        b.iter(|| {
            client
                .seal_into(MemberId(2), black_box(&frame), &mut out)
                .expect("seal");
            black_box(&out);
        })
    });

    // open() cannot be isolated the way seal() can: the replay window accepts
    // each counter exactly once, so a decrypt loop needs a fresh ciphertext
    // per iteration. Sealing one inside the measurement is the honest way to
    // get a monotonic counter, so this is the round trip and open's own cost
    // is this minus seal_into above.
    let (mut client, mut server) = session_pair();
    g.bench_function("seal_open_roundtrip/media_frame", |b| {
        let mut sealed = Vec::with_capacity(256);
        b.iter(|| {
            client
                .seal_into(MemberId(2), black_box(&frame), &mut sealed)
                .expect("seal");
            let Ok(Packet::Transport {
                counter,
                ciphertext,
                ..
            }) = wire::parse(&sealed)
            else {
                unreachable!("seal produces a transport packet")
            };
            black_box(server.open(counter, ciphertext).expect("open"));
        })
    });
    g.finish();
}

criterion_group!(benches, media, wire_bench, control, aead);
criterion_main!(benches);
