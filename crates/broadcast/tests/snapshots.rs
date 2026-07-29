//! Snapshot and determinism gates for the broadcast scene. Baselines live
//! in tests/snapshots/; every run writes review copies to
//! target/broadcast-previews/ and prints their paths.

mod common;

use common::{H, W, assert_snapshot, gradient_png, musician, roster, solid_png};
use jamstream_broadcast::{Renderer, SceneConfig};

fn frame() -> Vec<u8> {
    vec![0u8; (W * H * 4) as usize]
}

#[test]
fn one_musician_large() {
    let mut r = Renderer::new(SceneConfig::default());
    let members = vec![musician("Ana Solari", None, 0.62, 0.40)];
    let mut out = frame();
    r.render(0, &members, 0, &mut out);
    assert_snapshot("musicians_1", &out);
}

#[test]
fn two_musicians_with_avatars() {
    let mut r = Renderer::new(SceneConfig::default());
    let solid = solid_png([70, 110, 150], 64);
    let grad = gradient_png(96);
    let members = vec![
        musician("Ben Okafor", Some(&solid), 0.55, 0.35),
        musician("Chiara Voss", Some(&grad), 0.80, 0.50),
    ];
    let mut out = frame();
    r.render(0, &members, 0, &mut out);
    assert_snapshot("musicians_2_avatars", &out);
}

#[test]
fn four_musicians_mixed() {
    let mut r = Renderer::new(SceneConfig::default());
    let members = roster(4);
    let mut out = frame();
    r.render(0, &members, 0, &mut out);
    assert_snapshot("musicians_4", &out);
}

#[test]
fn six_musicians() {
    let mut r = Renderer::new(SceneConfig::default());
    // All initials discs: the no-avatar rendering at grid density.
    let members: Vec<_> = roster(6)
        .into_iter()
        .map(|mut m| {
            m.avatar = None;
            m
        })
        .collect();
    let mut out = frame();
    r.render(0, &members, 0, &mut out);
    assert_snapshot("musicians_6", &out);
}

#[test]
fn ten_musicians() {
    let mut r = Renderer::new(SceneConfig::default());
    let members = roster(10);
    let mut out = frame();
    r.render(0, &members, 0, &mut out);
    assert_snapshot("musicians_10", &out);
}

#[test]
fn long_names_ellipsize() {
    let mut r = Renderer::new(SceneConfig::default());
    let members = vec![
        musician("Bartholomew Featherstonehaugh-Cholmondeley", None, 0.5, 0.3),
        musician(
            "Dr. Maximiliana von Hohenzollern-Sigmaringen III",
            Some(&gradient_png(96)),
            0.7,
            0.45,
        ),
        musician("Jo", None, 0.4, 0.25),
    ];
    let mut out = frame();
    r.render(0, &members, 0, &mut out);
    assert_snapshot("long_names", &out);
}

#[test]
fn disconnected_member_dims() {
    let mut r = Renderer::new(SceneConfig::default());
    let mut members = roster(4);
    members[2].connected = false;
    members[2].level_peak = 0.0;
    members[2].level_rms = 0.0;
    let mut out = frame();
    r.render(0, &members, 0, &mut out);
    assert_snapshot("disconnected", &out);
}

#[test]
fn listener_line() {
    let mut r = Renderer::new(SceneConfig::default());
    let members = roster(3);
    let mut out = frame();
    r.render(0, &members, 12, &mut out);
    assert_snapshot("listeners", &out);
}

/// Feeds a peak transient at frame 0 and low level after: the hold segment
/// must stay put through frame 45, fade to 59, and the whole 60-frame run
/// must replay byte-identically from a fresh renderer.
#[test]
fn peak_hold_decay_is_deterministic() {
    let run = |grab: &[u64]| -> Vec<(u64, Vec<u8>)> {
        let mut r = Renderer::new(SceneConfig::default());
        let mut members = roster(2);
        let mut out = frame();
        let mut grabbed = Vec::new();
        for f in 0..60u64 {
            let (peak, rms) = if f == 0 { (0.95, 0.70) } else { (0.22, 0.15) };
            members[0].level_peak = peak;
            members[0].level_rms = rms;
            members[1].level_peak = peak * 0.8;
            members[1].level_rms = rms * 0.8;
            r.render(f, &members, 0, &mut out);
            if grab.contains(&f) {
                grabbed.push((f, out.clone()));
            }
        }
        grabbed
    };
    let grab = [0u64, 15, 45, 59];
    let first = run(&grab);
    let second = run(&grab);
    for ((fa, a), (fb, b)) in first.iter().zip(&second) {
        assert_eq!(fa, fb);
        assert!(a == b, "frame {fa} not reproducible across fresh renderers");
    }
    for (f, rgba) in &first {
        assert_snapshot(&format!("hold_f{f}"), rgba);
    }
}

/// Same inputs and frame index twice: byte-identical. Different frame
/// index: only meters move; the stage and footer bytes stay put.
#[test]
fn frame_index_moves_only_meters() {
    let mut r = Renderer::new(SceneConfig::default());
    let mut members = roster(4);
    let mut a = frame();
    let mut b = frame();

    r.render(7, &members, 5, &mut a);
    r.render(7, &members, 5, &mut b);
    assert!(a == b, "same frame index must be byte-identical");

    // Kick a hold at frame 10, then sample during hold and during fade.
    members[0].level_peak = 0.9;
    members[0].level_rms = 0.6;
    r.render(10, &members, 5, &mut a);
    members[0].level_peak = 0.2;
    members[0].level_rms = 0.12;
    for f in 11..=30 {
        r.render(f, &members, 5, &mut a);
    }
    r.render(31, &members, 5, &mut a);
    for f in 32..=58 {
        r.render(f, &members, 5, &mut b);
    }
    assert!(a != b, "hold fade must move meter pixels between frames");

    // Top margin rows and the footer band are static scene only.
    let row = (W * 4) as usize;
    assert!(
        a[..24 * row] == b[..24 * row],
        "stage bytes above the grid changed with frame_index"
    );
    let footer = (H as usize - 44) * row;
    assert!(
        a[footer..] == b[footer..],
        "footer bytes changed with frame_index"
    );
}
