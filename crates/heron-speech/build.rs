// Builds swift/whisperkit-helper as a static library and links it
// into heron-speech. The helper now depends on the upstream
// `argmaxinc/WhisperKit` Swift package (pinned in
// swift/whisperkit-helper/Package.swift); `swift build` therefore
// requires network access on first run. CI implications are tracked
// in docs/manual-test-matrix.md "WhisperKit STT backend".
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

    // WhisperKit pulls in CoreML for model inference, AVFoundation
    // for audio decode, and Accelerate for the spectrogram path.
    // Foundation is the always-on dep. We link them up-front so the
    // host crate doesn't have to repeat the list.
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=CoreML");
    println!("cargo:rustc-link-lib=framework=AVFoundation");
    println!("cargo:rustc-link-lib=framework=Accelerate");
    println!("cargo:rustc-link-lib=framework=CoreAudio");
    println!("cargo:rustc-link-lib=framework=AudioToolbox");

    // Swift concurrency (`Task`, async/await, AsyncStream,
    // withTaskGroup, withCheckedContinuation) lives in
    // libswift_Concurrency.dylib. The current API symbols
    // (TaskGroup with `isolation:`, etc.) live ONLY in the macOS
    // SDK's `usr/lib/swift/libswift_Concurrency.tbd`, NOT in the
    // toolchain's `swift-5.5/macosx` back-deploy archive. We add
    // both:
    //   - SDK swift dir as a link-search path so the linker resolves
    //     `_$ss13withTaskGroup… isolation:` against the system stub.
    //   - toolchain `swift-5.5/macosx` for the back-deploy compat
    //     archives (older OS versions don't have these symbols at
    //     runtime; the back-deploy lib provides them).
    //   - rpath `/usr/lib/swift` so dyld finds the runtime via the
    //     shared cache at load time.
    //   - explicit `-lswift_Concurrency` etc. WhisperKit pulls in
    //     these symbols deep enough that autolink alone is unreliable
    //     across a static-archive boundary.
    // Order matters: cargo emits `-L` flags in declaration order,
    // and ld resolves `-l` lookups against the *first* matching dir.
    // The SDK ships current-Xcode TBDs that include the modern
    // concurrency symbols (`withTaskGroup … isolation:`); the
    // toolchain `swift-5.5/macosx` dir has an older back-deploy
    // dylib that's missing them. Search SDK first.
    let sdk_swift = sdk_swift_lib_dir();
    println!("cargo:rustc-link-search=native={}", sdk_swift.display());
    let toolchain_lib = swift_runtime_lib_dir();
    println!("cargo:rustc-link-search=native={}", toolchain_lib.display());
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    println!("cargo:rustc-link-lib=dylib=swift_Concurrency");
    println!("cargo:rustc-link-lib=dylib=swift_StringProcessing");
    println!("cargo:rustc-link-lib=dylib=swiftCore");

    // WhisperKit + its transitive deps autolink the
    // `swiftCompatibility56` / `swiftCompatibilityConcurrency` /
    // `swiftCompatibilityPacks` static archives. They live in
    // <toolchain>/usr/lib/swift/macosx (NOT the swift-5.5/macosx
    // back-deploy dir). Add that dir to the link search and force
    // the libs in by name so a bare `cargo build` finds them
    // without the user setting LIBRARY_PATH manually.
    let macosx_compat = swift_macosx_compat_lib_dir();
    println!("cargo:rustc-link-search=native={}", macosx_compat.display());
    println!("cargo:rustc-link-lib=static=swiftCompatibility56");
    println!("cargo:rustc-link-lib=static=swiftCompatibilityConcurrency");
    println!("cargo:rustc-link-lib=static=swiftCompatibilityPacks");

    // WhisperKit (and its deps) require macOS 13+ as a build target.
    // Cargo defaults to a much older macOS; if the host crate doesn't
    // raise the deployment target, the linker emits a sea of
    // "object file built for newer 'macOS' version 13.0" warnings and
    // — more importantly — fails to autolink several Swift symbols.
    // We bump the deployment target to 14.0 to match Package.swift.
    println!("cargo:rustc-env=MACOSX_DEPLOYMENT_TARGET=14.0");
    println!("cargo:rustc-link-arg=-mmacosx-version-min=14.0");

    println!("cargo:rerun-if-changed={}", swift_dir.display());
    println!("cargo:rerun-if-changed=build.rs");
}

/// Resolve `<SDK>/usr/lib/swift`. The Swift concurrency stubs that
/// match Xcode 16's runtime live here and are not back-deployed into
/// the toolchain's `swift-5.5/macosx` archive.
#[cfg(target_vendor = "apple")]
fn sdk_swift_lib_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    use std::process::Command;

    let sdk = Command::new("xcrun")
        .args(["--show-sdk-path"])
        .output()
        .expect("xcrun --show-sdk-path");
    assert!(sdk.status.success(), "xcrun --show-sdk-path failed");
    let sdk_path = PathBuf::from(
        String::from_utf8(sdk.stdout)
            .expect("sdk path is utf8")
            .trim(),
    );
    sdk_path.join("usr/lib/swift")
}

#[cfg(target_vendor = "apple")]
fn swift_macosx_compat_lib_dir() -> std::path::PathBuf {
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
    toolchain.join("lib/swift/macosx")
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
