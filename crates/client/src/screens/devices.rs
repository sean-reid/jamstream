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

impl DevicesScreen {
    /// The full-screen route: a focused column like home and the wizard.
    pub fn ui(&mut self, ui: &mut Ui, catalog: &DeviceCatalog, levels: &LevelsView) {
        theme::focused_column(ui, 560.0, |ui| self.panels_ui(ui, catalog, levels));
    }

    /// The bare panels; also embedded in the settings window.
    pub fn panels_ui(&mut self, ui: &mut Ui, catalog: &DeviceCatalog, levels: &LevelsView) {
        theme::panel(ui).show(ui, |ui| {
            ui.set_width(ui.available_width().min(560.0));
            ui.label(theme::title(ui, "Devices"));
            egui::Grid::new("device-grid")
                .num_columns(2)
                .spacing(vec2(theme::SPACE_LG, 6.0))
                .show(ui, |ui| {
                    ui.label(theme::muted(ui, "Capture"));
                    device_combo(ui, "capture", &catalog.capture, &mut self.capture_idx);
                    ui.end_row();
                    ui.label(theme::muted(ui, "Playback"));
                    device_combo(ui, "playback", &catalog.playback, &mut self.playback_idx);
                    ui.end_row();
                });
        });
        ui.add_space(theme::SPACE_MD);
        theme::panel(ui).show(ui, |ui| {
            ui.set_width(ui.available_width().min(560.0));
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
        });
        ui.add_space(theme::SPACE_MD);
        theme::panel(ui).show(ui, |ui| {
            ui.set_width(ui.available_width().min(560.0));
            ui.label(theme::title(ui, "Input level"));
            ui.horizontal(|ui| {
                meter(
                    ui,
                    "devices-input",
                    levels.input_peak,
                    levels.input_rms,
                    vec2(220.0, 12.0),
                    Meter::Horizontal,
                );
                ui.label(theme::muted(ui, "speak or play to check the meter moves"));
            });
        });
    }
}

fn device_combo(ui: &mut Ui, id: &str, devices: &[DeviceInfo], selected: &mut usize) {
    if devices.is_empty() {
        ui.label(theme::muted(ui, "no devices found"));
        return;
    }
    *selected = (*selected).min(devices.len() - 1);
    ComboBox::from_id_salt(id)
        .width(280.0)
        .selected_text(devices[*selected].name.clone())
        .show_ui(ui, |ui| {
            for (i, dev) in devices.iter().enumerate() {
                ui.selectable_value(selected, i, dev.name.clone());
            }
        });
}
