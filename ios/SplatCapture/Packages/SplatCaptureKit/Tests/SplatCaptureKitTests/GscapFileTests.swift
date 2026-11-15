import Foundation
import XCTest
import SplatCaptureKit

final class GscapFileTests: XCTestCase {
    private func temporaryFile(_ name: String) -> URL {
        return FileManager.default.temporaryDirectory.appendingPathComponent(name)
    }

    /// Reads a fixture file and compares it with its JSON description.
    private func check(_ stem: String) throws -> [Data] {
        let expected = try Fixtures.jsonObject(at: Fixtures.url(stem + ".json"))
        let reader = try GscapReader(url: Fixtures.url(stem + ".gscap"))
        defer { reader.close() }
        XCTAssertEqual(Int(reader.fileSize), integer(expected["file_length"]), stem)
        XCTAssertEqual(reader.headerObject as NSDictionary, expected["header"] as? NSDictionary, stem)
        XCTAssertEqual(Int(reader.framesOffset), 12 + (integer(expected["header_json_length"]) ?? -1), stem)

        var frames = [Data]()
        while let frame = try reader.nextFrame() {
            frames.append(frame)
        }
        XCTAssertEqual(reader.truncated, expected["truncated"] as? Bool, stem)
        let wanted = try XCTUnwrap(expected["frames"] as? [[String: Any]], stem)
        XCTAssertEqual(frames.count, wanted.count, stem)
        for (data, want) in zip(frames, wanted) {
            XCTAssertEqual(data.count, integer(want["length"]), stem)
            XCTAssertEqual(sha256Hex(data), want["sha256"] as? String, stem)
            XCTAssertEqual(Int(try CaptureFrame.decode(data).index), integer(want["index"]), stem)
        }
        // Reading again from the start gives the same frames.
        try reader.rewind()
        var count = 0
        while try reader.nextFrame() != nil {
            count += 1
        }
        XCTAssertEqual(count, frames.count, stem)
        return frames
    }

    func testThreeFrames() throws {
        try Fixtures.require()
        let frames = try check("three_frames")
        XCTAssertEqual(frames.count, 3)
        let reader = try GscapReader(url: Fixtures.url("three_frames.gscap"))
        defer { reader.close() }
        let header = reader.header
        XCTAssertEqual(header.protocolVersion, 1)
        XCTAssertEqual(header.device, "iPhone17,2")
        XCTAssertEqual(header.app, "0.1.0")
        XCTAssertEqual(header.mode, "object")
        XCTAssertEqual(header.name, "Th\u{E9}i\u{E8}re")
        XCTAssertEqual(header.created, "2026-11-14T09:30:00Z")
        XCTAssertEqual(header.regionCenter, [0.5, -0.25, -1.5])
        XCTAssertEqual(header.regionRadius, 0.75)
        XCTAssertEqual(header.floorY, -1.25)
        XCTAssertNil(reader.headerObject["token"])
    }

    func testPaddedTruncatedAndEmpty() throws {
        try Fixtures.require()
        XCTAssertEqual(try check("padded_header").count, 2)
        XCTAssertEqual(try check("truncated").count, 2)           // cut inside the third frame
        XCTAssertEqual(try check("truncated_length").count, 2)    // cut inside its u32 length
        XCTAssertEqual(try check("empty").count, 0)
    }

    func testSummaries() throws {
        try Fixtures.require()
        let whole = try GscapSummary.read(url: Fixtures.url("three_frames.gscap"))
        XCTAssertEqual(whole.frameCount, 3)
        XCTAssertFalse(whole.truncated)
        XCTAssertEqual(whole.duration, 0.875, accuracy: 1e-9)  // 1000.5 ... 1001.375
        XCTAssertEqual(whole.header.mode, "object")
        let cut = try GscapSummary.read(url: Fixtures.url("truncated.gscap"))
        XCTAssertEqual(cut.frameCount, 2)
        XCTAssertTrue(cut.truncated)
        XCTAssertEqual(cut.duration, 0.125, accuracy: 1e-9)
        let cutInLength = try GscapSummary.read(url: Fixtures.url("truncated_length.gscap"))
        XCTAssertEqual(cutInLength.frameCount, 2)
        XCTAssertTrue(cutInLength.truncated)
        let empty = try GscapSummary.read(url: Fixtures.url("empty.gscap"))
        XCTAssertEqual(empty.frameCount, 0)
        XCTAssertEqual(empty.duration, 0)
    }

    func testInvalidFiles() throws {
        try Fixtures.require()
        let cases = try XCTUnwrap(Fixtures.json("invalid_files.json") as? [[String: Any]])
        XCTAssertGreaterThanOrEqual(cases.count, 9)
        for entry in cases {
            let name = try XCTUnwrap(entry["file"] as? String)
            do {
                let reader = try GscapReader(url: Fixtures.url(name))
                defer { reader.close() }
                while try reader.nextFrame() != nil {
                }
                XCTFail("accepted \(name)")
            } catch {
                XCTAssertEqual((error as? GscapError)?.code, entry["error"] as? String, name)
            }
        }
    }

