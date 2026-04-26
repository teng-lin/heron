// Builds swift/eventkit-helper as a static library and links it into
// heron-vault, along with the EventKit and Foundation system frameworks.
//
// See docs/archives/swift-bridge-pattern.md for the convention this build script
// is the canonical implementation of (per docs/archives/implementation.md §5.4).

// Build scripts panic on infrastructure failure; the workspace clippy
// deny list ("no expect/unwrap") only makes sense for runtime code.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[cfg(not(target_vendor = "apple"))]
fn main() {
    // EventKit only exists on Apple platforms. Off-Apple this crate
    // builds without the calendar bridge; calendar code paths must be
    // gated by `cfg(target_vendor = "apple")` on the Rust side.
    println!("cargo:rerun-if-changed=build.rs");
}

#[cfg(target_vendor = "apple")]
fn main() {
    use std::path::Path;
    use std::process::Command;

    let manifest =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by cargo");
    let swift_dir = Path::new(&manifest).join("../../swift/eventkit-helper");

    // Use Cargo's target-arch env var, NOT the build-script host's
    // `cfg!(target_arch)`. Build scripts run on the host architecture,
    // but `cargo build --target=…` may select a different target;
    // hard-coding the host arch produces a Swift archive whose triple
    // mismatches the Rust object files at link time. CI today builds
    // a single arch (macos-14 = arm64), but cross-build setups need
    // this distinction.
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
    assert!(status.success(), "swift build failed for eventkit-helper");

    let triple = format!("{arch}-apple-macosx");
    let lib_dir = swift_dir.join(format!(".build/{triple}/release"));
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=EventKitHelper");
    println!("cargo:rustc-link-lib=framework=EventKit");
    println!("cargo:rustc-link-lib=framework=Foundation");

    // The Swift bridge uses Task / async-await, which references
    // `@rpath/libswift_Concurrency.dylib`. We add:
    //
    // - the toolchain's swift-5.5/macosx as a *link-time* search path
    //   so unresolved symbols resolve at link;
    // - `/usr/lib/swift` as an rpath so dyld finds the runtime via
    //   the system shared cache at load time (no physical files there
    //   on macOS 12+, but the cache resolves the names).
    //
    // Adding the toolchain dir as an rpath instead would also work but
    // produces "Class … is implemented in both …" warnings because the
    // shared cache still gets loaded by EventKit's transitive deps.
    let toolchain_lib = swift_runtime_lib_dir();
    println!("cargo:rustc-link-search=native={}", toolchain_lib.display());
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    // Re-run when any source under the Swift package changes.
    println!("cargo:rerun-if-changed={}", swift_dir.display());
    println!("cargo:rerun-if-changed=build.rs");
}

#[cfg(target_vendor = "apple")]
fn swift_runtime_lib_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    use std::process::Command;

    // `xcrun -f swiftc` resolves to e.g.
    // /Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/swiftc
    // The runtime libs live at ../lib/swift-5.5/macosx relative to
    // that — Apple has kept the `swift-5.5` directory name as the
    // back-deployment compat dir; on Xcode 16 it still ships the
    // current-version runtimes.
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
    // swiftc is at <toolchain>/usr/bin/swiftc; the runtime libs are at
    // <toolchain>/usr/lib/swift-5.5/macosx.
    let toolchain = swiftc_path
        .parent()
        .and_then(|p| p.parent())
        .expect("xcrun -f swiftc returned an unexpected path layout");
    toolchain.join("lib/swift-5.5/macosx")
}
