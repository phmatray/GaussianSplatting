import Foundation
import XCTest
import SplatCaptureKit

final class DepthConversionTests: XCTestCase {
    func testSingleValues() {
        XCTAssertEqual(DepthConversion.millimeters(fromMeters: 1.0), 1000)
        XCTAssertEqual(DepthConversion.millimeters(fromMeters: 2.5), 2500)
        XCTAssertEqual(DepthConversion.millimeters(fromMeters: 5.0), 5000)
        XCTAssertEqual(DepthConversion.millimeters(fromMeters: 0.0), 0)
        XCTAssertEqual(DepthConversion.millimeters(fromMeters: -1.0), 0)
        XCTAssertEqual(DepthConversion.millimeters(fromMeters: 0.0004), 0)
        XCTAssertEqual(DepthConversion.millimeters(fromMeters: 0.0006), 1)
        XCTAssertEqual(DepthConversion.millimeters(fromMeters: 65.535), 65535)
        XCTAssertEqual(DepthConversion.millimeters(fromMeters: 65.536), 0)
        XCTAssertEqual(DepthConversion.millimeters(fromMeters: 1000), 0)
        XCTAssertEqual(DepthConversion.millimeters(fromMeters: Float.nan), 0)
        XCTAssertEqual(DepthConversion.millimeters(fromMeters: Float.infinity), 0)
        XCTAssertEqual(DepthConversion.millimeters(fromMeters: -Float.infinity), 0)
    }

    func testFixture() throws {
        try Fixtures.require()
        let cases = try XCTUnwrap(Fixtures.json("depth_conversion.json") as? [[String: Any]])
        XCTAssertGreaterThan(cases.count, 30)
        for entry in cases {
            let text = try XCTUnwrap(entry["bits"] as? String)
            let bits = try XCTUnwrap(UInt32(text, radix: 16), text)
            let expected = try XCTUnwrap(integer(entry["mm"]), text)
            XCTAssertEqual(Int(DepthConversion.millimeters(fromMeters: Float(bitPattern: bits))), expected, text)
        }
    }

    func testPlaneWithRowPadding() throws {
        // 3 x 2 pixels in rows of 16 bytes (4 bytes of padding each).
        let data = floatRows([[1.0, 0.5, 0.0], [2.0, Float.nan, 70.0]], bytesPerRow: 16)
        XCTAssertEqual(data.count, 32)
        let plane = try XCTUnwrap(DepthConversion.plane(fromMeters: data, width: 3, height: 2, bytesPerRow: 16))
        // 1000, 500, 0, 2000, 0, 0 as u16 little-endian.
        XCTAssertEqual([UInt8](plane), [0xE8, 0x03, 0xF4, 0x01, 0, 0, 0xD0, 0x07, 0, 0, 0, 0])
        XCTAssertEqual(DepthConversion.plane(fromMeters: [1.0, 0.5, 0.0, 2.0, Float.nan, 70.0]), plane)
        // The last row may lack its padding, but not its pixels.
        XCTAssertNotNil(DepthConversion.plane(fromMeters: data.prefix(28), width: 3, height: 2, bytesPerRow: 16))
        XCTAssertNil(DepthConversion.plane(fromMeters: data.prefix(27), width: 3, height: 2, bytesPerRow: 16))
        XCTAssertNil(DepthConversion.plane(fromMeters: data, width: 5, height: 2, bytesPerRow: 16))
    }

    func testBytePlanes() throws {
        let padded = Data([1, 2, 3, 99, 4, 5, 6, 99])
        XCTAssertEqual(DepthConversion.tightBytes(padded, width: 3, height: 2, bytesPerRow: 4), Data([1, 2, 3, 4, 5, 6]))
        XCTAssertNil(DepthConversion.tightBytes(padded, width: 3, height: 3, bytesPerRow: 4))

        let small = Data([1, 2, 3, 4])
        XCTAssertEqual(
            DepthConversion.resampleNearest(small, width: 2, height: 2, toWidth: 4, toHeight: 4),
            Data([1, 1, 2, 2, 1, 1, 2, 2, 3, 3, 4, 4, 3, 3, 4, 4]))
        let large = Data((0..<16).map { UInt8($0) })
        XCTAssertEqual(
            DepthConversion.resampleNearest(large, width: 4, height: 4, toWidth: 2, toHeight: 2),
            Data([5, 7, 13, 15]))
        XCTAssertEqual(DepthConversion.resampleNearest(small, width: 2, height: 2, toWidth: 2, toHeight: 2), small)
        XCTAssertNil(DepthConversion.resampleNearest(small, width: 3, height: 2, toWidth: 2, toHeight: 2))
    }
}

