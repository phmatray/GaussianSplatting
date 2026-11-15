import Foundation
import XCTest
import SplatCaptureKit

final class CaptureFrameTests: XCTestCase {
    private func sampleFrame() -> CaptureFrame {
        var depth = Data()
        for mm in [UInt16(0), 1, 2, 1000, 5000, 65535] {
            depth.append(UInt8(truncatingIfNeeded: mm))
            depth.append(UInt8(truncatingIfNeeded: mm >> 8))
        }
        return CaptureFrame(
            index: 7,
            timestamp: 12.5,
            worldFromCam: (0..<16).map { Float($0) },
            fx: 500, fy: 501, cx: 320, cy: 240,
            tracking: 1,
            orientation: 2,
            colorWidth: 640,
            colorHeight: 480,
            color: Data([0xFF, 0xD8, 0x4A, 0x50, 0x45, 0x47, 0xFF, 0xD9]),
            depthWidth: 3,
            depthHeight: 2,
            depth: depth,
            confidence: Data([0, 1, 2, 2, 1, 0]),
            mask: Data([0, 255, 0, 255, 0, 255]),
            compressed: false
        )
    }

    private func u32(_ bytes: [UInt8], _ offset: Int) -> UInt32 {
        return UInt32(bytes[offset]) | UInt32(bytes[offset + 1]) << 8
            | UInt32(bytes[offset + 2]) << 16 | UInt32(bytes[offset + 3]) << 24
    }

    private func u16(_ bytes: [UInt8], _ offset: Int) -> UInt16 {
        return UInt16(bytes[offset]) | UInt16(bytes[offset + 1]) << 8
    }

    /// Every field at the offset the specification table gives.
    func testHeaderLayout() throws {
        let frame = sampleFrame()
        let bytes = [UInt8](try frame.encoded())
        XCTAssertEqual(bytes.count, 160)
        XCTAssertEqual(Array(bytes[0..<4]), [0x47, 0x53, 0x43, 0x46])       // "GSCF"
        XCTAssertEqual(bytes[4], 1)                                         // version
        XCTAssertEqual(bytes[5], 0b101)                                     // confidence + mask
        XCTAssertEqual(bytes[6], 1)                                         // tracking
        XCTAssertEqual(bytes[7], 2)                                         // orientation
        XCTAssertEqual(u32(bytes, 8), 7)                                    // frame index
        XCTAssertEqual(u32(bytes, 12), 0)                                   // reserved
        let timestampBits = UInt64(u32(bytes, 16)) | UInt64(u32(bytes, 20)) << 32
        XCTAssertEqual(Double(bitPattern: timestampBits), 12.5)             // timestamp
        for i in 0..<16 {
            XCTAssertEqual(Float(bitPattern: u32(bytes, 24 + 4 * i)), Float(i))  // world_from_cam
        }
        XCTAssertEqual(Float(bitPattern: u32(bytes, 88)), 500)              // fx
        XCTAssertEqual(Float(bitPattern: u32(bytes, 92)), 501)              // fy
        XCTAssertEqual(Float(bitPattern: u32(bytes, 96)), 320)              // cx
        XCTAssertEqual(Float(bitPattern: u32(bytes, 100)), 240)             // cy
        XCTAssertEqual(u16(bytes, 104), 640)                                // colour width
        XCTAssertEqual(u16(bytes, 106), 480)                                // colour height
        XCTAssertEqual(u32(bytes, 108), 8)                                  // colour length
        XCTAssertEqual(u16(bytes, 112), 3)                                  // depth width
        XCTAssertEqual(u16(bytes, 114), 2)                                  // depth height
        XCTAssertEqual(u32(bytes, 116), 12)                                 // depth length
        XCTAssertEqual(u32(bytes, 120), 6)                                  // confidence length
        XCTAssertEqual(u32(bytes, 124), 6)                                  // mask length
        XCTAssertEqual(Array(bytes[128..<136]), [0xFF, 0xD8, 0x4A, 0x50, 0x45, 0x47, 0xFF, 0xD9])  // colour
        XCTAssertEqual(Array(bytes[136..<148]), [0, 0, 1, 0, 2, 0, 0xE8, 0x03, 0x88, 0x13, 0xFF, 0xFF])  // depth
        XCTAssertEqual(Array(bytes[148..<154]), [0, 1, 2, 2, 1, 0])         // confidence
        XCTAssertEqual(Array(bytes[154..<160]), [0, 255, 0, 255, 0, 255])   // mask
    }

