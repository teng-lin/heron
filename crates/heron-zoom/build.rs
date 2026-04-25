// Builds swift/zoomax-helper as a static library and links it into
// heron-zoom. The Swift side exports four @_cdecl entry points and
// links only ApplicationServices + Foundation (Apple-only).
//
// Pattern mirrors crates/heron-vault/build.rs (canonical Swift bridge,
// see docs/swift-bridge-pattern.md). Real AX-tree walking lands week 6 / §9;
// v0 ships the bridge surface returning NotYetImplemented.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[cfg(not(target_vendor = "apple"))]
fn main() {
    // ApplicationServices is Apple-only; off-Apple this crate builds
    // without the AX bridge.
    println!("cargo:rerun-if-changed=build.rs");
}

#[cfg(target_vendor = "apple")]
fn main() {
    use std::path::Path;
    use std::process::Command;

    let manifest =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by cargo");
    let swift_dir = Path::new(&manifest).join("../../swift/zoomax-helper");

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
    assert!(status.success(), "swift build failed for zoomax-helper");

    let triple = format!("{arch}-apple-macosx");
    let lib_dir = swift_dir.join(format!(".build/{triple}/release"));
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=ZoomAxHelper");
    // ApplicationServices ships AX*; Foundation is the always-on
    // dep. AppKit isn't needed for the bridge itself (the real impl
    // uses NSRunningApplication via Foundation+CoreFoundation).
    println!("cargo:rustc-link-lib=framework=ApplicationServices");
    println!("cargo:rustc-link-lib=framework=Foundation");

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
