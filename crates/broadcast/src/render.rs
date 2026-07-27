//! The stage renderer. A static scene (dithered near-black stage, footer,
//! member cards) is rebuilt only when the roster changes; the per-frame
//! pass copies it and then touches meter pixels only, allocating nothing.

use tiny_skia::{
    FillRule, FilterQuality, Paint, Path, PathBuilder, Pattern, Pixmap, PixmapRef, SpreadMode,
    Stroke, Transform,
};

use crate::avatar::AvatarImage;
use crate::palette::{self as pal, Rgb};
use crate::text::{self, Fonts};
use crate::{MemberVisual, Role, SceneConfig};

/// At most this many musicians are carded; extras are not drawn.
pub const MAX_CARDS: usize = 10;

// Meter law, matching crates/client/src/widgets/meter.rs.
const FLOOR_DB: f32 = -60.0;
const AMBER_FROM_DB: f32 = -12.0;
const RED_FROM_DB: f32 = -3.0;
const UNLIT_BLEND: f32 = 0.16;

/// Peak-hold: hold the top segment this many frames, then fade it out.
const HOLD_FRAMES: u64 = 45;
const HOLD_FADE_FRAMES: u64 = 15;

pub struct Renderer {
    cfg: SceneConfig,
    scale: f32,
    fonts: Fonts,
    /// Stage plus footer; never changes after `new`.
    background: Pixmap,
    /// Background plus every static card layer for the current roster.
    static_scene: Pixmap,
    key: SceneKey,
    meters: Vec<MeterGeom>,
    holds: [Hold; MAX_CARDS],
}

#[derive(Clone, Copy, Default)]
struct Hold {
    frac: f32,
    set_frame: u64,
    active: bool,
}

/// Integer meter geometry so the hot path and the static unlit layer write
/// exactly the same pixels.
#[derive(Clone, Copy)]
struct MeterGeom {
    x0: i32,
    y0: i32,
    seg_w: i32,
    seg_h: i32,
    pitch: i32,
    count: i32,
    connected: bool,
}

/// What the static scene was built for; compared without allocating.
#[derive(Default)]
struct SceneKey {
    members: Vec<MemberKey>,
    listeners: usize,
}

struct MemberKey {
    name: String,
    avatar: Option<(usize, u32, u32)>,
    connected: bool,
}

impl MemberKey {
    /// Equality against a live member, allocation free.
    fn matches(&self, m: &MemberVisual) -> bool {
        self.name == m.name
            && self.connected == m.connected
            && self.avatar
                == m.avatar
                    .as_ref()
                    .map(|a| (a.data_ptr(), a.width(), a.height()))
    }
}

impl Renderer {
    /// Preallocates every buffer for `cfg` and paints the empty stage.
    pub fn new(cfg: SceneConfig) -> Renderer {
        assert!(cfg.width > 0 && cfg.height > 0, "scene must have area");
        let scale = (cfg.width as f32 / 1280.0)
            .min(cfg.height as f32 / 720.0)
            .max(0.25);
        let fonts = Fonts::embedded();
        let mut background = Pixmap::new(cfg.width, cfg.height).expect("stage pixmap");
        paint_stage(&mut background);
        paint_footer(&mut background, &cfg, &fonts, scale);
        let static_scene = background.clone();
        Renderer {
            cfg,
            scale,
            fonts,
            background,
            static_scene,
            key: SceneKey::default(),
            meters: Vec::new(),
            holds: [Hold::default(); MAX_CARDS],
        }
    }