    func testRoundTripOfEveryFlagCombination() throws {
        for flags in 0..<8 {
            var frame = sampleFrame()
            frame.confidence = flags & 1 != 0 ? Data([2, 2, 1, 1, 0, 0]) : nil
            frame.mask = flags & 4 != 0 ? Data([255, 255, 0, 0, 255, 0]) : nil
            frame.compressed = flags & 2 != 0
            let encoded = try frame.encoded()
            XCTAssertEqual(Int(encoded[encoded.startIndex + 5]), flags)
            XCTAssertEqual(Int(frame.flags), flags)
            let decoded = try CaptureFrame.decode(encoded)
            XCTAssertEqual(decoded, frame, "flags \(flags)")
        }
    }

    func testEncoderRefusesInconsistentFrames() {
        var frame = sampleFrame()
        frame.depth = Data([0, 0])
        XCTAssertThrowsError(try frame.encoded())
        frame = sampleFrame()
        frame.confidence = Data([1])
        XCTAssertThrowsError(try frame.encoded())
        frame = sampleFrame()
        frame.mask = Data(count: 7)
        XCTAssertThrowsError(try frame.encoded())
        frame = sampleFrame()
        frame.tracking = 3
        XCTAssertThrowsError(try frame.encoded())
        frame = sampleFrame()
        frame.orientation = 4
        XCTAssertThrowsError(try frame.encoded())
        frame = sampleFrame()
        frame.worldFromCam = [1, 2, 3]
        XCTAssertThrowsError(try frame.encoded())
        frame = sampleFrame()
        frame.depthWidth = 0
        frame.depth = Data()
        XCTAssertThrowsError(try frame.encoded())
        frame = sampleFrame()
        frame.colorWidth = 70000
        XCTAssertThrowsError(try frame.encoded())
    }

