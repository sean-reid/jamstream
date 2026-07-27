//! Shared fixtures for the broadcast tests: procedural avatars, scene
//! construction, and snapshot comparison against committed baselines.

#![allow(dead_code)]

use std::io::Cursor;
use std::path::PathBuf;

use jamstream_broadcast::{AvatarImage, MemberVisual, Role, SceneConfig};

pub const W: u32 = 1280;
pub const H: u32 = 720;

pub fn config(session_name: &str) -> SceneConfig {
    SceneConfig::new(session_name)
}

/// Solid-color PNG bytes, the plainest possible avatar.
pub fn solid_png(rgb: [u8; 3], size: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(size, size, image::Rgba([rgb[0], rgb[1], rgb[2], 255]));
    encode_png(img)
}

/// A diagonal two-tone gradient, the closest thing to a real photo that
/// stays fully procedural.
pub fn gradient_png(size: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(size, size, |x, y| {
        let t = (x + y) as f32 / (2 * (size - 1)) as f32;
        let lerp = |a: f32, b: f32| (a + (b - a) * t) as u8;
        image::Rgba([lerp(180.0, 40.0), lerp(120.0, 60.0), lerp(60.0, 140.0), 255])
    });
    encode_png(img)
}

pub fn encode_png(img: image::RgbaImage) -> Vec<u8> {
    let mut bytes = Vec::new();
    img.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
        .expect("png encodes");
    bytes
}

pub fn musician(name: &str, avatar: Option<&[u8]>, peak: f32, rms: f32) -> MemberVisual {
    MemberVisual {
        name: name.to_owned(),
        avatar: avatar.map(|b| AvatarImage::from_bytes(b).expect("test avatar decodes")),
        level_peak: peak,
        level_rms: rms,
        connected: true,
        role: Role::Musician,
    }
}

/// A roster of n musicians with deterministic names, levels, and a mix of
/// avatars and initials discs.
pub fn roster(n: usize) -> Vec<MemberVisual> {
    let names = [
        "Ana Solari",
        "Ben Okafor",
        "Chiara Voss",
        "Dev Raman",
        "Eli Marsh",
        "Freya Lindqvist",
        "Goro Tanaka",
        "Hana Petrova",
        "Ivo Keller",
        "June Park",
    ];
    let gradient = gradient_png(96);
    (0..n)
        .map(|i| {
            let avatar = match i % 3 {
                0 => None,
                1 => Some(solid_png([40 + 20 * i as u8, 90, 120], 64)),
                _ => Some(gradient.clone()),
            };
            let peak = 0.15 + 0.08 * i as f32;
            musician(names[i], avatar.as_deref(), peak, peak * 0.6)
        })
        .collect()
}

fn previews_dir() -> PathBuf {
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    target.join("broadcast-previews")
}

fn baseline_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/snapshots/{name}.png"))
}

fn write_png(path: &PathBuf, rgba: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent dir")).expect("mkdir");
    let img = image::RgbaImage::from_raw(W, H, rgba.to_vec()).expect("frame dims");
    img.save(path).expect("png saves");
}

/// Compares `rgba` against tests/snapshots/{name}.png and always writes a
/// human-review copy under target/broadcast-previews/. Set
/// JAMSTREAM_UPDATE_SNAPSHOTS=1 to rewrite baselines.
pub fn assert_snapshot(name: &str, rgba: &[u8]) {
    let preview = previews_dir().join(format!("{name}.png"));
    write_png(&preview, rgba);
    println!("preview: {}", preview.display());

    let baseline = baseline_path(name);
    if std::env::var("JAMSTREAM_UPDATE_SNAPSHOTS").is_ok() {
        write_png(&baseline, rgba);
        return;
    }
    assert!(
        baseline.exists(),
        "missing baseline {}; run with JAMSTREAM_UPDATE_SNAPSHOTS=1 and commit it",
        baseline.display()
    );
    let expected = image::open(&baseline).expect("baseline decodes").to_rgba8();
    assert_eq!((expected.width(), expected.height()), (W, H), "{name} dims");
    assert!(
        expected.as_raw().as_slice() == rgba,
        "{name} differs from baseline {}; actual at {}",
        baseline.display(),
        preview.display()
    );
}
