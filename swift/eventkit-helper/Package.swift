// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "EventKitHelper",
    platforms: [.macOS(.v14)],
    products: [
        .library(
            name: "EventKitHelper",
            type: .static,
            targets: ["EventKitHelper"]
        ),
    ],
    targets: [
        .target(name: "EventKitHelper"),
    ]
)
