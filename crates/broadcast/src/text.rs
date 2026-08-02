//! ab_glyph text with embedded fonts. Glyph origins are quantized to whole
//! pixels so a string rendered at the same box never shimmers between
//! frames. All text lands in cached static layers, never the hot path.

use ab_glyph::{Font, FontRef, GlyphId, PxScale, ScaleFont};

use crate::palette::Rgb;

/// Every character [`Fonts::mono_digits`] can draw. Anything else comes out
/// blank, so widen the set in scripts/subset-card-font.sh before drawing it.
pub const MONO_CHARSET: &str = "0123456789";

pub struct Fonts {
    pub sans: FontRef<'static>,
    pub semibold: FontRef<'static>,
    /// A digits-only cut of IBM Plex Mono. The full face is 155,940 bytes to
    /// draw the listener count, in a binary every session VM downloads at
    /// boot; see [`MONO_CHARSET`] for what this one holds.
    pub mono_digits: FontRef<'static>,
}

impl Fonts {
    pub fn embedded() -> Fonts {
        let load =
            |bytes: &'static [u8]| FontRef::try_from_slice(bytes).expect("embedded font parses");
        Fonts {
            sans: load(include_bytes!("../../../fonts/PublicSans-Regular.ttf")),
            semibold: load(include_bytes!("../../../fonts/PublicSans-SemiBold.ttf")),
            mono_digits: load(include_bytes!("../../../fonts/StageDigits-Regular.ttf")),
        }
    }
}

/// Advance width of `text`, with `spacing` added between glyphs.
pub fn width(font: &FontRef<'static>, size: f32, spacing: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut w = 0.0;
    let mut prev: Option<GlyphId> = None;
    for ch in text.chars() {
        let id = font.glyph_id(ch);
        if let Some(p) = prev {
            w += scaled.kern(p, id) + spacing;
        }
        w += scaled.h_advance(id);
        prev = Some(id);
    }
    w
}

/// Trims `text` to fit `max_w`, appending an ellipsis when it had to cut.
pub fn ellipsize(font: &FontRef<'static>, size: f32, text: &str, max_w: f32) -> String {
    if width(font, size, 0.0, text) <= max_w {
        return text.to_owned();
    }
    let mut s = text.to_owned();
    while s.pop().is_some() {
        let candidate = format!("{}\u{2026}", s.trim_end());
        if width(font, size, 0.0, &candidate) <= max_w {
            return candidate;
        }
    }
    "\u{2026}".to_owned()
}

/// Draws one run at `(x, baseline)`, both rounded to whole pixels, and
/// returns the advance. Coverage is blended over an opaque destination.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    data: &mut [u8],
    w: u32,
    h: u32,
    font: &FontRef<'static>,
    size: f32,
    spacing: f32,
    x: f32,
    baseline: f32,
    color: Rgb,
    alpha: f32,
    text: &str,
) -> f32 {
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    let origin = x.round();
    let baseline = baseline.round();
    let mut caret = origin;
    let mut prev: Option<GlyphId> = None;
    for ch in text.chars() {
        let id = font.glyph_id(ch);
        if let Some(p) = prev {
            caret += scaled.kern(p, id) + spacing;
        }
        let glyph = id.with_scale_and_position(scale, ab_glyph::point(caret.round(), baseline));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, cov| {
                let px = bounds.min.x as i32 + gx as i32;
                let py = bounds.min.y as i32 + gy as i32;
                blend_px(data, w, h, px, py, color, cov * alpha);
            });
        }
        caret += scaled.h_advance(id);
        prev = Some(id);
    }
    caret - origin
}

pub fn blend_px(data: &mut [u8], w: u32, h: u32, x: i32, y: i32, color: Rgb, cov: f32) {
    if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
        return;
    }
    let cov = cov.clamp(0.0, 1.0);
    if cov <= 0.0 {
        return;
    }
    let i = ((y as u32 * w + x as u32) * 4) as usize;
    for k in 0..3 {
        let d = data[i + k] as f32;
        data[i + k] = (d + (color[k] as f32 - d) * cov).round() as u8;
    }
    data[i + 3] = 255;
}

/// Holds the committed digits-only face to the full IBM Plex Mono it was cut
/// from. Nothing in the build regenerates it, so this is what catches a subset
/// that has drifted from its source, in either direction.
#[cfg(test)]
mod tests {
    use super::*;

    /// Only ever linked into the test binary, never into jamstreamd.
    const FULL: &[u8] = include_bytes!("../../../fonts/IBMPlexMono-Regular.ttf");

    /// The card's own size at 1280x720, then off-scale sizes either side of it.
    const SIZES: [f32; 4] = [9.75, 13.0, 19.5, 31.0];

    fn full() -> FontRef<'static> {
        FontRef::try_from_slice(FULL).expect("full mono parses")
    }

    /// One glyph drawn the way the card draws it, onto an opaque field.
    fn pixels(font: &FontRef<'static>, ch: char, size: f32) -> Vec<u8> {
        let (w, h) = (96u32, 72u32);
        let mut data = vec![0u8; (w * h * 4) as usize];
        draw(
            &mut data,
            w,
            h,
            font,
            size,
            0.0,
            12.0,
            56.0,
            [255, 255, 255],
            1.0,
            &ch.to_string(),
        );
        data
    }

    #[test]
    fn subset_draws_every_supported_character_like_the_full_face() {
        let full = full();
        let subset = Fonts::embedded().mono_digits;
        for ch in MONO_CHARSET.chars() {
            assert_ne!(
                subset.glyph_id(ch),
                GlyphId(0),
                "subset is missing {ch:?} from its own charset"
            );
            for size in SIZES {
                assert_eq!(
                    width(&full, size, 0.0, &ch.to_string()),
                    width(&subset, size, 0.0, &ch.to_string()),
                    "advance for {ch:?} at {size}"
                );
                assert!(
                    pixels(&full, ch, size) == pixels(&subset, ch, size),
                    "{ch:?} at {size} rasterises differently out of the subset"
                );
            }
        }
    }

    /// Kerning is read out of the legacy `kern` table, which the subsetter
    /// could plausibly rewrite; the listener count is the only string that
    /// would show it.
    #[test]
    fn subset_kerns_supported_pairs_like_the_full_face() {
        let full = full();
        let subset = Fonts::embedded().mono_digits;
        for size in SIZES {
            let (f, s) = (
                full.as_scaled(PxScale::from(size)),
                subset.as_scaled(PxScale::from(size)),
            );
            for a in MONO_CHARSET.chars() {
                for b in MONO_CHARSET.chars() {
                    assert_eq!(
                        f.kern(full.glyph_id(a), full.glyph_id(b)),
                        s.kern(subset.glyph_id(a), subset.glyph_id(b)),
                        "kern {a:?}{b:?} at {size}"
                    );
                }
            }
        }
    }

    /// The other half: proof this is a subset and not a second copy of the
    /// whole font under another name.
    #[test]
    fn subset_carries_nothing_beyond_its_charset() {
        let full = full();
        let subset = Fonts::embedded().mono_digits;
        let mut checked = 0;
        for ch in ' '..='~' {
            if MONO_CHARSET.contains(ch) {
                continue;
            }
            assert_ne!(
                full.glyph_id(ch),
                GlyphId(0),
                "full mono should have {ch:?}; pick a different probe"
            );
            assert_eq!(
                subset.glyph_id(ch),
                GlyphId(0),
                "subset still carries {ch:?}"
            );
            checked += 1;
        }
        assert_eq!(checked, 85, "ASCII probe set changed size");
    }
}
