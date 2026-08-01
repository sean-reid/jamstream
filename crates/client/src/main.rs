//! The jamstream desktop app. `--demo` opens a live fake session so the
//! whole interface can be exercised without a server.

use jamstream_client::app::JamApp;

/// The committed 512px render of the app icon (source of record:
/// assets/icon/jamstream.svg; regenerate with scripts/render-icon.sh).
const ICON_PNG: &[u8] = include_bytes!("../assets/icon/jamstream-512.png");

fn main() -> eframe::Result {
    // First, before anything can fail: the subscriber and the panic hook are
    // what make a failure before the window opens visible at all.
    jamstream_client::logging::init();
    let demo = std::env::args().any(|a| a == "--demo");
    let icon = eframe::icon_data::from_png_bytes(ICON_PNG)
        .expect("assets/icon/jamstream-512.png is a valid PNG");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([800.0, 600.0])
            .with_title("jamstream")
            .with_icon(icon),
        ..Default::default()
    };
    eframe::run_native(
        "jamstream",
        options,
        Box::new(move |_cc| {
            let app = if demo {
                JamApp::demo()
            } else {
                JamApp::with_system_devices()
            };
            Ok(Box::new(app))
        }),
    )
}
