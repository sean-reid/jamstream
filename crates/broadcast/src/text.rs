//! ab_glyph text with embedded fonts. Glyph origins are quantized to whole
//! pixels so a string rendered at the same box never shimmers between
//! frames. All text lands in cached static layers, never the hot path.

use ab_glyph::{Font, FontRef, GlyphId, PxScale, ScaleFont};

use crate::palette::Rgb;

pub struct Fonts {
    pub sans: FontRef<'static>,
    pub semibold: FontRef<'static>,
    pub mono: FontRef<'static>,
}

impl Fonts {
    pub fn embedded() -> Fonts {
        let load =
            |bytes: &'static [u8]| FontRef::try_from_slice(bytes).expect("embedded font parses");
        Fonts {
            sans: load(include_bytes!("../../../fonts/PublicSans-Regular.ttf")),
            semibold: load(include_bytes!("../../../fonts/PublicSans-SemiBold.ttf")),
            mono: load(include_bytes!("../../../fonts/IBMPlexMono-Regular.ttf")),
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
