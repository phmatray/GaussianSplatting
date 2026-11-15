import Foundation

/// Decides which camera frames are worth sending.
///
/// Fusing modes (room, object, person): a frame is sent when the camera moved
/// 5 cm or turned 5 degrees since the last frame sent, or after 0.5 s; at most
/// 10 per second. Live mode: every frame, at most 15 per second.
///
/// Poses are `world_from_cam` as 16 floats, column-major.
public struct KeyframeSelector {
    /// Shortest time between two frames, seconds.
    public var minInterval: Double
    /// A frame is sent after this long even if the camera did not move.
    /// 0 means every frame the rate allows.
    public var maxInterval: Double
    /// Meters.
    public var minTranslation: Float
    /// Radians.
    public var minRotation: Float

    /// The camera runs at 60 frames per second and its time stamps jitter
    /// by a fraction of a millisecond. Without this slack a 0.1 s interval
    /// would often be missed by a hair and become 0.1167 s.
    public static let timeSlack: Double = 0.004

    private var lastDecisionTime: Double?
    private var lastSentTime: Double?
    private var lastSentPose: [Float]?

    public init(minInterval: Double, maxInterval: Double, minTranslation: Float, minRotation: Float) {
        self.minInterval = minInterval
        self.maxInterval = maxInterval
        self.minTranslation = minTranslation
        self.minRotation = minRotation
    }

    public init(mode: CaptureMode) {
        if mode == .live {
            self.init(minInterval: 1.0 / 15.0, maxInterval: 0.0, minTranslation: 0.0, minRotation: 0.0)
        } else {
            self.init(minInterval: 0.1, maxInterval: 0.5, minTranslation: 0.05, minRotation: 5.0 * Float.pi / 180.0)
        }
    }

    /// Whether the frame with this pose and time should be sent. Follow it
    /// with `markSent` or `markSkipped`.
    public func wants(pose: [Float], time: Double) -> Bool {
        if let last = lastDecisionTime {
            let elapsed = time - last
            // A clock that went backwards (new AR session) starts afresh.
            if elapsed >= 0 && elapsed < minInterval - KeyframeSelector.timeSlack {
                return false
            }
        }
        guard let sentPose = lastSentPose, let sentTime = lastSentTime else {
            return true
        }
        let sinceSent = time - sentTime
        if maxInterval <= 0 || sinceSent < 0 || sinceSent >= maxInterval - KeyframeSelector.timeSlack {
            return true
        }
        if KeyframeSelector.translation(sentPose, pose) >= minTranslation {
            return true
        }
        return KeyframeSelector.rotationAngle(sentPose, pose) >= minRotation
    }

    /// The frame was handed to the encoder.
    public mutating func markSent(pose: [Float], time: Double) {
        lastDecisionTime = time
        lastSentTime = time
        lastSentPose = pose
    }

    /// The frame was wanted but could not be sent (flow control): the next
    /// attempt waits for `minInterval`, the reference pose stays.
    public mutating func markSkipped(time: Double) {
        lastDecisionTime = time
    }

    public mutating func reset() {
        lastDecisionTime = nil
        lastSentTime = nil
        lastSentPose = nil
    }

    /// Distance between the camera positions of two poses, meters.
    public static func translation(_ a: [Float], _ b: [Float]) -> Float {
        guard a.count == 16, b.count == 16 else { return 0 }
        let dx = a[12] - b[12]
        let dy = a[13] - b[13]
        let dz = a[14] - b[14]
        return (dx * dx + dy * dy + dz * dz).squareRoot()
    }

    /// Angle of the rotation between two poses, radians (0...pi).
    public static func rotationAngle(_ a: [Float], _ b: [Float]) -> Float {
        guard a.count == 16, b.count == 16 else { return 0 }
        // trace(Ra^T * Rb) is the sum of the products of matching elements.
        var trace: Float = 0
        for column in 0..<3 {
            for row in 0..<3 {
                trace += a[column * 4 + row] * b[column * 4 + row]
            }
        }
        let cosine = max(-1.0, min(1.0, (trace - 1.0) / 2.0))
        return acos(cosine)
    }
}