    /// Every file of Fixtures/frames against its sidecar.
    func testFixtureFrames() throws {
        try Fixtures.require()
        let files = try Fixtures.frameFiles()
        XCTAssertGreaterThanOrEqual(files.count, 40)
        var seenFlags = Set<Int>()
        var refused = 0
        for file in files {
            let name = file.lastPathComponent
            let data = try Data(contentsOf: file)
            let expected = try Fixtures.sidecar(of: file)
            XCTAssertEqual(data.count, integer(expected["length"]), name)

            if (expected["valid"] as? Bool) != true {
                XCTAssertThrowsError(try CaptureFrame.decode(data), name) { error in
                    XCTAssertEqual((error as? CaptureFormatError)?.code, expected["error"] as? String, name)
                }
                refused += 1
                continue
            }

            let frame = try CaptureFrame.decode(data)
            XCTAssertEqual(Int(frame.flags), integer(expected["flags"]), name)
            seenFlags.insert(Int(frame.flags))
            XCTAssertEqual(frame.compressed, expected["compressed"] as? Bool, name)
            XCTAssertEqual(Int(frame.tracking), integer(expected["tracking"]), name)
            XCTAssertEqual(Int(frame.orientation), integer(expected["orientation"]), name)
            XCTAssertEqual(UInt64(frame.index), (expected["index"] as? NSNumber)?.uint64Value, name)
            XCTAssertEqual(hex64(frame.timestamp), expected["timestamp_bits"] as? String, name)
            XCTAssertEqual(frame.worldFromCam.map { hex32($0) }, expected["world_from_cam_bits"] as? [String], name)
            XCTAssertEqual(
                [frame.fx, frame.fy, frame.cx, frame.cy].map { hex32($0) },
                expected["intrinsics_bits"] as? [String], name)

            let color = try XCTUnwrap(expected["color"] as? [String: Any], name)
            XCTAssertEqual(frame.colorWidth, integer(color["width"]), name)
            XCTAssertEqual(frame.colorHeight, integer(color["height"]), name)
            XCTAssertEqual(frame.color.count, integer(color["length"]), name)
            XCTAssertEqual(sha256Hex(frame.color), color["sha256"] as? String, name)

            let depth = try XCTUnwrap(expected["depth"] as? [String: Any], name)
            XCTAssertEqual(frame.depthWidth, integer(depth["width"]), name)
            XCTAssertEqual(frame.depthHeight, integer(depth["height"]), name)
            XCTAssertEqual(frame.depth.count, 2 * frame.depthWidth * frame.depthHeight, name)
            XCTAssertEqual(sha256Hex(frame.depth), depth["sha256"] as? String, name)
            if let values = depth["values"] as? [NSNumber] {
                XCTAssertEqual(frame.depthMillimeters.map { Int($0) }, values.map { $0.intValue }, name)
            }

            let confidence = expected["confidence"] as? [String: Any]
            XCTAssertEqual(frame.confidence != nil, confidence != nil, name)
            if let plane = frame.confidence, let confidence = confidence {
                XCTAssertEqual(plane.count, frame.depthWidth * frame.depthHeight, name)
                XCTAssertEqual(sha256Hex(plane), confidence["sha256"] as? String, name)
                if let values = confidence["values"] as? [NSNumber] {
                    XCTAssertEqual([UInt8](plane).map { Int($0) }, values.map { $0.intValue }, name)
                }
            }

            let mask = expected["mask"] as? [String: Any]
            XCTAssertEqual(frame.mask != nil, mask != nil, name)
            if let plane = frame.mask, let mask = mask {
                XCTAssertEqual(plane.count, frame.depthWidth * frame.depthHeight, name)
                XCTAssertEqual(sha256Hex(plane), mask["sha256"] as? String, name)
            }

            // Encoding the decoded fields again.
            let again = try frame.encoded()
            if (expected["reencode_identical"] as? Bool) == true {
                XCTAssertEqual(again, data, name)
            } else {
                XCTAssertEqual(try CaptureFrame.decode(again), frame, name)
            }
        }
        XCTAssertEqual(seenFlags, Set(0..<8))
        XCTAssertGreaterThanOrEqual(refused, 29)
    }

