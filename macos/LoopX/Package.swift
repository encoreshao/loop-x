// swift-tools-version:5.9
import Foundation
import PackageDescription

// Command Line Tools ship swift-testing's macro plugin outside the default plugin
// search path, so `swift test` cannot expand @Test/@Suite without a hint. Full Xcode
// toolchains find it on their own, in which case this flag list stays empty.
let cltTestingPlugins = "/Library/Developer/CommandLineTools/usr/lib/swift/host/plugins/testing"
let testSwiftSettings: [SwiftSetting] =
    FileManager.default.fileExists(atPath: cltTestingPlugins + "/libTestingMacros.dylib")
    ? [.unsafeFlags(["-plugin-path", cltTestingPlugins])] : []

let package = Package(
    name: "LoopX",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "LoopXKit", targets: ["LoopXKit"]),
        .executable(name: "LoopX", targets: ["LoopX"]),
    ],
    dependencies: [
        .package(url: "https://github.com/sparkle-project/Sparkle", exact: "2.6.4"),
    ],
    targets: [
        .target(name: "LoopXKit"),
        .executableTarget(
            name: "LoopX",
            dependencies: ["LoopXKit", .product(name: "Sparkle", package: "Sparkle")],
            // The build script puts Sparkle.framework in Contents/Frameworks.
            linkerSettings: [.unsafeFlags(["-Xlinker", "-rpath", "-Xlinker", "@executable_path/../Frameworks"])]
        ),
        .testTarget(name: "LoopXKitTests", dependencies: ["LoopXKit"], swiftSettings: testSwiftSettings),
    ]
)
