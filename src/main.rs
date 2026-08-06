// Prevent an extra console window from popping up on Windows in release
// builds. Debug builds still get a console, which is handy for the
// eprintln!/println! diagnostics.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod player;
mod settings;
mod tvh;
mod update;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([600.0, 400.0])
            .with_title("TVH Client"),
        // Embedded mpv playback needs the glow (OpenGL) backend - it's how
        // we get access to the GL context/proc-address loader that mpv's
        // render API needs. Forced explicitly so a future eframe default
        // change can't silently switch us to wgpu.
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    eframe::run_native(
        "TVH Client",
        options,
        Box::new(|cc| Ok(Box::new(app::TvhApp::new(cc)))),
    )
}
