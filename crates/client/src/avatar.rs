//! Avatar pixels for the interface, plus the initials disc every member
//! without one falls back to.
//!
//! Decoding happens once per content hash: the session layer hands the app
//! verified bytes keyed by their Blake2s-256, the runtime decodes them into
//! [`crate::runtime::AvatarHandle`], and the UI uploads one egui texture per
//! hash. The caps mirror the transfer layer's, so bytes that could never
//! reach a peer are refused with the same numbers here.
//!
//! A picture the musician picks off their own disk goes through [`load`]
//! first: a phone photo is 3 MB and 4000 px across, so it is cropped to a
//! square, scaled to [`FIT_DIM`], and re-encoded before [`decode`] sees it.
//! The caps are unchanged and still the last word; they are simply met by
//! construction, so nobody is sent to find an image editor.
//!
//! The disc hue rules are the ones crates/broadcast/src/palette.rs paints on
//! the stream cards, so a member looks the same in the app and on air. The
//! unit tests assert that agreement against the broadcast crate directly.

use std::path::Path;
use std::sync::Arc;

use data_encoding::HEXLOWER;
use egui::Color32;
use image::{ImageDecoder, Limits};

use crate::runtime::AvatarHandle;

/// Encoded size cap: the transfer layer's, so a file the session would
/// refuse is refused here with the same number, before any decoding.
pub const MAX_BYTES: usize = jamstream_protocol::control::MAX_AVATAR_BYTES;

/// Decoded dimension cap per axis, mirroring crates/broadcast. Checked from
/// the header before the full decode so a small file cannot decompress into
/// a huge allocation.
pub const MAX_DIM: u32 = 1024;

/// The square [`load`] fits a picked picture into.
///
/// Taken from what actually gets drawn. The largest avatar anywhere is the
/// broadcast card's disc: card height caps at 430 px in a 720p frame and the
/// disc is 0.46 of it, so 198 px. In the app the largest is the mixer
/// strip's 30 points, which is 60 px at the 2x scaling the snapshot suite
/// and every retina laptop run at. 256 covers both, leaves room for a 3x
/// display, and re-encodes to roughly 25 KB, a tenth of the byte cap.
pub const FIT_DIM: u32 = 256;

/// Quality for the re-encode. Higher than the usual 85 because there is
/// byte budget to spare and the result is scaled down again when drawn.
const JPEG_QUALITY: u8 = 90;

/// Source pixels [`load`] will decode. A 100 megapixel file is past any
/// phone or camera, and its RGBA buffer is already 400 MB, so this is the
/// ceiling on what a decompression bomb can ask for.
const MAX_SOURCE_PIXELS: u64 = 100_000_000;

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
    /// Past what the fitter will decode at all, so it is refused from the
    /// header rather than allocated for.
    TooManyPixels {
        width: u32,
        height: u32,
    },
    /// Header said PNG or JPEG, the pixels disagreed.
    Decode(String),
    /// The fitted pixels would not re-encode.
    Encode(String),
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
            AvatarError::TooManyPixels { width, height } => write!(
                f,
                "the image is {width}x{height}; the limit is {} megapixels",
                MAX_SOURCE_PIXELS / 1_000_000
            ),
            AvatarError::Decode(err) => write!(f, "the image did not decode: {err}"),
            AvatarError::Encode(err) => write!(f, "the image did not re-encode: {err}"),
        }
    }
}

impl std::error::Error for AvatarError {}

/// A picture chosen off disk, fitted and decoded: everything the settings
/// row needs to show it and the session needs to send it.
#[derive(Clone)]
pub struct Picture {
    /// The file's own name, so the row can name what was picked.
    pub file: String,
    /// Pixels the file held, for the line that says what happened to it.
    pub source: (u32, u32),
    /// Pixels the fitted image holds.
    pub fitted: (u32, u32),
    /// The fitted encoding; what a join announces and a peer decodes.
    pub bytes: Vec<u8>,
    pub handle: AvatarHandle,
}

impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Picture")
            .field("file", &self.file)
            .field("source", &self.source)
            .field("fitted", &self.fitted)
            .field("bytes", &format_args!("{} bytes", self.bytes.len()))
            .finish()
    }
}

