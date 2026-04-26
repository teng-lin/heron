// `tauri_build::build()` reads tauri.conf.json + capabilities/ and
// stamps generated bindings into OUT_DIR. Per `docs/archives/implementation.md`
// §13 (Tauri shell, week 11) — the v0 scaffold is enough for the
// onboarding routes to land.
//
// On Apple platforms heron-desktop transitively links Swift static
// libs via heron-vault (EventKit) and heron-zoom (AX). Each Swift lib
// references `@rpath/libswift_Concurrency.dylib`, which dyld resolves
// at load time. The bridge crates' build scripts emit
//
//     println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
//
// but `cargo:rustc-link-arg` only applies to binaries the *emitting*
// crate produces — it does NOT propagate to test/bin binaries of
// downstream crates. So heron-desktop's lib-test binary is built
// without the rpath and SIGABRTs at load time on hosts whose dyld
// shared cache doesn't ship libswift_Concurrency (current example:
// GitHub Actions macos-14). Mirror `heron-cli/build.rs` and re-emit
// the rpath here so heron-desktop's bin + lib-tests carry it.
#![allow(clippy::expect_used, clippy::unwrap_used)]

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Use the `CARGO_CFG_TARGET_VENDOR` env Cargo sets for build
    // scripts (so we read the *target* triple, not the build-script
    // host's). Cross-builds to non-Apple targets simply skip.
    let target_vendor = std::env::var("CARGO_CFG_TARGET_VENDOR").unwrap_or_default();
    if target_vendor == "apple" {
        // cargo:rustc-link-arg applies to ALL binary targets the
        // crate produces (bin, test, example, bench). For
        // heron-desktop that's both `heron-desktop` (bin) and the
        // lib + integration test binaries.
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }

    tauri_build::build();
}