    /// Fills `out` (width*height*4 RGBA, alpha always 255) for one frame.
    /// Musicians beyond [`MAX_CARDS`] are ignored; members with
    /// [`Role::Listener`] never get cards, only `listener_count` shows in
    /// the footer line. Allocates only when the roster changed since the
    /// previous call.
    pub fn render(
        &mut self,
        frame_index: u64,
        members: &[MemberVisual],
        listener_count: usize,
        out: &mut [u8],
    ) {
        let expected = self.cfg.width as usize * self.cfg.height as usize * 4;
        assert_eq!(out.len(), expected, "out must be width*height*4 bytes");

        // Roster comparison without allocating: fixed-capacity scratch of refs.
        let mut musicians: [Option<&MemberVisual>; MAX_CARDS] = [None; MAX_CARDS];
        let mut n = 0;
        for m in members.iter().filter(|m| m.role == Role::Musician) {
            if n == MAX_CARDS {
                break;
            }
            musicians[n] = Some(m);
            n += 1;
        }
        let fresh = self.key.listeners == listener_count
            && self.key.members.len() == n
            && self
                .key
                .members
                .iter()
                .zip(&musicians[..n])
                .all(|(k, m)| k.matches(m.expect("filled above")));
        if !fresh {
            let roster: Vec<&MemberVisual> =
                musicians[..n].iter().map(|m| m.expect("filled")).collect();
            self.rebuild(&roster, listener_count);
        }

        out.copy_from_slice(self.static_scene.data());

        let (w, h) = (self.cfg.width, self.cfg.height);
        for (slot, m) in musicians[..n].iter().enumerate() {
            let m = m.expect("filled above");
            let geom = self.meters[slot];
            draw_meter_live(
                out,
                w,
                h,
                &geom,
                &mut self.holds[slot],
                frame_index,
                m.level_peak,
                m.level_rms,
            );
        }
    }

    /// Repaints the static scene for a changed roster. The only place that
    /// allocates after construction.
    fn rebuild(&mut self, musicians: &[&MemberVisual], listeners: usize) {
        self.static_scene
            .data_mut()
            .copy_from_slice(self.background.data());
        self.meters.clear();

        let s = self.scale;
        let (w, h) = (self.cfg.width as f32, self.cfg.height as f32);
        let margin = 24.0 * s;
        let footer_h = 48.0 * s;
        let grid_x = margin;
        let grid_w = w - 2.0 * margin;
        let grid_y = margin;
        let grid_h = h - footer_h - 2.0 * margin;

        if !musicians.is_empty() {
            let rows = row_breaks(musicians.len());
            let cols = *rows.iter().max().expect("rows nonempty") as f32;
            let nrows = rows.len() as f32;
            let gutter = 20.0 * s;
            let card_w_max = (grid_w - (cols - 1.0) * gutter) / cols;
            let card_h_max = (grid_h - (nrows - 1.0) * gutter) / nrows;
            const ASPECT: f32 = 0.86; // card width over height
            let card_h = card_h_max.min(card_w_max / ASPECT).min(430.0 * s);
            let card_w = card_h * ASPECT;
            let total_h = nrows * card_h + (nrows - 1.0) * gutter;
            let y0 = grid_y + (grid_h - total_h) / 2.0;
            let mut idx = 0;
            for (r, &k) in rows.iter().enumerate() {
                let row_w = k as f32 * card_w + (k as f32 - 1.0) * gutter;
                let x0 = grid_x + (grid_w - row_w) / 2.0;
                let y = y0 + r as f32 * (card_h + gutter);
                for c in 0..k {
                    let x = x0 + c as f32 * (card_w + gutter);
                    let geom = self.draw_card(musicians[idx], x, y, card_w, card_h);
                    self.meters.push(geom);
                    idx += 1;
                }
            }
        }

        if listeners > 0 {
            self.draw_listener_line(listeners);
        }

        self.key = SceneKey {
            members: musicians
                .iter()
                .map(|m| MemberKey {
                    name: m.name.clone(),
                    avatar: m
                        .avatar
                        .as_ref()
                        .map(|a| (a.data_ptr(), a.width(), a.height())),
                    connected: m.connected,
                })
                .collect(),
            listeners,
        };
    }

