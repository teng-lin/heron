// build.rs for heron-pipeline.
//
// heron-pipeline depends on heron-vault, heron-zoom, and heron-speech,
// each of which links a Swift static library referencing
// `@rpath/libswift_Concurrency.dylib`. Cargo's `rustc-link-arg`
// doesn't propagate to dependent crates' binaries, so the bridge
// crates' rpath emit doesn't reach this crate's tests. Re-emit it here
// so this crate's lib + tests don't SIGABRT at load on hosts whose dyld
// shared cache doesn't ship libswift_Concurrency (current example:
// GitHub Actions macos-14).
//
// See `crates/heron-cli/build.rs` for the canonical write-up.

#![allow(clippy::expect_used, clippy::unwrap_used)]

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target_vendor = std::env::var("CARGO_CFG_TARGET_VENDOR").unwrap_or_default();
    if target_vendor == "apple" {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
