//! Broadcast copy of the dark palette in crates/client/src/theme.rs. The
//! stage is always dark; keep these bytes in lockstep with theme::DARK.

pub type Rgb = [u8; 3];

pub const WELL: Rgb = [0x0b, 0x0c, 0x0d];
pub const SURFACE0: Rgb = [0x12, 0x13, 0x14];
pub const SURFACE1: Rgb = [0x1b, 0x1d, 0x1f];
pub const TEXT_PRIMARY: Rgb = [0xe8, 0xea, 0xec];
pub const TEXT_MUTED: Rgb = [0x9e, 0xa4, 0xaa];
pub const ACCENT: Rgb = [0xf5, 0x92, 0x2b];
pub const METER_GREEN: Rgb = [0x40, 0xc0, 0x57];
pub const METER_AMBER: Rgb = [0xfa, 0xb0, 0x05];
pub const METER_RED: Rgb = [0xfa, 0x52, 0x52];
pub const BORDER: Rgb = [0x34, 0x37, 0x3b];

/// Linear blend of `b` into `a`; `t` in 0..1.
pub fn blend(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    [ch(a[0], b[0]), ch(a[1], b[1]), ch(a[2], b[2])]
}

/// Initials-disc fill: hue hashed from the name, desaturated so identity
/// color never competes with meter color. Deterministic by construction.
/// The 240..300 band is skipped; purple-indigo is banned by the design
/// language even at this saturation.
pub fn disc_color(name: &str) -> Rgb {
    // Skip 210 through 330: at this saturation even allowed blues read
    // as the banned indigo, so the whole blue-to-purple arc goes.
    let mut hue = (fnv1a(name.as_bytes()) % 240) as f32;
    if hue >= 210.0 {
        hue += 120.0;
    }
    hsl_to_rgb(hue, 0.30, 0.32)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> Rgb {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let u = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    [u(r), u(g), u(b)]
}
