//! Audio device setup. Enumeration is fed in through [`DeviceCatalog`]:
//! the production app fills it from the platform backend, the demo and
//! the UI tests list two fake devices per direction.

use egui::{ComboBox, Ui, vec2};

use crate::runtime::LevelsView;
use crate::theme;
use crate::widgets::{Meter, meter, pick_row};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    /// Backend device id; `None` means the system default (demo entries).
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCatalog {
    pub capture: Vec<DeviceInfo>,
    pub playback: Vec<DeviceInfo>,
}

impl DeviceCatalog {
    pub fn demo() -> Self {
        let dev = |name: &str| DeviceInfo {
            name: name.to_owned(),
            id: None,
        };
        DeviceCatalog {
            capture: vec![dev("Scarlett 2i2 input"), dev("Built-in microphone")],
            playback: vec![dev("Scarlett 2i2 output"), dev("Built-in speakers")],
        }
    }

    /// Real enumeration, defaults listed first so index 0 is the device a
    /// fresh install uses.
    pub fn from_backend(devices: &[jamstream_audio_io::DeviceInfo]) -> Self {
        let pick = |direction| {
            let mut rows: Vec<&jamstream_audio_io::DeviceInfo> = devices
                .iter()
                .filter(|d| d.direction == direction)
                .collect();
            rows.sort_by_key(|d| !d.is_default);
            rows.into_iter()
                .map(|d| DeviceInfo {
                    name: d.name.clone(),
                    id: Some(d.id.clone()),
                })
                .collect()
        };
        DeviceCatalog {
            capture: pick(jamstream_audio_io::Direction::Capture),
            playback: pick(jamstream_audio_io::Direction::Playback),
        }
    }
}

/// Frames per buffer at 48 kHz; the protocol runs 2.5 ms media frames.
pub const BUFFER_CHOICES: [u32; 3] = [120, 240, 480];

fn buffer_label(frames: u32) -> String {
    format!("{frames} frames ({:.1} ms)", frames as f32 / 48.0)
}

pub struct DevicesScreen {
    pub capture_idx: usize,
    pub playback_idx: usize,
    pub buffer_frames: u32,
}

impl Default for DevicesScreen {
    fn default() -> Self {
        DevicesScreen {
            capture_idx: 0,
            playback_idx: 0,
            buffer_frames: BUFFER_CHOICES[0],
        }
    }
}

/// How a block is set. On its own screen each one is a panel, sitting on the
/// window surface like every other panel in the app. In the settings drawer
/// they are flat, because the drawer is already the panel and the sheets
/// never put a card inside a card: Destinations and Invites read the same
/// way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    Panel,
    Flat,
}

impl Block {
    fn show(self, ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
        match self {
            Block::Panel => {
                theme::panel(ui).show(ui, |ui| {
                    ui.set_width(ui.available_width().min(560.0));
                    add(ui);
                });
            }
            Block::Flat => add(ui),
        }
    }
}

impl DevicesScreen {
    /// The audio blocks, as the settings drawer's Audio tab.
    ///
    /// There is no full-screen route any more. It was reachable from nothing
    /// but a snapshot fixture, which made its baseline a picture of dead code
    /// (#191); the tab is where these controls have lived since the drawer
    /// existed, and it is reachable from every screen.
    ///
    /// Buffer size and the input meter come first because they are what a
    /// musician reaches for mid session, by ear and by meter, while the
    /// device pickers are set once. Whatever is last is what a short window
    /// puts behind a scroll, so the order is the priority.
    /// `refusal` is why there is no audio stream right now, when there is
    /// none: the pickers are what a musician came here to change, so the
    /// reason belongs beside them rather than only over the mixer they cannot
    /// see with this drawer open (#263).
    pub fn audio_ui(
        &mut self,
        ui: &mut Ui,
        block: Block,
        catalog: &DeviceCatalog,
        levels: &LevelsView,
        mouth_to_ear_ms: Option<f32>,
        refusal: Option<&str>,
    ) {
        self.buffer_ui(ui, block, mouth_to_ear_ms);
        ui.add_space(theme::SPACE_MD);
        input_level_ui(ui, block, levels);
        ui.add_space(theme::SPACE_MD);
        self.devices_ui(ui, block, catalog, refusal);
    }