    /// Paints one card's static content and returns its meter geometry.
    fn draw_card(&mut self, m: &MemberVisual, x: f32, y: f32, cw: f32, ch: f32) -> MeterGeom {
        let s = self.scale;
        let px = &mut self.static_scene;
        let dim: f32 = if m.connected { 1.0 } else { 0.42 };

        let panel = rounded_rect(x, y, cw, ch, 4.0 * s);
        fill(px, &panel, pal::SURFACE1, 255);
        let border = if m.connected {
            pal::BORDER
        } else {
            pal::blend(pal::SURFACE1, pal::BORDER, 0.55)
        };
        stroke(px, &panel, border, 1.0);

        // Avatar circle, cover-cropped, or the initials disc.
        let d = ch * 0.46;
        let cx = x + cw / 2.0;
        let cy = y + ch * 0.14 + d / 2.0;
        match &m.avatar {
            Some(av) => draw_avatar(px, av, cx, cy, d / 2.0),
            None => {
                let disc = PathBuilder::from_circle(cx, cy, d / 2.0).expect("circle path");
                fill(px, &disc, pal::disc_color(&m.name), 255);
                let init = initials(&m.name);
                let size = d * 0.34;
                let tw = text::width(&self.fonts.semibold, size, 0.0, &init);
                text::draw(
                    px.data_mut(),
                    self.cfg.width,
                    self.cfg.height,
                    &self.fonts.semibold,
                    size,
                    0.0,
                    cx - tw / 2.0,
                    cy + size * 0.36,
                    pal::TEXT_PRIMARY,
                    1.0,
                    &init,
                );
            }
        }
        if !m.connected {
            // Pull the portrait down toward the panel; the card stays.
            let veil = PathBuilder::from_circle(cx, cy, d / 2.0 + 0.5).expect("circle path");
            fill(px, &veil, pal::SURFACE1, 170);
        }

        // Name, ellipsized to the card, centered.
        let name_size = (ch * 0.085).clamp(12.0, 21.0);
        let name_max = cw - 24.0 * s;
        let name = text::ellipsize(&self.fonts.sans, name_size, &m.name, name_max);
        let nw = text::width(&self.fonts.sans, name_size, 0.0, &name);
        let name_color = if m.connected {
            pal::TEXT_PRIMARY
        } else {
            pal::TEXT_MUTED
        };
        text::draw(
            px.data_mut(),
            self.cfg.width,
            self.cfg.height,
            &self.fonts.sans,
            name_size,
            0.0,
            cx - nw / 2.0,
            cy + d / 2.0 + ch * 0.065 + name_size * 0.78,
            name_color,
            dim.max(0.55),
            &name,
        );

        // Meter track and unlit segments; lit segments come per frame.
        let m_w = cw * 0.72;
        let m_h = (ch * 0.052).clamp(7.0, 13.0);
        let m_x = cx - m_w / 2.0;
        let m_y = y + ch - ch * 0.105 - m_h;
        let track = rounded_rect(m_x, m_y, m_w, m_h, 2.0);
        fill(px, &track, pal::WELL, 255);

        let inset = 2.0_f32.max((2.0 * s).round());
        let ix = (m_x + inset).round() as i32;
        let iy = (m_y + inset).round() as i32;
        let iw = (m_w - 2.0 * inset).round() as i32;
        let ih = (m_h - 2.0 * inset).round() as i32;
        let seg_w = 3.0_f32.max((4.0 * s).round()) as i32;
        let gap = 1.0_f32.max((2.0 * s).round()) as i32;
        let pitch = seg_w + gap;
        let count = ((iw + gap) / pitch).max(1);
        let span = count * pitch - gap;
        let geom = MeterGeom {
            x0: ix + (iw - span) / 2,
            y0: iy,
            seg_w,
            seg_h: ih.max(1),
            pitch,
            count,
            connected: m.connected,
        };
        let unlit_t = if m.connected { UNLIT_BLEND } else { 0.07 };
        let data = px.data_mut();
        for i in 0..count {
            let zone = zone_color(seg_mid_frac(i, count));
            fill_rect_rgba(
                data,
                self.cfg.width,
                self.cfg.height,
                [geom.x0 + i * geom.pitch, geom.y0, geom.seg_w, geom.seg_h],
                pal::blend(pal::WELL, zone, unlit_t),
            );
        }
        geom
    }