/// Reads a picked file and runs the whole pipeline on it: fit, then the
/// same [`decode`] any avatar off the wire goes through.
///
/// Called on the picker thread, never on the paint thread: decoding a 12
/// megapixel photo and scaling it takes long enough to drop frames.
pub fn load(path: &Path) -> Result<Picture, String> {
    let file = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let bytes = std::fs::read(path).map_err(|err| format!("{file} could not be read: {err}"))?;
    // Named, because a musician who picked the wrong file needs to know
    // which file the complaint is about.
    let fitted = fit(&bytes).map_err(|err| format!("{file}: {err}"))?;
    let handle =
        decode(local_key(&fitted.bytes), &fitted.bytes).map_err(|err| format!("{file}: {err}"))?;
    Ok(Picture {
        file,
        source: fitted.source,
        fitted: fitted.fitted,
        bytes: fitted.bytes,
        handle,
    })
}

/// A picture cut down to the size the app and the stream card draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fitted {
    pub bytes: Vec<u8>,
    pub source: (u32, u32),
    pub fitted: (u32, u32),
}

/// Crops a picked picture to a centred square, scales it to [`FIT_DIM`], and
/// re-encodes it, so that [`decode`]'s caps hold for any real photograph.
///
/// The header discipline is [`decode`]'s: format allow list and dimensions
/// off the header, explicit [`Limits`] on the decoder, and only then the
/// pixels. A local file is a different trust level from a stranger's avatar,
/// but a picture bomb is a picture bomb.
pub fn fit(bytes: &[u8]) -> Result<Fitted, AvatarError> {
    if bytes.is_empty() {
        return Err(AvatarError::Empty);
    }
    let mut limits = alloc_limit(MAX_SOURCE_PIXELS * 4);
    let decoder = header(bytes, limits.clone())?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 {
        return Err(AvatarError::TooLarge { width, height });
    }
    if u64::from(width) * u64::from(height) > MAX_SOURCE_PIXELS {
        return Err(AvatarError::TooManyPixels { width, height });
    }
    // Already at drawing size and under the cap: send the file the musician
    // chose rather than recompressing their picture for nothing.
    if width <= FIT_DIM && height <= FIT_DIM && bytes.len() <= MAX_BYTES {
        return Ok(Fitted {
            bytes: bytes.to_vec(),
            source: (width, height),
            fitted: (width, height),
        });
    }
    limits
        .reserve(decoder.total_bytes())
        .map_err(|err| AvatarError::Decode(err.to_string()))?;
    let source = image::DynamicImage::from_decoder(decoder)
        .map_err(|err| AvatarError::Decode(err.to_string()))?;

    // Never upscale: a 64 px picture stays 64 px rather than being blown up
    // into four times the bytes for no more detail.
    let target = FIT_DIM.min(width.min(height));
    // Half size is the retry, and it only exists for one case: 256x256 of
    // pure noise with an alpha channel is 262 KB as a PNG, a hair over the
    // cap, and 128 px of anything at all is far under it.
    let mut last = 0;
    for side in [target, (target / 2).max(1)] {
        let out = encode(&source, side)?;
        last = out.len();
        if out.len() <= MAX_BYTES {
            return Ok(Fitted {
                bytes: out,
                source: (width, height),
                fitted: (side, side),
            });
        }
    }
    Err(AvatarError::TooManyBytes(last))
}

/// Crops to a centred square of `side` and encodes. Transparency keeps PNG;
/// anything opaque, which is every photograph, goes to JPEG, where the same
/// pixels cost a fifth of the bytes.
fn encode(source: &image::DynamicImage, side: u32) -> Result<Vec<u8>, AvatarError> {
    let square = source.resize_to_fill(side, side, image::imageops::FilterType::Lanczos3);
    let rgba = square.to_rgba8();
    let mut out = Vec::new();
    let encoded = if rgba.pixels().any(|px| px.0[3] < u8::MAX) {
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
    } else {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
            .encode_image(&square.to_rgb8())
    };
    encoded.map_err(|err| AvatarError::Encode(err.to_string()))?;
    Ok(out)
}

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
    // An avatar at the dimension cap is 1024x1024, and 16 bits a channel at
    // that size is 8 MB. Nothing legitimate here needs more.
    let mut limits = alloc_limit(u64::from(MAX_DIM) * u64::from(MAX_DIM) * 8);
    let decoder = header(bytes, limits.clone())?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return Err(AvatarError::TooLarge { width, height });
    }
    limits
        .reserve(decoder.total_bytes())
        .map_err(|err| AvatarError::Decode(err.to_string()))?;
    // The same decoder the dimensions came from, so the checked size and the
    // decoded size cannot disagree.
    let rgba = image::DynamicImage::from_decoder(decoder)
        .map_err(|err| AvatarError::Decode(err.to_string()))?
        .to_rgba8();
    Ok(AvatarHandle {
        hash,
        width,
        height,
        rgba: Arc::from(rgba.into_raw().into_boxed_slice()),
    })
}

