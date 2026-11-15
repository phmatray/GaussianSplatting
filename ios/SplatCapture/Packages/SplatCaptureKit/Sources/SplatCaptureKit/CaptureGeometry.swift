import Foundation

/// Byte 7 of the frame header: how the device is held, as a hint for
/// previews. Derived from gravity in the camera pose, so it does not depend
/// on the interface orientation lock.
public enum DisplayOrientation {
    public static let sensorNative: UInt8 = 0        // landscape, as the sensor delivers the image
    public static let portrait: UInt8 = 1
    public static let landscapeFlipped: UInt8 = 2
    public static let portraitUpsideDown: UInt8 = 3

    /// `worldFromCam`: 16 floats, column-major. The world is +Y up. Camera
    /// space is +X right and +Y up of the image in sensor orientation.
    ///
    /// The world's up axis seen from the camera is (m[1], m[5], m[9]). In
    /// the sensor's own orientation it is the image's up (+Y). Held in
    /// portrait, the image's +X axis points to the ground, so up is -X.
    /// When the camera looks almost straight up or down the answer is
    /// arbitrary and `previous` is kept.
    public static func code(worldFromCam m: [Float], previous: UInt8) -> UInt8 {
        guard m.count == 16 else { return previous }
        let upX = m[1]
        let upY = m[5]
        guard upX.isFinite, upY.isFinite, max(abs(upX), abs(upY)) >= 0.35 else {
            return previous
        }
        if abs(upY) >= abs(upX) {
            return upY > 0 ? sensorNative : landscapeFlipped
        }
        return upX < 0 ? portrait : portraitUpsideDown
    }
}

/// Size of the colour image as sent, and the intrinsics that go with it.
public struct ColorGeometry: Equatable {
    public var width: Int
    public var height: Int
    public var fx: Float
    public var fy: Float
    public var cx: Float
    public var cy: Float

    public init(width: Int, height: Int, fx: Float, fy: Float, cx: Float, cy: Float) {
        self.width = width
        self.height = height
        self.fx = fx
        self.fy = fy
        self.cx = cx
        self.cy = cy
    }

    /// Widths offered by the app; 0 means the sensor's full size.
    public static let widthChoices = [640, 960, 1440, 0]

    /// The geometry after scaling the sensor image to `targetWidth` pixels
    /// (0, or a width not smaller than the sensor's: unchanged). The aspect
    /// ratio is kept; intrinsics are in pixels with the origin at the top
    /// left corner of the image, so they scale with the image.
    public func scaled(toWidth targetWidth: Int) -> ColorGeometry {
        guard targetWidth > 0, targetWidth < width, width > 0, height > 0 else {
            return self
        }
        let newHeight = max(1, Int((Double(height) * Double(targetWidth) / Double(width)).rounded()))
        let sx = Float(Double(targetWidth) / Double(width))
        let sy = Float(Double(newHeight) / Double(height))
        return ColorGeometry(width: targetWidth, height: newHeight, fx: fx * sx, fy: fy * sy, cx: cx * sx, cy: cy * sy)
    }
}