    /// "N listening", count in the mono, quiet and centered in the footer.
    fn draw_listener_line(&mut self, listeners: usize) {
        let s = self.scale;
        let size = 13.0 * s;
        let count = listeners.to_string();
        let label = " listening";
        let wc = text::width(&self.fonts.mono, size, 0.0, &count);
        let wl = text::width(&self.fonts.sans, size, 0.0, label);
        let x = (self.cfg.width as f32 - wc - wl) / 2.0;
        let baseline = self.cfg.height as f32 - 17.0 * s;
        let (w, h) = (self.cfg.width, self.cfg.height);
        let advance = text::draw(
            self.static_scene.data_mut(),
            w,
            h,
            &self.fonts.mono,
            size,
            0.0,
            x,
            baseline,
            pal::TEXT_MUTED,
            1.0,
            &count,
        );
        text::draw(
            self.static_scene.data_mut(),
            w,
            h,
            &self.fonts.sans,
            size,
            0.0,
            x.round() + advance,
            baseline,
            pal::TEXT_MUTED,
            1.0,
            label,
        );
    }
}

/// Row layout per musician count, tuned to look intentional at each count.
fn row_breaks(n: usize) -> &'static [usize] {
    const TABLE: [&[usize]; 10] = [
        &[1],
        &[2],
        &[3],
        &[2, 2],
        &[3, 2],
        &[3, 3],
        &[4, 3],
        &[4, 4],
        &[3, 3, 3],
        &[4, 3, 3],
    ];
    TABLE[n.clamp(1, MAX_CARDS) - 1]
}

/// Near-black stage with a vertical falloff a few code values deep, ordered
/// dithered so the ramp never reads as bands. Stays neutral: equal deltas
/// per channel around surface0.
fn paint_stage(px: &mut Pixmap) {
    #[rustfmt::skip]
    const BAYER: [[u8; 8]; 8] = [
        [ 0, 32,  8, 40,  2, 34, 10, 42],
        [48, 16, 56, 24, 50, 18, 58, 26],
        [12, 44,  4, 36, 14, 46,  6, 38],
        [60, 28, 52, 20, 62, 30, 54, 22],
        [ 3, 35, 11, 43,  1, 33,  9, 41],
        [51, 19, 59, 27, 49, 17, 57, 25],
        [15, 47,  7, 39, 13, 45,  5, 37],
        [63, 31, 55, 23, 61, 29, 53, 21],
    ];
    let (w, h) = (px.width() as usize, px.height() as usize);
    let data = px.data_mut();
    for y in 0..h {
        let t = if h > 1 {
            y as f32 / (h - 1) as f32
        } else {
            0.0
        };
        let base: [f32; 3] = [
            pal::SURFACE0[0] as f32 + 3.0 - 7.0 * t,
            pal::SURFACE0[1] as f32 + 3.0 - 7.0 * t,
            pal::SURFACE0[2] as f32 + 3.0 - 7.0 * t,
        ];
        for x in 0..w {
            let thresh = (BAYER[y % 8][x % 8] as f32 + 0.5) / 64.0;
            let i = (y * w + x) * 4;
            for k in 0..3 {
                data[i + k] = (base[k] + thresh) as u8;
            }
            data[i + 3] = 255;
        }
    }
}

