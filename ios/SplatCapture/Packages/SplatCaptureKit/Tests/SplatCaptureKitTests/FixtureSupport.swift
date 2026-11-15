import CryptoKit
import Foundation
import XCTest

/// Access to the `Fixtures` directory next to this file: the test data
/// shared with the Python reference (tools/gscap.py) and with the Rust
/// server. `ios/SplatCapture/Fixtures` is a symbolic link to it.
///
/// The directory is a resource of this test target (see Package.swift), so
/// it is found in the test bundle on a Mac, in a simulator and on a real
/// device. Should the bundle not hold it, the path of this source file is
/// tried (it exists on the Mac that compiled the tests). The environment
/// variable SPLAT_FIXTURES, if set, wins over both.
enum Fixtures {
    static let directory: URL = {
        if let path = ProcessInfo.processInfo.environment["SPLAT_FIXTURES"], !path.isEmpty {
            return URL(fileURLWithPath: path, isDirectory: true)
        }
        if let bundled = Bundle.module.url(forResource: "Fixtures", withExtension: nil) {
            return bundled
        }
        // <package>/Tests/SplatCaptureKitTests/FixtureSupport.swift
        let source = URL(fileURLWithPath: #filePath)
        return source.deletingLastPathComponent().appendingPathComponent("Fixtures", isDirectory: true)
    }()

    static func require() throws {
        let marker = directory.appendingPathComponent("pairing_uris.json").path
        try XCTSkipUnless(
            FileManager.default.fileExists(atPath: marker),
            "Fixtures not found at \(directory.path); set SPLAT_FIXTURES to the Fixtures directory")
    }

    static func url(_ name: String) -> URL {
        return directory.appendingPathComponent(name)
    }

    static func data(_ name: String) throws -> Data {
        return try Data(contentsOf: url(name))
    }

    static func json(_ name: String) throws -> Any {
        return try JSONSerialization.jsonObject(with: data(name), options: [])
    }

    static func jsonObject(at url: URL) throws -> [String: Any] {
        let parsed = try JSONSerialization.jsonObject(with: Data(contentsOf: url), options: [])
        return try XCTUnwrap(parsed as? [String: Any], url.lastPathComponent)
    }

    /// All `frames/*.bin`, sorted by name.
    static func frameFiles() throws -> [URL] {
        let folder = directory.appendingPathComponent("frames", isDirectory: true)
        let all = try FileManager.default.contentsOfDirectory(at: folder, includingPropertiesForKeys: nil)
        return all.filter { $0.pathExtension == "bin" }.sorted { $0.lastPathComponent < $1.lastPathComponent }
    }

    static func sidecar(of frameFile: URL) throws -> [String: Any] {
        return try jsonObject(at: frameFile.deletingPathExtension().appendingPathExtension("json"))
    }
}

func sha256Hex(_ data: Data) -> String {
    return SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
}

/// A whole number from parsed JSON.
func integer(_ value: Any?) -> Int? {
    return (value as? NSNumber)?.intValue
}

func hex32(_ value: Float) -> String {
    return String(format: "%08x", value.bitPattern)
}

func hex64(_ value: Double) -> String {
    return String(format: "%016llx", value.bitPattern)
}

/// world_from_cam (column-major) of a level camera turned by `yawDegrees`
/// around the world's up axis and placed at (x, y, z).
func levelPose(yawDegrees: Float = 0, x: Float = 0, y: Float = 0, z: Float = 0) -> [Float] {
    let angle = yawDegrees * Float.pi / 180
    let c = cos(angle)
    let s = sin(angle)
    return [c, 0, -s, 0, 0, 1, 0, 0, s, 0, c, 0, x, y, z, 1]
}

/// Little-endian bytes of Float32 values, each row padded to `bytesPerRow`.
func floatRows(_ rows: [[Float]], bytesPerRow: Int) -> Data {
    var out = Data()
    for row in rows {
        var line = Data()
        for value in row {
            let bits = value.bitPattern
            line.append(UInt8(truncatingIfNeeded: bits))
            line.append(UInt8(truncatingIfNeeded: bits >> 8))
            line.append(UInt8(truncatingIfNeeded: bits >> 16))
            line.append(UInt8(truncatingIfNeeded: bits >> 24))
        }
        while line.count < bytesPerRow {
            line.append(0xEE)
        }
        out.append(line)
    }
    return out
}
