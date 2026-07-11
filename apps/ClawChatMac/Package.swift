// swift-tools-version: 5.9

import PackageDescription

let package = Package(
    name: "ClawChatMac",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "ClawChatMac", targets: ["ClawChatMac"]),
    ],
    targets: [
        .executableTarget(name: "ClawChatMac"),
        .testTarget(name: "ClawChatMacTests", dependencies: ["ClawChatMac"]),
    ],
    swiftLanguageVersions: [.v5]
)
