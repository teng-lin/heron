// swift-tools-version:5.9
//
// Scaffold for the §4 WhisperKit Swift bridge. v0 ships the @_cdecl
// surface only — no `WhisperKit` package dependency is declared yet,
// so `swift build` runs offline and CI doesn't need network access
// to compile the bridge.
//
// Once the model-download UX in week 11 / §13.3 lands, add:
//
//     dependencies: [
//         .package(url: "https://github.com/argmaxinc/WhisperKit",
//                  from: "0.x"),
//     ],
//
// and replace the stub bodies in Sources/WhisperKitHelper.swift with
// real `WhisperKit.transcribe` calls. The @_cdecl wire shape stays
// the same; the Rust side will not need to change.

import PackageDescription

let package = Package(
    name: "WhisperKitHelper",
    platforms: [.macOS(.v14)],
    products: [
        .library(
            name: "WhisperKitHelper",
            type: .static,
            targets: ["WhisperKitHelper"]
        ),
    ],
    targets: [
        .target(name: "WhisperKitHelper"),
    ]
)