    /// Damaged frames are refused with an error, never with a crash: every
    /// truncation, unknown flag bits, other versions, lengths that overflow.
    func testDamagedFramesAreRefused() throws {
        let good = [UInt8](try sampleFrame().encoded())
        for size in 0..<good.count {
            XCTAssertThrowsError(try CaptureFrame.decode(Data(good[0..<size])), "cut at \(size)") { error in
                let code = (error as? CaptureFormatError)?.code
                XCTAssertEqual(code, size < 128 ? "too_short" : "bad_length", "cut at \(size)")
            }
        }
        XCTAssertThrowsError(try CaptureFrame.decode(Data(good + [0])))

        func refused(_ offset: Int, _ replacement: [UInt8], _ expected: CaptureFormatError, line: UInt = #line) {
            var bytes = good
            for (i, byte) in replacement.enumerated() {
                bytes[offset + i] = byte
            }
            XCTAssertThrowsError(try CaptureFrame.decode(Data(bytes)), line: line) { error in
                XCTAssertEqual(error as? CaptureFormatError, expected, line: line)
            }
        }
        refused(0, [0x47, 0x53, 0x43, 0x47], .badMagic)             // "GSCG"
        refused(4, [0], .badVersion)
        refused(4, [2], .badVersion)
        refused(4, [255], .badVersion)
        for bit in 3..<8 {
            refused(5, [good[5] | UInt8(1 << bit)], .badFlags)
        }
        refused(6, [3], .badTracking)
        refused(7, [4], .badOrientation)
        refused(112, [0, 0], .badDimensions)                        // depth width 0
        refused(112, [0xFF, 0xFF, 0xFF, 0xFF], .badDimensions)      // 65535 x 65535 pixels
        for offset in [108, 116, 120, 124] {
            refused(offset, [0xFF, 0xFF, 0xFF, 0xFF], .badLength)   // 2^32 - 1
            refused(offset, [0x00, 0x00, 0x00, 0x80], .badLength)   // 2^31
        }
        // Largest depth map accepted, and one pixel row more.
        XCTAssertEqual(CaptureFrame.maxDepthPixels, 2048 * 2048)
        var large = sampleFrame()
        large.depthWidth = 2048
        large.depthHeight = 2049
        large.depth = Data(count: 2 * 2048 * 2049)
        large.confidence = nil
        large.mask = nil
        XCTAssertThrowsError(try large.encoded()) { error in
            XCTAssertEqual(error as? CaptureFormatError, .badDimensions)
        }
    }

    func testExtremeFrame() throws {
        try Fixtures.require()
        let data = try Fixtures.data("frames/frame_extreme.bin")
        let frame = try CaptureFrame.decode(data)
        XCTAssertEqual(frame.index, UInt32.max)
        XCTAssertEqual(data.count, 130)
        XCTAssertEqual(frame.depthWidth, 1)
        XCTAssertEqual(frame.depthHeight, 1)
        XCTAssertEqual(frame.depthMillimeters, [65535])
        XCTAssertTrue(frame.color.isEmpty)
        XCTAssertEqual(frame.colorWidth, 65535)
        XCTAssertEqual(frame.timestamp, Double.greatestFiniteMagnitude)
        XCTAssertEqual(frame.worldFromCam[0], Float.greatestFiniteMagnitude)
        XCTAssertEqual(frame.worldFromCam[3], Float.leastNonzeroMagnitude)
        XCTAssertEqual(frame.worldFromCam[4].sign, .minus)  // -0.0 survives
        XCTAssertTrue(frame.worldFromCam.allSatisfy { $0.isFinite })
        XCTAssertEqual(try frame.encoded(), data)
    }

    func testDeflateRoundTripAndLimits() throws {
        var plane = Data()
        for i in 0..<20000 {
            plane.append(UInt8(truncatingIfNeeded: (i / 7) &* 13))
        }
        let packed = try XCTUnwrap(RawDeflate.compress(plane))
        XCTAssertLessThan(packed.count, plane.count)
        XCTAssertEqual(RawDeflate.decompress(packed, expectedSize: plane.count), plane)
        XCTAssertNil(RawDeflate.decompress(packed, expectedSize: plane.count - 1))
        XCTAssertNil(RawDeflate.decompress(packed, expectedSize: plane.count + 1))
        XCTAssertNil(RawDeflate.decompress(Data([0x07, 0xFF, 0xFF, 0xFF]), expectedSize: 24))
        XCTAssertNil(RawDeflate.decompress(Data(), expectedSize: 4))
        XCTAssertNil(RawDeflate.compress(Data()))

        // Data that does not compress still round-trips.
        var noise = Data()
        var state: UInt32 = 12345
        for _ in 0..<5000 {
            state = state &* 1664525 &+ 1013904223
            noise.append(UInt8(truncatingIfNeeded: state >> 24))
        }
        let packedNoise = try XCTUnwrap(RawDeflate.compress(noise))
        XCTAssertEqual(RawDeflate.decompress(packedNoise, expectedSize: noise.count), noise)
    }
}
