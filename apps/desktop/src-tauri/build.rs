// `tauri_build::build()` reads tauri.conf.json + capabilities/ and
// stamps generated bindings into OUT_DIR. Per `docs/implementation.md`
// §13 (Tauri shell, week 11) — the v0 scaffold is enough for the
// onboarding routes to land.
#![allow(clippy::expect_used, clippy::unwrap_used)]

fn main() {
    tauri_build::build();
}