final class KeyframeSelectorTests: XCTestCase {
    func testGeometry() {
        XCTAssertEqual(KeyframeSelector.translation(levelPose(x: 1, y: 2, z: 3), levelPose(x: 1, y: 2, z: 5)), 2, accuracy: 1e-6)
        XCTAssertEqual(KeyframeSelector.rotationAngle(levelPose(), levelPose()), 0, accuracy: 1e-3)
        XCTAssertEqual(
            KeyframeSelector.rotationAngle(levelPose(yawDegrees: 10), levelPose(yawDegrees: 40)),
            30 * Float.pi / 180, accuracy: 1e-3)
        XCTAssertEqual(
            KeyframeSelector.rotationAngle(levelPose(), levelPose(yawDegrees: 180)), Float.pi, accuracy: 1e-3)
    }

    func testFusingModeThresholds() {
        var selector = KeyframeSelector(mode: .room)
        let start = levelPose()
        XCTAssertTrue(selector.wants(pose: start, time: 10.0))               // the first frame
        selector.markSent(pose: start, time: 10.0)
        XCTAssertFalse(selector.wants(pose: levelPose(x: 1), time: 10.05))   // too soon, whatever the motion
        XCTAssertFalse(selector.wants(pose: start, time: 10.2))              // did not move
        XCTAssertFalse(selector.wants(pose: levelPose(x: 0.04), time: 10.2)) // 4 cm
        XCTAssertTrue(selector.wants(pose: levelPose(x: 0.06), time: 10.2))  // 6 cm
        XCTAssertFalse(selector.wants(pose: levelPose(yawDegrees: 4), time: 10.2))
        XCTAssertTrue(selector.wants(pose: levelPose(yawDegrees: 6), time: 10.2))
        XCTAssertFalse(selector.wants(pose: start, time: 10.4))
        XCTAssertTrue(selector.wants(pose: start, time: 10.5))               // 0.5 s without moving

        // A skipped frame delays the next attempt but keeps the reference pose.
        selector.markSkipped(time: 10.2)
        XCTAssertFalse(selector.wants(pose: levelPose(x: 0.06), time: 10.25))
        XCTAssertTrue(selector.wants(pose: levelPose(x: 0.06), time: 10.31))

        // A clock that jumps back (new session) does not block the selector.
        selector.markSent(pose: start, time: 100.0)
        XCTAssertTrue(selector.wants(pose: start, time: 5.0))

        selector.reset()
        XCTAssertTrue(selector.wants(pose: start, time: 5.01))
    }

    /// One second of a 60 Hz camera: at most 10 frames (fusing), 15 (live).
    func testRateLimits() {
        var fusing = KeyframeSelector(mode: .person)
        var live = KeyframeSelector(mode: .live)
        var fusingCount = 0
        var liveCount = 0
        for i in 0..<60 {
            let time = 200.0 + Double(i) / 60.0
            let moving = levelPose(x: Float(i) * 0.02)  // 1.2 m/s
            if fusing.wants(pose: moving, time: time) {
                fusing.markSent(pose: moving, time: time)
                fusingCount += 1
            }
            let still = levelPose()
            if live.wants(pose: still, time: time) {
                live.markSent(pose: still, time: time)
                liveCount += 1
            }
        }
        XCTAssertEqual(fusingCount, 10)
        XCTAssertEqual(liveCount, 15)
    }

    func testStillCameraSendsTwicePerSecond() {
        var selector = KeyframeSelector(mode: .object)
        var count = 0
        for i in 0..<120 {
            let time = Double(i) / 60.0
            if selector.wants(pose: levelPose(), time: time) {
                selector.markSent(pose: levelPose(), time: time)
                count += 1
            }
        }
        XCTAssertEqual(count, 4)  // at 0, 0.5, 1.0 and 1.5 s
    }
}

final class FlowControlTests: XCTestCase {
    func testWindow() {
        var flow = FlowControl()
        XCTAssertFalse(flow.isOpen)
        XCTAssertFalse(flow.canSend)                 // nothing before the welcome
        XCTAssertFalse(flow.reserve(0))
        flow.welcome(maxInflight: 2)
        XCTAssertTrue(flow.isOpen)
        XCTAssertTrue(flow.reserve(0))
        XCTAssertTrue(flow.reserve(1))
        XCTAssertFalse(flow.canSend)
        XCTAssertFalse(flow.reserve(2))              // skipped, not queued
        XCTAssertTrue(flow.acknowledge(0))
        XCTAssertFalse(flow.acknowledge(0))          // only once
        XCTAssertFalse(flow.acknowledge(7))          // never sent
        XCTAssertTrue(flow.reserve(2))
        XCTAssertEqual(flow.inflight, [1, 2])
        flow.cancel(2)
        XCTAssertTrue(flow.holds(1))
        XCTAssertFalse(flow.holds(2))
        XCTAssertTrue(flow.canSend)
    }

