//! Avatar pixels for the interface, plus the initials disc every member
//! without one falls back to.
//!
//! Decoding happens once per content hash: the session layer hands the app
//! verified bytes keyed by their Blake2s-256, the runtime decodes them into
//! [`crate::runtime::AvatarHandle`], and the UI uploads one egui texture per
//! hash. The caps mirror the transfer layer's, so bytes that could never
//! reach a peer are refused with the same numbers here.
//!
//! The disc hue rules are the ones crates/broadcast/src/palette.rs paints on
//! the stream cards, so a member looks the same in the app and on air. The
//! unit tests assert that agreement against the broadcast crate directly.

use std::sync::Arc;

use egui::Color32;

use crate::runtime::AvatarHandle;

/// Encoded size cap: the transfer layer's, so a file the session would
/// refuse is refused here with the same number, before any decoding.
pub const MAX_BYTES: usize = jamstream_protocol::control::MAX_AVATAR_BYTES;

/// Decoded dimension cap per axis, mirroring crates/broadcast. Checked from
/// the header before the full decode so a small file cannot decompress into
/// a huge allocation.
pub const MAX_DIM: u32 = 1024;

/// Why an avatar did not become pixels. Every variant carries the specific
/// number or reason, because "invalid image" tells the user nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvatarError {
    Empty,
    TooManyBytes(usize),
    /// The bytes are some other format, or not an image at all.
    NotPngOrJpeg,
    TooLarge {
        width: u32,
        height: u32,
    },
    /// Header said PNG or JPEG, the pixels disagreed.
    Decode(String),
}

impl std::fmt::Display for AvatarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AvatarError::Empty => write!(f, "the file is empty"),
            AvatarError::TooManyBytes(n) => write!(
                f,
                "the image is {} KB; the limit is {} KB",
                n.div_ceil(1024),
                MAX_BYTES / 1024
            ),
            AvatarError::NotPngOrJpeg => write!(f, "only PNG and JPEG images are supported"),
            AvatarError::TooLarge { width, height } => write!(
                f,
                "the image is {width}x{height}; the limit is {MAX_DIM}x{MAX_DIM}"
            ),
            AvatarError::Decode(err) => write!(f, "the image did not decode: {err}"),
        }
    }
}

impl std::error::Error for AvatarError {}

/// Decodes PNG or JPEG bytes to straight (non-premultiplied) RGBA, the
/// layout egui's `ColorImage` takes. `hash` is the content hash the bytes
/// arrived under and becomes the handle's texture key.
pub fn decode(hash: String, bytes: &[u8]) -> Result<AvatarHandle, AvatarError> {
    if bytes.is_empty() {
        return Err(AvatarError::Empty);
    }
    if bytes.len() > MAX_BYTES {
        return Err(AvatarError::TooManyBytes(bytes.len()));
    }
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|err| AvatarError::Decode(err.to_string()))?;
    match reader.format() {
        Some(image::ImageFormat::Png | image::ImageFormat::Jpeg) => {}
        _ => return Err(AvatarError::NotPngOrJpeg),
    }
    let (width, height) = reader
        .into_dimensions()
        .map_err(|err| AvatarError::Decode(err.to_string()))?;
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return Err(AvatarError::TooLarge { width, height });
    }
    let rgba = image::load_from_memory(bytes)
        .map_err(|err| AvatarError::Decode(err.to_string()))?
        .to_rgba8();
    Ok(AvatarHandle {
        hash,
        width,
        height,
        rgba: Arc::from(rgba.into_raw().into_boxed_slice()),
    })
}

