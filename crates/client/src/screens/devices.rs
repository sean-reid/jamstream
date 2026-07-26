//! Audio device setup. Enumeration is fed in through [`DeviceCatalog`];
//! real device discovery arrives with the audio pass, the demo lists two
//! fake devices per direction.

use egui::{ComboBox, Ui, vec2};

use crate::runtime::LevelsView;
use crate::theme;
use crate::widgets::{Meter, meter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
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
        };
        DeviceCatalog {
            capture: vec![dev("Scarlett 2i2 input"), dev("Built-in microphone")],
            playback: vec![dev("Scarlett 2i2 output"), dev("Built-in speakers")],
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
    pub fn ui(&mut self, ui: &mut Ui, catalog: &DeviceCatalog, levels: &LevelsView) {
        ui.add_space(theme::SPACE_MD);
        theme::panel(ui).show(ui, |ui| {
            ui.set_width(ui.available_width().min(560.0));
            ui.label("Devices");
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
            ui.label("Buffer size");
            ui.label(theme::muted(
                ui,
                "Smaller buffers cut latency; pick the smallest that stays clean.",
            ));
            for frames in BUFFER_CHOICES {
                ui.radio_value(&mut self.buffer_frames, frames, buffer_label(frames));
            }
        });
        ui.add_space(theme::SPACE_MD);
        theme::panel(ui).show(ui, |ui| {
            ui.set_width(ui.available_width().min(560.0));
            ui.label("Input level");
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
