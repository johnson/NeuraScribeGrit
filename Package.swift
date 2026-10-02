// swift-tools-version: 5.9
import PackageDescription

// The grit-backed Git backend, replacing the gix/UniFFI package.
//
// Structured like NeuraScribeGit: the static library lives in an xcframework,
// a systemLibrary target supplies the C module, and the Swift layer wraps it.
// The difference is that the Swift here is hand-written rather than generated —
// grit is consumed through a plain C ABI of 26 functions, not UniFFI.
let package = Package(
    name: "NeuraScribeGrit",
    platforms: [
        .iOS(.v15),
        .macOS(.v13)
    ],
    products: [
        .library(
            name: "NeuraScribeGrit",
            targets: ["NeuraScribeGrit"]
        ),
        // Throwaway diagnostic harness; not shipped, not depended on.
        .executable(name: "gritDiag", targets: ["gritDiag"]),
        .executable(name: "gritBench", targets: ["gritBench"])
    ],
    targets: [
        // Static library built from the Rust shim in this same package.
        // No module.modulemap inside (avoids the Xcode duplicate-conflict the
        // gix package hit); the C module comes from the systemLibrary below.
        .binaryTarget(
            name: "neurascribe_gritFFI_bin",
            path: "neurascribe_gritFFI.xcframework"
        ),
        .systemLibrary(
            name: "neurascribe_gritFFI",
            path: "Sources/neurascribe_gritFFI",
            pkgConfig: nil,
            providers: nil
        ),
        .target(
            name: "NeuraScribeGrit",
            dependencies: [
                .target(name: "neurascribe_gritFFI"),
                .target(name: "neurascribe_gritFFI_bin")
            ],
            path: "Sources/NeuraScribeGrit"
        ),
        .executableTarget(
            name: "gritDiag",
            dependencies: ["NeuraScribeGrit"],
            path: "Sources/gritDiag"
        ),
        .executableTarget(
            name: "gritBench",
            dependencies: ["NeuraScribeGrit"],
            path: "Sources/gritBench"
        ),
        .testTarget(
            name: "NeuraScribeGritTests",
            dependencies: ["NeuraScribeGrit"],
            // Lowercase on purpose. This directory also holds the Rust
            // integration tests (differential.rs, index_ops.rs and the rest),
            // and Cargo only auto-discovers a lowercase `tests/`. The macOS
            // volume is case-insensitive so both tools find the folder either
            // way, but SwiftPM validates the declared casing against the
            // disk and fails the whole package when they disagree -- so the
            // manifest follows the disk rather than the Swift convention.
            path: "tests/NeuraScribeGritTests"
        )
    ]
)
