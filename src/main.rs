mod app;
#[cfg(target_os = "macos")]
mod platform_macos;
mod storage;

use app::ReferenceBoardApp;
use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("img-ref-tool")
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([640.0, 480.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };

    eframe::run_native(
        "img-ref-tool",
        options,
        Box::new(|creation_context| Ok(Box::new(ReferenceBoardApp::new(creation_context)))),
    )
}