/// Session name lower left, wordmark lockup lower right.
fn paint_footer(px: &mut Pixmap, cfg: &SceneConfig, fonts: &Fonts, s: f32) {
    let (w, h) = (cfg.width, cfg.height);
    let baseline = h as f32 - 17.0 * s;
    text::draw(
        px.data_mut(),
        w,
        h,
        &fonts.semibold,
        15.0 * s,
        0.0,
        24.0 * s,
        baseline,
        pal::TEXT_PRIMARY,
        1.0,
        &cfg.session_name,
    );
    if cfg.wordmark {
        let size = 14.0 * s;
        let spacing = -size * 0.015;
        let word = "jamstream";
        let tw = text::width(&fonts.semibold, size, spacing, word);
        let dot_r = (size * 0.10).max(2.0);
        let dot_gap = size * 0.28;
        let x = w as f32 - 24.0 * s - (tw + dot_gap + dot_r * 2.0);
        text::draw(
            px.data_mut(),
            w,
            h,
            &fonts.semibold,
            size,
            spacing,
            x,
            baseline,
            pal::TEXT_PRIMARY,
            1.0,
            word,
        );
        // The amber tuning dot at x-height center, like a channel lamp.
        let dot = PathBuilder::from_circle(
            x.round() + tw + dot_gap + dot_r,
            baseline - size * 0.31,
            dot_r,
        )
        .expect("dot path");
        fill(px, &dot, pal::ACCENT, 255);
    }
}

/// First letters of the first two words; two letters of a lone word. Public
/// so the desktop client's copy can be held to it by test.
pub fn initials(name: &str) -> String {
    let mut words = name.split_whitespace();
    let first = words.next().unwrap_or("");
    let second = words.next();
    let mut out = String::new();
    let mut push_first_upper = |w: &str| {
        if let Some(c) = w.chars().next() {
            out.extend(c.to_uppercase());
        }
    };
    match second {
        Some(second) => {
            push_first_upper(first);
            push_first_upper(second);
        }
        None => {
            let mut chars = first.chars();
            if let Some(c) = chars.next() {
                out.extend(c.to_uppercase());
            }
            if let Some(c) = chars.next() {
                out.extend(c.to_uppercase());
            }
        }
    }
    if out.is_empty() {
        out.push('?');
    }
    out
}

