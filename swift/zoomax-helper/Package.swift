// swift-tools-version:5.9
//
// Scaffold for the §9 AXObserver Swift bridge. The Apple
// Accessibility framework (in ApplicationServices.framework on
// macOS) is the only dependency; no external Swift packages, so
// `swift build` is offline. Real impl lands week 6 / §9 once the
// week-0 spike fixture (per §3.3) lets us pin the
// `(role, subrole, identifier)` triple for Zoom's speaker indicator.

import PackageDescription

let package = Package(
    name: "ZoomAxHelper",
    platforms: [.macOS(.v14)],
    products: [
        .library(
            name: "ZoomAxHelper",
            type: .static,
            targets: ["ZoomAxHelper"]
        ),
    ],
    targets: [
        .target(name: "ZoomAxHelper"),
    ]
)
