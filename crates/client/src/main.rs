//! The jamstream desktop app. `--demo` opens a live fake session so the
//! whole interface can be exercised without a server.

use jamstream_client::app::JamApp;

fn main() -> eframe::Result {
    let demo = std::env::args().any(|a| a == "--demo");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([800.0, 600.0])
            .with_title("jamstream"),
        ..Default::default()
    };
    eframe::run_native(
        "jamstream",
        options,
        Box::new(move |_cc| {
            let app = if demo { JamApp::demo() } else { JamApp::new() };
            Ok(Box::new(app))
        }),
    )
}