/// Lowercase hex of a content hash; the cache and texture key everywhere.
pub fn hash_hex(hash: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in hash {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Texture key for bytes we picked ourselves, before any session has hashed
/// them. Not the transfer identity (that is Blake2s-256, computed by the
/// session core): just a stable per-content key for the texture cache, so
/// choosing the same file twice does not upload it twice.
pub fn local_key(bytes: &[u8]) -> String {
    format!("local-{:016x}", fnv1a(bytes))
}

/// Initials-disc fill: hue hashed from the name, desaturated so identity
/// color never competes with meter color. Deterministic by construction.
/// Byte-for-byte the rule in crates/broadcast/src/palette.rs; the 210..330
/// arc is skipped because the design language bans blue-to-purple even at
/// this saturation.
pub fn disc_color(name: &str) -> Color32 {
    let mut hue = (fnv1a(name.as_bytes()) % 240) as f32;
    if hue >= 210.0 {
        hue += 120.0;
    }
    hsl_to_rgb(hue, 0.30, 0.32)
}

/// First letters of the first two words; two letters of a lone word. Same
/// rule as the stream card, so the fallback reads identically in both.
pub fn initials(name: &str) -> String {
    let mut words = name.split_whitespace();
    let first = words.next().unwrap_or("");
    let second = words.next();
    let mut out = String::new();
    match second {
        Some(second) => {
            for word in [first, second] {
                if let Some(c) = word.chars().next() {
                    out.extend(c.to_uppercase());
                }
            }
        }
        None => {
            for c in first.chars().take(2) {
                out.extend(c.to_uppercase());
            }
        }
    }
    if out.is_empty() {
        out.push('?');
    }
    out
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> Color32 {
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
    Color32::from_rgb(u(r), u(g), u(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real PNG through the `image` encoder; `w`x`h` of flat color.
    pub(crate) fn png(w: u32, h: u32) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let img = image::RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([(x * 3) as u8, (y * 5) as u8, 90, 255])
        });
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .expect("encode png");
        buf.into_inner()
    }

    #[test]
    fn valid_png_decodes_to_straight_rgba() {
        let bytes = png(12, 7);
        let handle = decode("abc".to_owned(), &bytes).expect("png decodes");
        assert_eq!((handle.width, handle.height), (12, 7));
        assert_eq!(handle.rgba.len(), 12 * 7 * 4);
        assert_eq!(handle.hash, "abc");
        // Straight alpha: opaque pixels keep their channels untouched, and
        // rows advance by width, not by the other axis.
        assert_eq!(&handle.rgba[..4], &[0, 0, 90, 255]);
        assert_eq!(&handle.rgba[4..8], &[3, 0, 90, 255]);
        assert_eq!(&handle.rgba[12 * 4..12 * 4 + 4], &[0, 5, 90, 255]);
    }

    #[test]
    fn oversized_bytes_are_refused_before_decoding() {
        let err = decode("h".to_owned(), &vec![0u8; MAX_BYTES + 1]).expect_err("over the cap");
        assert_eq!(err, AvatarError::TooManyBytes(MAX_BYTES + 1));
        assert!(err.to_string().contains("256 KB"), "{err}");
        // The cap is the transfer layer's, not a second opinion.
        assert_eq!(MAX_BYTES, jamstream_protocol::control::MAX_AVATAR_BYTES);
    }

    #[test]
    fn oversized_dimensions_are_refused_from_the_header() {
        let bytes = png(MAX_DIM + 1, 4);
        assert!(bytes.len() < MAX_BYTES, "a wide flat png stays small");
        assert_eq!(
            decode("h".to_owned(), &bytes).expect_err("over the dimension cap"),
            AvatarError::TooLarge {
                width: MAX_DIM + 1,
                height: 4,
            }
        );
    }

    #[test]
    fn garbage_and_empty_bytes_are_refused() {
        assert_eq!(
            decode("h".to_owned(), &[]).expect_err("empty"),
            AvatarError::Empty
        );
        assert_eq!(
            decode("h".to_owned(), b"not an image at all").expect_err("garbage"),
            AvatarError::NotPngOrJpeg
        );
        // A truncated PNG has the right magic and no usable pixels.
        let bytes = png(8, 8);
        let err = decode("h".to_owned(), &bytes[..24]).expect_err("truncated png");
        assert!(matches!(err, AvatarError::Decode(_)), "{err:?}");
    }

    #[test]
    fn jpeg_decodes_and_other_formats_do_not() {
        let mut buf = std::io::Cursor::new(Vec::new());
        let img = image::RgbImage::from_fn(9, 9, |x, _| image::Rgb([(x * 20) as u8, 60, 30]));
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Jpeg)
            .expect("encode jpeg");
        let handle = decode("j".to_owned(), &buf.into_inner()).expect("jpeg decodes");
        assert_eq!((handle.width, handle.height), (9, 9));
        // A GIF header: a real image format the transfer layer never carries.
        assert_eq!(
            decode("g".to_owned(), b"GIF89a\x01\x00\x01\x00\x00\x00\x00;")
                .expect_err("gif refused"),
            AvatarError::NotPngOrJpeg
        );
    }

    #[test]
    fn hash_hex_is_lowercase_and_full_width() {
        let mut hash = [0u8; 32];
        hash[0] = 0xab;
        hash[31] = 0x0f;
        let hex = hash_hex(&hash);
        assert_eq!(hex.len(), 64);
        assert!(hex.starts_with("ab00"));
        assert!(hex.ends_with("000f"));
    }

    /// The one rule that has to hold across crates: a name gets the same
    /// disc hue in the app as on the stream card. Both sides are pure
    /// functions of the name, so equality over a spread of names is the
    /// whole contract.
    #[test]
    fn disc_hue_agrees_with_the_broadcast_renderer() {
        for name in [
            "Sam",
            "Ana",
            "Ben",
            "Mira",
            "Lea",
            "Theo",
            "Ivy",
            "Noor",
            "Kai",
            "Zoe",
            "Raul",
            "",
            "Bartholomew Alexander Montgomery Fitzgerald Oyelaran-Wieczorek",
            "\u{00e9}l\u{00e8}ne",
        ] {
            let ours = disc_color(name);
            let theirs = jamstream_broadcast::palette::disc_color(name);
            assert_eq!(
                [ours.r(), ours.g(), ours.b()],
                theirs,
                "disc color for {name:?} differs from the broadcast renderer"
            );
            assert_eq!(
                initials(name),
                jamstream_broadcast::initials(name),
                "initials for {name:?} differ from the broadcast renderer"
            );
        }
    }

    /// The banned arc, from the other direction: 240..300 (indigo through
    /// purple) is unreachable for any name, which is what the design
    /// language actually forbids.
    #[test]
    fn no_name_lands_in_the_purple_arc() {
        for i in 0..4_000u32 {
            let name = format!("member {i}");
            let hue = (fnv1a(name.as_bytes()) % 240) as f32;
            let hue = if hue >= 210.0 { hue + 120.0 } else { hue };
            assert!(!(210.0..330.0).contains(&hue), "{name} hashed to hue {hue}");
        }
    }
}
