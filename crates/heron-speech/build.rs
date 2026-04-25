// Builds swift/whisperkit-helper as a static library and links it
// into heron-speech. v0 doesn't depend on the WhisperKit package; the
// bridge ships @_cdecl wrappers that return NotYetImplemented until
// the §4 / week-4 work drops the real WhisperKit calls in.
//
// Pattern mirrors crates/heron-vault/build.rs (canonical Swift bridge,
// see docs/swift-bridge-pattern.md).

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[cfg(not(target_vendor = "apple"))]
fn main() {
    // WhisperKit is Apple-only; off-Apple this crate builds without
    // the bridge and code that calls into it must be gated by
    // `cfg(target_vendor = "apple")`.
    println!("cargo:rerun-if-changed=build.rs");
}

#[cfg(target_vendor = "apple")]
fn main() {
    use std::path::Path;
    use std::process::Command;

    let manifest =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by cargo");
    let swift_dir = Path::new(&manifest).join("../../swift/whisperkit-helper");

    let cargo_arch = std::env::var("CARGO_CFG_TARGET_ARCH")
        .expect("CARGO_CFG_TARGET_ARCH is set by cargo for build scripts");
    let arch = match cargo_arch.as_str() {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => panic!("unsupported target arch for swift bridge: {other}"),
    };

    let status = Command::new("swift")
        .args(["build", "-c", "release", "--arch", arch])
        .current_dir(&swift_dir)
        .status()
        .expect("invoke swift build");
    assert!(status.success(), "swift build failed for whisperkit-helper");

    let triple = format!("{arch}-apple-macosx");
    let lib_dir = swift_dir.join(format!(".build/{triple}/release"));
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=WhisperKitHelper");
    // Foundation only — no AVFoundation or CoreML yet (those land
    // when the real WhisperKit dep goes in).
    println!("cargo:rustc-link-lib=framework=Foundation");

    // No async/await yet in the v0 stubs, but link the Swift runtime
    // search path anyway so adding it later is a one-line change.
    let toolchain_lib = swift_runtime_lib_dir();
    println!("cargo:rustc-link-search=native={}", toolchain_lib.display());
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    println!("cargo:rerun-if-changed={}", swift_dir.display());
    println!("cargo:rerun-if-changed=build.rs");
}

#[cfg(target_vendor = "apple")]
fn swift_runtime_lib_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    use std::process::Command;

    let swiftc = Command::new("xcrun")
        .args(["-f", "swiftc"])
        .output()
        .expect("xcrun -f swiftc");
    assert!(swiftc.status.success(), "xcrun -f swiftc failed");
    let swiftc_path = PathBuf::from(
        String::from_utf8(swiftc.stdout)
            .expect("swiftc path is utf8")
            .trim(),
    );
    let toolchain = swiftc_path
        .parent()
        .and_then(|p| p.parent())
        .expect("xcrun -f swiftc returned an unexpected path layout");
    toolchain.join("lib/swift-5.5/macosx")
}
