// Prevent an extra console window from popping up on Windows in release
// builds. Debug builds still get a console, which is handy for the
// eprintln!/println! diagnostics.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod epg;
mod logos;
mod player;
mod recordings;
mod settings;
mod tvh;
mod update;

/// Window/taskbar icon, shown while the app is running (title bar, Alt+Tab,
/// taskbar). Decoded once at startup from the same PNG the `.exe`'s own
/// file icon (`assets/icon.ico`, embedded via `build.rs`) is generated
/// from - see `assets/generate_icon.py` for how both were made.
fn load_icon() -> eframe::egui::IconData {
    let bytes = include_bytes!("../assets/icon-256.png");
    let image = image::load_from_memory(bytes)
        .expect("embedded assets/icon-256.png should always decode")
        .into_rgba8();
    let (width, height) = image.dimensions();
    eframe::egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([600.0, 400.0])
            .with_title("TVH Client")
            .with_icon(load_icon()),
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
