//! Embeds `assets/icon.ico` as the `.exe`'s own file icon (what File
//! Explorer/taskbar show *before* the window - egui's `.with_icon()` in
//! `main.rs` only controls the *window*/titlebar icon while running,
//! that's a separate thing). No-op on non-Windows targets.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.compile().expect(
            "Failed to embed assets/icon.ico as the exe icon (winresource - needs the MSVC \
             linker/rc.exe, same as the rest of the Windows build)",
        );
    }
}
