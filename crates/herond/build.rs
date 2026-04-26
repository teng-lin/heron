// build.rs for herond.
//
// herond transitively depends on heron-vault (via heron-session)
// which links a Swift static library referencing
// `@rpath/libswift_Concurrency.dylib`. The vault crate's build
// script emits the rpath flag, but Cargo's `rustc-link-arg` doesn't
// propagate to dependent crates' binaries. So we re-emit here for
// herond's bin + integration test binaries.
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
