// build.rs for heron-cli.
//
// heron-cli depends on heron-vault, heron-zoom, and heron-speech, each
// of which links a Swift static library. The Swift libs reference
// `@rpath/libswift_Concurrency.dylib`, which dyld resolves at load
// time. The bridge crates' build scripts emit
//
//   println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
//
// but `cargo:rustc-link-arg` only applies to binaries the *emitting*
// crate produces — it does NOT propagate to test binaries of crates
// that depend on the bridge crate. So heron-cli's lib-test binary
// is built without the rpath and SIGABRTs at load time on hosts whose
// dyld shared cache doesn't ship libswift_Concurrency (current
// example: GitHub Actions macos-14).
//
// Re-emit the rpath here so heron-cli's bin + tests + examples all
// carry it. We cannot rely on a workspace-level `.cargo/config.toml`
// because CI's `actions-rust-lang/setup-rust-toolchain` action sets
// `RUSTFLAGS=-D warnings`, which (per Cargo's precedence rules)
// overrides any `target.<cfg>.rustflags` in `.cargo/config.toml`
// rather than appending to it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Use the `CARGO_CFG_TARGET_VENDOR` env Cargo sets for build
    // scripts (so we read the *target* triple, not the build-script
    // host's). Cross-builds to non-Apple targets (Linux/Windows v2)
    // simply skip the rpath emit.
    let target_vendor = std::env::var("CARGO_CFG_TARGET_VENDOR").unwrap_or_default();
    if target_vendor == "apple" {
        // cargo:rustc-link-arg applies to ALL binary targets the
        // crate produces (bin, test, example, bench). For heron-cli
        // that's both `heron` (bin) and the lib + integration test
        // binaries — exactly what we need.
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
