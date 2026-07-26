//! Level meter with real ballistics: instant attack, timed release, and a
//! 1.5 s peak-hold tick. Green to -12 dBFS, amber to -3, red above. Drawn
//! entirely with the painter; ballistic state lives in egui memory keyed by
//! id, advanced with `stable_dt`.

use egui::{Color32, CornerRadius, Rect, Sense, Stroke, Ui, Vec2, pos2};

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
    let p = theme::palette_of(ui);
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(2),
        p.surface0,
        Stroke::new(1.0, p.border),
        egui::StrokeKind::Inside,
    );
    let inner = rect.shrink(1.0);
    let frac = |db: f32| (db - FLOOR_DB) / -FLOOR_DB;

    // Solid rms fill, split into color zones.
    let zones = [
        (FLOOR_DB, AMBER_FROM_DB, p.meter_green),
        (AMBER_FROM_DB, RED_FROM_DB, p.meter_amber),
        (RED_FROM_DB, 0.0, p.meter_red),
    ];
    for (from, to, color) in zones {
        let hi = s.rms_db.min(to);
        if hi <= from {
            continue;
        }
        let seg = segment(inner, frac(from), frac(hi), orientation);
        painter.rect_filled(seg, 0.0, color);
    }
    // Peak line and the 1.5 s hold tick.
    paint_tick(
        painter,
        inner,
        frac(s.peak_db),
        orientation,
        zone_color(s.peak_db, p),
    );
    if s.hold_db > FLOOR_DB + 0.5 {
        paint_tick(
            painter,
            inner,
            frac(s.hold_db),
            orientation,
            zone_color(s.hold_db, p),
        );
    }
}

fn segment(inner: Rect, from: f32, to: f32, orientation: Meter) -> Rect {
    match orientation {
        Meter::Vertical => Rect::from_min_max(
            pos2(inner.left(), inner.bottom() - to * inner.height()),
            pos2(inner.right(), inner.bottom() - from * inner.height()),
        ),
        Meter::Horizontal => Rect::from_min_max(
            pos2(inner.left() + from * inner.width(), inner.top()),
            pos2(inner.left() + to * inner.width(), inner.bottom()),
        ),
    }
}

fn paint_tick(painter: &egui::Painter, inner: Rect, frac: f32, orientation: Meter, color: Color32) {
    let frac = frac.clamp(0.0, 1.0);
    match orientation {
        Meter::Vertical => {
            let y = inner.bottom() - frac * inner.height();
            painter.line_segment(
                [pos2(inner.left(), y), pos2(inner.right(), y)],
                Stroke::new(2.0, color),
            );
        }
        Meter::Horizontal => {
            let x = inner.left() + frac * inner.width();
            painter.line_segment(
                [pos2(x, inner.top()), pos2(x, inner.bottom())],
                Stroke::new(2.0, color),
            );
        }
    }
}
