// swift-tools-version: 5.9
import PackageDescription

// The macOS app: a native SwiftUI/AppKit front end over the shared Rust core.
//
// `VocalScopeCore` and `vocalscope_coreFFI` contain generated code only (the
// UniFFI Swift bindings and their C header). `build.sh` regenerates them and
// places the core's static library in `.corelib/` before invoking SwiftPM.
let package = Package(
    name: "VocalScope",
    platforms: [.macOS(.v14)],
    targets: [
        .target(name: "vocalscope_coreFFI", path: "Sources/vocalscope_coreFFI"),
        .target(
            name: "VocalScopeCore",
            dependencies: ["vocalscope_coreFFI"],
            path: "Sources/VocalScopeCore",
            swiftSettings: [.unsafeFlags(["-suppress-warnings"])],
            linkerSettings: [
                .unsafeFlags(["-L", "\(Context.packageDirectory)/.corelib"]),
                .linkedLibrary("vocalscope_core"),
                // ONNX Runtime, linked into the core, is C++.
                .linkedLibrary("c++"),
                .linkedLibrary("iconv"),
                .linkedFramework("AudioToolbox"),
                .linkedFramework("CoreAudio"),
                .linkedFramework("CoreFoundation"),
                .linkedFramework("Foundation"),
                .linkedFramework("IOKit"),
            ]
        ),
        .executableTarget(
            name: "VocalScope",
            dependencies: ["VocalScopeCore"],
            path: "Sources/VocalScope"
        ),
    ]
)