fn draw_avatar(px: &mut Pixmap, av: &AvatarImage, cx: f32, cy: f32, r: f32) {
    let src = PixmapRef::from_bytes(av.data(), av.width(), av.height())
        .expect("avatar buffer is width*height*4");
    // Cover crop: scale the short side to the diameter, center the rest.
    let scale = (2.0 * r) / av.width().min(av.height()) as f32;
    let tx = cx - av.width() as f32 * scale / 2.0;
    let ty = cy - av.height() as f32 * scale / 2.0;
    let paint = Paint {
        shader: Pattern::new(
            src,
            SpreadMode::Pad,
            FilterQuality::Bilinear,
            1.0,
            Transform::from_row(scale, 0.0, 0.0, scale, tx, ty),
        ),
        anti_alias: true,
        ..Paint::default()
    };
    let path = PathBuilder::from_circle(cx, cy, r).expect("circle path");
    px.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

fn fill(px: &mut Pixmap, path: &Path, rgb: Rgb, a: u8) {
    let mut paint = Paint::default();
    paint.set_color_rgba8(rgb[0], rgb[1], rgb[2], a);
    paint.anti_alias = true;
    px.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
}

fn stroke(px: &mut Pixmap, path: &Path, rgb: Rgb, width: f32) {
    let mut paint = Paint::default();
    paint.set_color_rgba8(rgb[0], rgb[1], rgb[2], 255);
    paint.anti_alias = true;
    let stroke = Stroke {
        width,
        ..Stroke::default()
    };
    px.stroke_path(path, &paint, &stroke, Transform::identity(), None);
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Path {
    let r = r.min(w / 2.0).min(h / 2.0);
    let k = r * 0.5523;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish().expect("rounded rect path")
}

fn to_db(linear: f32) -> f32 {
    if linear <= 0.0 {
        FLOOR_DB
    } else {
        (20.0 * linear.log10()).clamp(FLOOR_DB, 0.0)
    }
}

fn frac(db: f32) -> f32 {
    (db - FLOOR_DB) / -FLOOR_DB
}

fn seg_mid_frac(i: i32, count: i32) -> f32 {
    (i as f32 + 0.5) / count as f32
}

fn zone_color(f_mid: f32) -> Rgb {
    let db = FLOOR_DB + f_mid * -FLOOR_DB;
    if db >= RED_FROM_DB {
        pal::METER_RED
    } else if db >= AMBER_FROM_DB {
        pal::METER_AMBER
    } else {
        pal::METER_GREEN
    }
}

/// Per-frame meter pass: lit segments over the prebaked unlit track, plus
/// the peak-hold segment. Hold state advances on frame_index arithmetic
/// only, so replaying a frame sequence is byte-identical.
#[allow(clippy::too_many_arguments)]
fn draw_meter_live(
    out: &mut [u8],
    w: u32,
    h: u32,
    g: &MeterGeom,
    hold: &mut Hold,
    frame: u64,
    peak: f32,
    rms: f32,
) {
    if !g.connected {
        hold.active = false;
        return;
    }
    let peak_db = to_db(peak);
    let live = peak_db > FLOOR_DB + 0.5;
    let peak_frac = frac(peak_db);
    let rms_frac = frac(to_db(rms));

    // Instant attack; after hold plus fade, snap back to the live peak.
    // `>=` keeps re-rendering the same frame idempotent.
    if !hold.active
        || peak_frac >= hold.frac
        || frame.saturating_sub(hold.set_frame) >= HOLD_FRAMES + HOLD_FADE_FRAMES
    {
        hold.frac = peak_frac;
        hold.set_frame = frame;
        hold.active = live;
    }
    let age = frame.saturating_sub(hold.set_frame);
    let hold_alpha = if age <= HOLD_FRAMES {
        1.0
    } else {
        1.0 - (age - HOLD_FRAMES) as f32 / HOLD_FADE_FRAMES as f32
    };

    let count = g.count;
    let last = count - 1;
    let peak_seg = ((peak_frac * count as f32) as i32).min(last);
    let hold_seg = ((hold.frac * count as f32) as i32).min(last);
    for i in 0..count {
        let f_mid = seg_mid_frac(i, count);
        let zone = zone_color(f_mid);
        let alpha = if f_mid <= rms_frac || (live && i == peak_seg) {
            1.0
        } else if hold.active && i == hold_seg {
            hold_alpha
        } else {
            continue;
        };
        let color = if alpha >= 1.0 {
            zone
        } else {
            pal::blend(pal::blend(pal::WELL, zone, UNLIT_BLEND), zone, alpha)
        };
        fill_rect_rgba(
            out,
            w,
            h,
            [g.x0 + i * g.pitch, g.y0, g.seg_w, g.seg_h],
            color,
        );
    }
}

/// `rect` is `[x, y, w, h]` in pixels, clamped to the frame.
fn fill_rect_rgba(out: &mut [u8], w: u32, h: u32, rect: [i32; 4], rgb: Rgb) {
    let [x, y, rw, rh] = rect;
    let x0 = x.max(0) as u32;
    let y0 = y.max(0) as u32;
    let x1 = ((x + rw).max(0) as u32).min(w);
    let y1 = ((y + rh).max(0) as u32).min(h);
    for row in y0..y1 {
        let start = ((row * w + x0) * 4) as usize;
        let end = ((row * w + x1) * 4) as usize;
        for px in out[start..end].chunks_exact_mut(4) {
            px[0] = rgb[0];
            px[1] = rgb[1];
            px[2] = rgb[2];
            px[3] = 255;
        }
    }
}