    func testWriterAndHeaderRewrite() throws {
        try Fixtures.require()
        let frames = [
            try Fixtures.data("frames/frame_plain.bin"),
            try Fixtures.data("frames/frame_conf.bin"),
            try Fixtures.data("frames/frame_deflate_conf_mask.bin"),
        ]
        let url = temporaryFile("splatcapture-test-\(UUID().uuidString).gscap")
        defer { try? FileManager.default.removeItem(at: url) }

        var header = GscapHeader(
            device: "iPhone17,2", app: "0.1.0", mode: "person", name: "Zo\u{E9}", created: "2026-11-14T10:00:00Z")
        let writer = try GscapWriter(url: url, header: header)
        for frame in frames {
            try writer.append(frame)
        }
        XCTAssertThrowsError(try writer.append(Data(count: 100)))  // shorter than a frame header
        header.regionCenter = [1, 2, 3]
        header.regionRadius = 0.5
        header.floorY = -1
        try writer.rewriteHeader(header)
        try writer.append(frames[0])
        XCTAssertEqual(writer.frameCount, 4)
        try writer.close()

        let expectedSize = 12 + GscapWriter.headerPadding + frames.reduce(0) { $0 + 4 + $1.count } + 4 + frames[0].count
        XCTAssertEqual(writer.byteCount, expectedSize)
        let raw = [UInt8](try Data(contentsOf: url))
        XCTAssertEqual(raw.count, expectedSize)
        XCTAssertEqual(Array(raw[0..<8]), [0x47, 0x53, 0x43, 0x41, 0x50, 0x31, 0, 0])   // "GSCAP1\0\0"
        XCTAssertEqual(Array(raw[8..<12]), [0x00, 0x04, 0x00, 0x00])                    // 1024, little-endian
        XCTAssertEqual(raw[12], 0x7B)                                                   // "{"
        XCTAssertEqual(raw[12 + 1023], 0x20)                                            // padded with spaces
        // First record: u32 length, then the frame as on the wire.
        let first = 12 + 1024
        XCTAssertEqual(Int(raw[first]) | Int(raw[first + 1]) << 8, frames[0].count)
        XCTAssertEqual(Data(raw[(first + 4)..<(first + 4 + frames[0].count)]), frames[0])

        let reader = try GscapReader(url: url)
        defer { reader.close() }
        XCTAssertEqual(reader.header, header)
        XCTAssertEqual(Int(reader.framesOffset), 12 + GscapWriter.headerPadding)
        var back = [Data]()
        while let frame = try reader.nextFrame() {
            back.append(frame)
        }
        XCTAssertEqual(back, frames + [frames[0]])
        XCTAssertFalse(reader.truncated)

        let summary = try GscapSummary.read(url: url)
        XCTAssertEqual(summary.frameCount, 4)
        XCTAssertEqual(summary.header.name, "Zo\u{E9}")
        XCTAssertEqual(summary.header.regionCenter, [1, 2, 3])
    }

    func testHeaderWithoutRegionOrFloor() throws {
        let header = GscapHeader(device: "d", app: "a", mode: "room", name: "n", created: "2026-01-01T00:00:00Z")
        let encoded = [UInt8](try GscapFile.encodeHeader(header))
        let length = Int(encoded[8]) | Int(encoded[9]) << 8
        XCTAssertEqual(encoded.count, 12 + length)
        let object = try JSONSerialization.jsonObject(with: Data(encoded[12...]), options: []) as? NSDictionary
        let expected: NSDictionary = [
            "protocol": 1, "device": "d", "app": "a", "mode": "room", "name": "n", "created": "2026-01-01T00:00:00Z",
        ]
        XCTAssertEqual(object, expected)
        // `floor` as a bare number is understood too.
        XCTAssertEqual(GscapHeader(jsonObject: ["floor": -0.5]).floorY, -0.5)
        XCTAssertEqual(GscapHeader(jsonObject: ["floor": ["y": 2.0]]).floorY, 2.0)
        XCTAssertNil(GscapHeader(jsonObject: ["floor": "low"]).floorY)
        XCTAssertFalse(GscapHeader.timestampNow().isEmpty)
    }

    /// Writes frames compressed by Apple's Compression framework to a file
    /// that the Python reference can check:
    ///
    ///     python3 tools/gscap.py verify "$TMPDIR/splatcapture-swift-sample.gscap"
    func testWritesSampleForThePythonReference() throws {
        try Fixtures.require()
        let url = temporaryFile("splatcapture-swift-sample.gscap")
        try? FileManager.default.removeItem(at: url)
        let header = GscapHeader(
            device: "swift-test", app: "0.1.0", mode: "room", name: "Swift sample", created: GscapHeader.timestampNow(),
            floorY: -1.25)
        let writer = try GscapWriter(url: url, header: header)
        var index: UInt32 = 0
        for name in ["frame_lidar", "frame_deflate_conf_mask", "frame_plain", "frame_deflate"] {
            var frame = try CaptureFrame.decode(try Fixtures.data("frames/\(name).bin"))
            frame.index = index
            frame.timestamp = 50.0 + Double(index) * 0.1
            index += 1
            let encoded = try frame.encoded()  // deflated here, not by zlib
            XCTAssertEqual(try CaptureFrame.decode(encoded), frame, name)
            try writer.append(encoded)
        }
        try writer.close()
        XCTAssertEqual(try GscapSummary.read(url: url).frameCount, 4)
        print("Sample for the Python reference: python3 tools/gscap.py verify \"\(url.path)\"")
    }
}
