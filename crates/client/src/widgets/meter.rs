//! Level meter with real ballistics: instant attack, timed release, and a
//! 1.5 s peak-hold tick. Green to -12 dBFS, amber to -3, red above. Drawn
//! entirely with the painter; ballistic state lives in egui memory keyed by
//! id, advanced with `stable_dt`.

use egui::{Color32, CornerRadius, Rect, Sense, Ui, Vec2, pos2};

use crate::theme;

const FLOOR_DB: f32 = -60.0;
const AMBER_FROM_DB: f32 = -12.0;
const RED_FROM_DB: f32 = -3.0;
const PEAK_RELEASE_DB_PER_S: f32 = 26.0;
const RMS_RELEASE_DB_PER_S: f32 = 20.0;
const HOLD_SECS: f32 = 1.5;
const HOLD_RELEASE_DB_PER_S: f32 = 40.0;

#[derive(Clone, Copy)]
struct MeterState {
    peak_db: f32,
    rms_db: f32,
    hold_db: f32,
    hold_age: f32,
}

impl Default for MeterState {
    fn default() -> Self {
        MeterState {
            peak_db: FLOOR_DB,
            rms_db: FLOOR_DB,
            hold_db: FLOOR_DB,
            hold_age: 0.0,
        }
    }
}

fn to_db(linear: f32) -> f32 {
    if linear <= 0.0 {
        FLOOR_DB
    } else {
        (20.0 * linear.log10()).clamp(FLOOR_DB, 0.0)
    }
}

fn zone_color(db: f32, p: &theme::Palette) -> Color32 {
    if db >= RED_FROM_DB {
        p.meter_red
    } else if db >= AMBER_FROM_DB {
        p.meter_amber
    } else {
        p.meter_green
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Meter {
    Vertical,
    Horizontal,
}

/// `peak` and `rms` are linear 0..1 from [`crate::runtime::LevelsView`].
pub fn meter(ui: &mut Ui, id_salt: &str, peak: f32, rms: f32, size: Vec2, orientation: Meter) {
    let id = ui.id().with("jamstream-meter").with(id_salt);
    let dt = ui.input(|i| i.stable_dt).clamp(0.0, 0.1);

    let mut s: MeterState = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    let peak_db = to_db(peak);
    let rms_db = to_db(rms);
    // Fast attack: jump up instantly. Slow release: decay at a fixed rate,
    // never below the live value.
    s.peak_db = if peak_db >= s.peak_db {
        peak_db
    } else {
        (s.peak_db - PEAK_RELEASE_DB_PER_S * dt).max(peak_db)
    };
    s.rms_db = if rms_db >= s.rms_db {
        rms_db
    } else {
        (s.rms_db - RMS_RELEASE_DB_PER_S * dt).max(rms_db)
    };
    if s.peak_db >= s.hold_db {
        s.hold_db = s.peak_db;
        s.hold_age = 0.0;
    } else {
        s.hold_age += dt;
        if s.hold_age > HOLD_SECS {
            s.hold_db = (s.hold_db - HOLD_RELEASE_DB_PER_S * dt).max(s.peak_db);
        }
    }
    ui.data_mut(|d| d.insert_temp(id, s));

    let (rect, _response) = ui.allocate_exact_size(size, Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    use egui::emath::GuiRounding;
    let p = theme::palette_of(ui);
    let ppp = ui.pixels_per_point();
    let painter = ui.painter();
    let rect = rect.round_to_pixels(ppp);
    painter.rect_filled(rect, CornerRadius::same(2), p.well);
    let inner = rect.shrink(1.0);
    let frac = |db: f32| (db - FLOOR_DB) / -FLOOR_DB;

    // Discrete LED segments; unlit segments stay faintly visible so the
    // scale reads even in silence.
    let length = match orientation {
        Meter::Vertical => inner.height(),
        Meter::Horizontal => inner.width(),
    };
    let count = (length / (SEG_LEN + SEG_GAP)).floor().max(1.0) as usize;
    let pitch = length / count as f32;
    let rms_frac = frac(s.rms_db);
    let peak_seg = ((frac(s.peak_db) * count as f32) as usize).min(count - 1);
    let hold_seg = ((frac(s.hold_db) * count as f32) as usize).min(count - 1);
    let hold_live = s.hold_db > FLOOR_DB + 0.5;
    for i in 0..count {
        let f0 = i as f32 / count as f32;
        let f_mid = (i as f32 + 0.5) / count as f32;
        let color = zone_color(FLOOR_DB + f_mid * -FLOOR_DB, p);
        let lit = f_mid <= rms_frac
            || (i == peak_seg && s.peak_db > FLOOR_DB + 0.5)
            || (i == hold_seg && hold_live);
        let color = if lit {
            color
        } else {
            theme::blend(p.well, color, 0.16)
        };
        let seg =
            segment_rect(inner, f0 * length, pitch - SEG_GAP, orientation).round_to_pixels(ppp);
        painter.rect_filled(seg, 0.0, color);
    }
}

const SEG_LEN: f32 = 3.0;
const SEG_GAP: f32 = 1.0;

/// One segment starting `offset` px from the meter's zero end.
fn segment_rect(inner: Rect, offset: f32, len: f32, orientation: Meter) -> Rect {
    match orientation {
        Meter::Vertical => Rect::from_min_max(
            pos2(inner.left(), inner.bottom() - offset - len),
            pos2(inner.right(), inner.bottom() - offset),
        ),
        Meter::Horizontal => Rect::from_min_max(
            pos2(inner.left() + offset, inner.top()),
            pos2(inner.left() + offset + len, inner.bottom()),
        ),
    }
}
