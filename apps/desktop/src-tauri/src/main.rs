// Hide the console window on Windows release builds. Per Tauri v2
// scaffolding convention.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    heron_desktop_lib::run();
}
