// build.rs for heron-orchestrator.
//
// heron-orchestrator depends on heron-vault, which links a Swift
// static library referencing `@rpath/libswift_Concurrency.dylib`.
// The vault crate's build script emits the rpath flag, but Cargo's
// `rustc-link-arg` doesn't propagate to dependent crates' binaries.
// Re-emit here so this crate's test binaries don't SIGABRT at load.
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