    func testReconnection() {
        var flow = FlowControl()
        flow.welcome(maxInflight: 0)                 // clamped to 1
        XCTAssertEqual(flow.maxInflight, 1)
        XCTAssertTrue(flow.reserve(5))
        flow.disconnect()
        XCTAssertFalse(flow.canSend)
        XCTAssertFalse(flow.holds(5))
        flow.welcome(maxInflight: 4)
        XCTAssertTrue(flow.inflight.isEmpty)         // old frames are forgotten
        XCTAssertFalse(flow.acknowledge(5))
        XCTAssertTrue(flow.canSend)
    }

    /// A server that offers a huge window does not make the phone queue
    /// that many frames in memory.
    func testWindowIsBounded() {
        var flow = FlowControl()
        flow.welcome(maxInflight: 1_000_000)
        XCTAssertEqual(flow.maxInflight, FlowControl.largestWindow)
        var accepted = 0
        for index in 0..<100 {
            if flow.reserve(UInt32(index)) {
                accepted += 1
            }
        }
        XCTAssertEqual(accepted, FlowControl.largestWindow)
        flow.welcome(maxInflight: 4)
        XCTAssertEqual(flow.maxInflight, 4)
    }
}

final class GeometryTests: XCTestCase {
    func testDisplayOrientation() {
        // Sensor orientation: the image's up is the world's up.
        XCTAssertEqual(DisplayOrientation.code(worldFromCam: levelPose(yawDegrees: 33), previous: 9), 0)
        // Portrait: the image's +X axis points to the ground, its +Y to the side.
        let portrait: [Float] = [0, -1, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1]
        XCTAssertEqual(DisplayOrientation.code(worldFromCam: portrait, previous: 0), 1)
        let flipped: [Float] = [-1, 0, 0, 0, 0, -1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1]
        XCTAssertEqual(DisplayOrientation.code(worldFromCam: flipped, previous: 0), 2)
        let upsideDown: [Float] = [0, 1, 0, 0, -1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1]
        XCTAssertEqual(DisplayOrientation.code(worldFromCam: upsideDown, previous: 0), 3)
        // Looking straight down: undecidable, the previous answer is kept.
        let down: [Float] = [1, 0, 0, 0, 0, 0, -1, 0, 0, 1, 0, 0, 0, 0, 0, 1]
        XCTAssertEqual(DisplayOrientation.code(worldFromCam: down, previous: 1), 1)
        XCTAssertEqual(DisplayOrientation.code(worldFromCam: down, previous: 2), 2)
        XCTAssertEqual(DisplayOrientation.code(worldFromCam: [1, 2, 3], previous: 3), 3)
    }

    func testColorGeometry() {
        let sensor = ColorGeometry(width: 1920, height: 1440, fx: 1450, fy: 1460, cx: 960, cy: 720)
        XCTAssertEqual(sensor.scaled(toWidth: 960), ColorGeometry(width: 960, height: 720, fx: 725, fy: 730, cx: 480, cy: 360))
        let small = sensor.scaled(toWidth: 640)
        XCTAssertEqual(small.width, 640)
        XCTAssertEqual(small.height, 480)
        XCTAssertEqual(small.fx, 1450.0 / 3.0, accuracy: 1e-3)
        XCTAssertEqual(small.fy, 1460.0 / 3.0, accuracy: 1e-3)
        XCTAssertEqual(small.cx, 320, accuracy: 1e-3)
        XCTAssertEqual(small.cy, 240, accuracy: 1e-3)
        XCTAssertEqual(sensor.scaled(toWidth: 1440).height, 1080)
        XCTAssertEqual(sensor.scaled(toWidth: 0), sensor)       // full size
        XCTAssertEqual(sensor.scaled(toWidth: 1920), sensor)
        XCTAssertEqual(sensor.scaled(toWidth: 4000), sensor)    // never enlarged
        let wide = ColorGeometry(width: 1920, height: 1080, fx: 1400, fy: 1400, cx: 960, cy: 540)
        XCTAssertEqual(wide.scaled(toWidth: 960).height, 540)
    }

    func testModes() {
        XCTAssertEqual(CaptureMode.allCases.map { $0.rawValue }, ["room", "object", "person", "live"])
        XCTAssertTrue(CaptureMode.object.usesRegion)
        XCTAssertTrue(CaptureMode.person.usesRegion)
        XCTAssertFalse(CaptureMode.room.usesRegion)
        XCTAssertFalse(CaptureMode.live.usesRegion)
    }
}
