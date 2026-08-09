// swift-tools-version:5.3

import PackageDescription

let package = Package(
    name: "tine-ios-folder-picker-native",
    // `.macOS` is NOT vestigial here, even though this package is iOS-only.
    // swift-rs cross-compiles by a deliberate hack (swift-rs-1.0.7
    // src-rs/build.rs:248-291): it runs a PLAIN `swift build`, which SwiftPM
    // resolves as a *macOS host* build — hence the hardcoded
    // `{arch}-apple-macosx` output directory at :286 — and only redirects the
    // real target through `-Xswiftc -target arm64-apple-iosNN.N[-simulator]`.
    // So SwiftPM evaluates macOS platform constraints regardless, and the
    // `Tauri` dependency declares `.macOS(.v10_13)`. Omitting it leaves this
    // package on the tools-version floor, below its own dependency.
    // `tauri-plugin-opener`, which links through this exact path and resolves
    // fine, declares both. Match it.
    platforms: [
        .macOS(.v10_13),
        .iOS(.v14),
    ],
    products: [
        .library(
            name: "tine-ios-folder-picker-native",
            type: .static,
            targets: ["tine-ios-folder-picker-native"]),
    ],
    dependencies: [
        .package(name: "Tauri", path: "../.tauri/tauri-api")
    ],
    targets: [
        .target(
            name: "tine-ios-folder-picker-native",
            dependencies: [
                .byName(name: "Tauri")
            ],
            path: "Sources")
    ]
)