    fn buffer_ui(&mut self, ui: &mut Ui, block: Block, mouth_to_ear_ms: Option<f32>) {
        block.show(ui, |ui| {
            ui.label(theme::title(ui, "Buffer size"));
            ui.label(theme::muted(
                ui,
                "Smaller buffers cut latency; pick the smallest that stays clean.",
            ));
            ui.add_space(theme::SPACE_XS);
            for frames in BUFFER_CHOICES {
                let label = buffer_label(frames);
                let selected = self.buffer_frames == frames;
                let response = pick_row(ui, &label, selected, true, |ui| {
                    ui.label(label.clone());
                });
                if response.clicked() {
                    self.buffer_frames = frames;
                }
            }
            // The number the choice is being traded against. The capture
            // buffer is one of the four terms in mouth to ear, so this moves
            // with the pick, and the same figure in the same monospace is in
            // the status bar. Outside a session nothing has been measured,
            // and a placeholder would be an instrument reading a made-up
            // number, so the row is absent instead.
            if let Some(ms) = mouth_to_ear_ms {
                ui.add_space(theme::SPACE_SM);
                ui.horizontal(|ui| {
                    ui.label(theme::mono(ui, format!("{ms:.1} ms")));
                    ui.label(theme::muted(ui, "mouth to ear"));
                });
            }
        });
    }

    fn devices_ui(
        &mut self,
        ui: &mut Ui,
        block: Block,
        catalog: &DeviceCatalog,
        refusal: Option<&str>,
    ) {
        block.show(ui, |ui| {
            ui.label(theme::title(ui, "Devices"));
            // The pickers take the width that is left rather than a fixed
            // 280 px. A row wider than its container widens the sheet around
            // it, which is how the settings sheet came to be half again as
            // wide as the width it asks for.
            let combo_w =
                (ui.available_width() - DEVICE_LABEL_W - theme::SPACE_LG).clamp(140.0, 280.0);
            egui::Grid::new("device-grid")
                .num_columns(2)
                .min_col_width(DEVICE_LABEL_W)
                .spacing(vec2(theme::SPACE_LG, 6.0))
                .show(ui, |ui| {
                    ui.label(theme::muted(ui, "Capture"));
                    device_combo(
                        ui,
                        "capture",
                        &catalog.capture,
                        &mut self.capture_idx,
                        combo_w,
                    );
                    ui.end_row();
                    ui.label(theme::muted(ui, "Playback"));
                    device_combo(
                        ui,
                        "playback",
                        &catalog.playback,
                        &mut self.playback_idx,
                        combo_w,
                    );
                    ui.end_row();
                });
            // The pick that will not open, in the device's own words, under
            // the picker that made it.
            if let Some(reason) = refusal {
                ui.add_space(theme::SPACE_XS);
                theme::reason(ui, format!("no audio device is running: {reason}"));
            }
        });
    }
}

/// The label column in the device grid: "Capture" and "Playback".
const DEVICE_LABEL_W: f32 = 66.0;

fn input_level_ui(ui: &mut Ui, block: Block, levels: &LevelsView) {
    block.show(ui, |ui| {
        ui.label(theme::title(ui, "Input level"));
        // The meter spans the full width and its line sits under it. Side by
        // side, the two needed more width than the drawer has.
        let w = ui.available_width();
        meter(
            ui,
            "devices-input",
            levels.input_peak,
            levels.input_rms,
            vec2(w, 12.0),
            Meter::Horizontal,
        );
        ui.label(theme::muted(ui, "speak or play to check the meter moves"));
    });
}

fn device_combo(ui: &mut Ui, id: &str, devices: &[DeviceInfo], selected: &mut usize, width: f32) {
    if devices.is_empty() {
        ui.label(theme::muted(ui, "no devices found"));
        return;
    }
    *selected = (*selected).min(devices.len() - 1);
    ComboBox::from_id_salt(id)
        .width(width)
        .selected_text(devices[*selected].name.clone())
        .show_ui(ui, |ui| {
            for (i, dev) in devices.iter().enumerate() {
                ui.selectable_value(selected, i, dev.name.clone());
            }
        });
}
