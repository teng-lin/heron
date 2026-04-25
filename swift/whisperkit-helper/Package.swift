// swift-tools-version:5.9
//
// Swift bridge for the §4 WhisperKit integration. The @_cdecl surface
// stays stable; the implementation in Sources/WhisperKitHelper.swift
// now calls into the upstream WhisperKit Swift package.
//
// === Network requirement ===
// `swift build` resolves the WhisperKit dependency from GitHub on
// first build. Network access is required at build time. CI must
// either be allowed network egress to github.com / Swift Package
// Registry mirrors, or the `.build/` checkout must be vendored as
// part of the repo. The latter is out of scope for this PR; flagged
// in the commit message body so the CI gardener knows to plan for
// it before turning the WhisperKit backend on by default.
//
// === Pinned WhisperKit version ===
// argmaxinc/WhisperKit v0.18.0 — the latest stable release at the
// time of writing (commit e2adabbe7d98dc4d0ab9a5b75424ecc42a9cdbef,
// see `git ls-remote --tags https://github.com/argmaxinc/WhisperKit`).
// Pin is `.exact` so an upstream re-tag or a transitive resolver
// move can't silently swap our ABI. Bump deliberately when we want
// a new WhisperKit release.

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
    dependencies: [
        // Pinned to a specific release tag for reproducibility. The
        // commit hash is recorded in the comment block above so a
        // forced-push or re-tag on upstream doesn't go unnoticed.
        .package(
            url: "https://github.com/argmaxinc/WhisperKit",
            exact: "0.18.0"
        ),
    ],
    targets: [
        .target(
            name: "WhisperKitHelper",
            dependencies: [
                .product(name: "WhisperKit", package: "WhisperKit"),
            ]
        ),
    ]
)
