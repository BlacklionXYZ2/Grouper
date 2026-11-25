// swift-tools-version:5.10
// GrouperKit: the Rust core (via UniFFI) packaged for Swift.
// Run scripts/build-apple.sh first to build lib/libgrouper_ffi.a and the bindings.
//
// This links a static library with -L/-l flags so it builds with the Command Line
// Tools alone. Once Xcode is installed, swap this for an XCFramework binaryTarget
// (needed for iOS device + simulator slices).
import PackageDescription
import Foundation

let libDir = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("lib").path

var products: [Product] = [
    .library(name: "GrouperAPI", targets: ["GrouperAPI"]),
]
var targets: [Target] = [
    .target(name: "GrouperAPI"),
    .testTarget(name: "GrouperAPITests", dependencies: ["GrouperAPI"]),
    .testTarget(name: "GrouperAPILiveTests", dependencies: ["GrouperAPI"]),
]

#if !os(Windows)
products += [
    .library(name: "GrouperKit", targets: ["GrouperKit"]),
    .executable(name: "grouper-mac", targets: ["grouper-mac"]),
]
targets += [
    .target(
        name: "grouper_ffiFFI",
        path: "Sources/grouper_ffiFFI",
        linkerSettings: [
            .unsafeFlags(["-L\(libDir)", "-lgrouper_ffi"]),
            .linkedFramework("Security"),
            .linkedFramework("SystemConfiguration"),
            .linkedFramework("CoreFoundation"),
        ]
    ),
    .target(name: "GrouperKit", dependencies: ["grouper_ffiFFI"]),
    .executableTarget(name: "grouper-mac", dependencies: ["GrouperKit"]),
]
#endif

let package = Package(
    name: "GrouperKit",
    platforms: [.macOS(.v14)],
    products: products,
    targets: targets
)
