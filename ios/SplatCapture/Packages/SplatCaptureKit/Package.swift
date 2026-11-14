// swift-tools-version:5.9
import PackageDescription

// Everything of the capture app that needs neither ARKit nor UIKit: the frame
// encoder, the .gscap file, the pairing URI, keyframe selection, flow control,
// depth conversion and the stream messages. Builds and tests on macOS:
//
//     swift test
//
let package = Package(
    name: "SplatCaptureKit",
    platforms: [
        .iOS(.v17),
        .macOS(.v13),
    ],
    products: [
        .library(name: "SplatCaptureKit", targets: ["SplatCaptureKit"]),
    ],
    targets: [
        .target(name: "SplatCaptureKit"),
        .testTarget(
            name: "SplatCaptureKitTests",
            dependencies: ["SplatCaptureKit"],
            // Tests/SplatCaptureKitTests/Fixtures, copied as it is (with its
            // frames/ subdirectory) into the test bundle: Bundle.module.
            resources: [.copy("Fixtures")]
        ),
    ]
)