/// The header half of both paths: guess the format, refuse anything outside
/// the allow list, and hand back a decoder that has read no pixels yet and
/// carries `limits`.
fn header(bytes: &[u8], limits: Limits) -> Result<impl ImageDecoder + '_, AvatarError> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|err| AvatarError::Decode(err.to_string()))?;
    match reader.format() {
        Some(image::ImageFormat::Png | image::ImageFormat::Jpeg) => {}
        _ => return Err(AvatarError::NotPngOrJpeg),
    }
    reader.limits(limits);
    reader
        .into_decoder()
        .map_err(|err| AvatarError::Decode(err.to_string()))
}

/// Explicit limits rather than the crate's defaults: the PNG decoder takes
/// `max_alloc` as its own buffer ceiling, so this is what a bomb runs into.
/// The dimension fields stay unset on purpose, because a strict dimension
/// limit fails with a generic limits error and both callers want to name the
/// size they refused.
fn alloc_limit(max_alloc: u64) -> Limits {
    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(max_alloc);
    limits
}

/// Lowercase hex of a content hash; the cache and texture key everywhere.
pub fn hash_hex(hash: &[u8; 32]) -> String {
    HEXLOWER.encode(hash)
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
pub(crate) mod tests {
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

    /// A stand-in for a photograph: flat-ish background with one centred
    /// square marker, encoded as a JPEG the way a camera would.
    fn photo(w: u32, h: u32, marker: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            let centred = x.abs_diff(w / 2) * 2 < marker && y.abs_diff(h / 2) * 2 < marker;
            if centred {
                image::Rgb([235, 60, 40])
            } else {
                image::Rgb([30, 100 + (y % 60) as u8, 80])
            }
        });
        let mut buf = std::io::Cursor::new(Vec::new());
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 92)
            .encode_image(&img)
            .expect("encode jpeg");
        buf.into_inner()
    }

    /// The whole point: a photograph straight off a phone, which trips both
    /// caps on its own, comes out the other side inside both of them.
    #[test]
    fn a_phone_photo_fits_under_both_caps() {
        let bytes = photo(4032, 3024, 1512);
        assert!(bytes.len() > MAX_BYTES, "a 12 megapixel jpeg is not small");
        assert_eq!(
            decode("raw".to_owned(), &bytes).expect_err("the file itself is refused"),
            AvatarError::TooManyBytes(bytes.len()),
            "the caps are what the fitter exists for"
        );

        let fitted = fit(&bytes).expect("a phone photo fits");
        assert_eq!(fitted.source, (4032, 3024));
        assert_eq!(fitted.fitted, (FIT_DIM, FIT_DIM));
        // Comfortably under, not marginally: a quarter of the cap is the
        // bar, and a 256 px jpeg lands nearer a tenth.
        assert!(
            fitted.bytes.len() < MAX_BYTES / 4,
            "fitted to {} KB, which is not comfortable",
            fitted.bytes.len().div_ceil(1024)
        );

        let handle = decode(local_key(&fitted.bytes), &fitted.bytes).expect("the fit decodes");
        assert_eq!((handle.width, handle.height), (FIT_DIM, FIT_DIM));
        assert!(handle.width <= MAX_DIM && handle.height <= MAX_DIM);
    }

    /// Square and undistorted: the crop is the centred square of the
    /// original, so a square marker in the middle of a wide picture stays
    /// square, and stays the fraction of the frame it started as.
    #[test]
    fn fitting_crops_to_a_centred_square_without_stretching() {
        // The centred 600x600 of this is half marker, so the marker must
        // come out half of 256.
        let fitted = fit(&photo(1200, 600, 300)).expect("fits");
        assert_eq!(fitted.fitted, (FIT_DIM, FIT_DIM));
        let handle = decode(local_key(&fitted.bytes), &fitted.bytes).expect("decodes");

        let marker = |x: u32, y: u32| {
            let i = ((y * handle.width + x) * 4) as usize;
            handle.rgba[i] > 150 && handle.rgba[i + 1] < 140
        };
        let across = (0..FIT_DIM).filter(|&x| marker(x, FIT_DIM / 2)).count() as i64;
        let down = (0..FIT_DIM).filter(|&y| marker(FIT_DIM / 2, y)).count() as i64;
        assert!(
            (across - down).abs() <= 2,
            "the marker came out {across} across and {down} down"
        );
        assert!(
            (across - i64::from(FIT_DIM) / 2).abs() <= 3,
            "the marker is {across} px, not the half frame it started as"
        );
    }

    /// A picture already at drawing size is sent as the musician's own file,
    /// not recompressed for nothing.
    #[test]
    fn a_small_picture_is_left_alone() {
        let bytes = png(64, 48);
        let fitted = fit(&bytes).expect("fits");
        assert_eq!(fitted.bytes, bytes);
        assert_eq!(fitted.source, (64, 48));
        assert_eq!(fitted.fitted, (64, 48));
    }

    /// Transparency is the one reason to stay in PNG, so it survives.
    #[test]
    fn transparency_survives_the_fit() {
        let img = image::RgbaImage::from_fn(700, 700, |x, y| {
            let alpha = if x < 350 && y < 350 { 0 } else { 255 };
            image::Rgba([200, 120, 40, alpha])
        });
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .expect("encode png");

        let fitted = fit(&buf.into_inner()).expect("fits");
        assert_eq!(fitted.fitted, (FIT_DIM, FIT_DIM));
        assert!(fitted.bytes.len() <= MAX_BYTES);
        let handle = decode(local_key(&fitted.bytes), &fitted.bytes).expect("decodes");
        assert_eq!(handle.rgba[3], 0, "the clear corner went opaque");
        let opposite = ((FIT_DIM * FIT_DIM - 1) * 4 + 3) as usize;
        assert_eq!(handle.rgba[opposite], 255);
    }

    /// A header claiming more pixels than the fitter will ever decode is
    /// refused from that header, before a byte of image data is touched.
    #[test]
    fn a_picture_bomb_is_refused_from_the_header() {
        let bytes = png_header_claiming(12_000, 9_000);
        assert!(bytes.len() < 100, "the header alone, no pixels behind it");
        assert_eq!(
            fit(&bytes).expect_err("108 megapixels is past the ceiling"),
            AvatarError::TooManyPixels {
                width: 12_000,
                height: 9_000,
            }
        );
        assert!(
            fit(&bytes)
                .expect_err("refused")
                .to_string()
                .contains("100 megapixels")
        );
    }

    #[test]
    fn the_fitter_refuses_what_is_not_a_picture() {
        assert_eq!(fit(&[]).expect_err("empty"), AvatarError::Empty);
        assert_eq!(
            fit(b"GIF89a\x01\x00\x01\x00\x00\x00\x00;").expect_err("gif"),
            AvatarError::NotPngOrJpeg
        );
        let bytes = png(8, 8);
        assert!(matches!(
            fit(&bytes[..24]).expect_err("truncated png"),
            AvatarError::Decode(_)
        ));
    }

    /// A PNG that is nothing but a header: the signature, an IHDR saying
    /// `w`x`h` of 8-bit grey, and an empty IDAT so the decoder stops there.
    fn png_header_claiming(w: u32, h: u32) -> Vec<u8> {
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
            let mut tagged = kind.to_vec();
            tagged.extend_from_slice(body);
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            out.extend_from_slice(&tagged);
            out.extend_from_slice(&crc32(&tagged).to_be_bytes());
        }
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        // 8 bits a channel, colour type 0 (grey), no interlace.
        ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);

        let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &[]);
        chunk(&mut out, b"IEND", &[]);
        out
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
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
