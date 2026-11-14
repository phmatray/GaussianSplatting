import Foundation

/// Conversion of ARKit's planes to the planes of a capture frame.
public enum DepthConversion {
    /// One depth sample: Float32 meters to u16 millimeters.
    ///
    /// `floor(meters * 1000 + 0.5)` in double precision. A value that is not
    /// positive, not finite or beyond 65535 mm gives 0, "no value".
    public static func millimeters(fromMeters meters: Float) -> UInt16 {
        if !(meters > 0) || !meters.isFinite {
            return 0
        }
        let mm = (Double(meters) * 1000.0 + 0.5).rounded(.down)
        if mm > 65535.0 {
            return 0
        }
        return UInt16(mm)
    }

    /// A depth plane (u16 little-endian, rows without padding) from Float32
    /// meters stored in rows of `bytesPerRow` bytes, as in a CVPixelBuffer.
    public static func plane(fromMeters data: Data, width: Int, height: Int, bytesPerRow: Int) -> Data? {
        guard width > 0, height > 0, bytesPerRow >= width * 4,
              data.count >= (height - 1) * bytesPerRow + width * 4 else {
            return nil
        }
        var out = [UInt8](repeating: 0, count: width * height * 2)
        data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            var o = 0
            for row in 0..<height {
                let rowStart = row * bytesPerRow
                for col in 0..<width {
                    let meters = raw.loadUnaligned(fromByteOffset: rowStart + col * 4, as: Float.self)
                    let mm = millimeters(fromMeters: meters)
                    out[o] = UInt8(truncatingIfNeeded: mm)
                    out[o + 1] = UInt8(truncatingIfNeeded: mm >> 8)
                    o += 2
                }
            }
        }
        return Data(out)
    }

    /// Convenience for tests and tools.
    public static func plane(fromMeters values: [Float]) -> Data {
        var out = Data()
        out.reserveCapacity(values.count * 2)
        for value in values {
            out.appendU16LE(millimeters(fromMeters: value))
        }
        return out
    }

    /// An 8-bit plane (confidence, mask) without row padding.
    public static func tightBytes(_ data: Data, width: Int, height: Int, bytesPerRow: Int) -> Data? {
        guard width > 0, height > 0, bytesPerRow >= width,
              data.count >= (height - 1) * bytesPerRow + width else {
            return nil
        }
        let bytes = [UInt8](data)
        var out = [UInt8]()
        out.reserveCapacity(width * height)
        for row in 0..<height {
            let start = row * bytesPerRow
            out.append(contentsOf: bytes[start..<(start + width)])
        }
        return Data(out)
    }

    /// Nearest-neighbour resampling of a tight 8-bit plane (used when the
    /// person mask does not have the size of the depth map).
    public static func resampleNearest(_ plane: Data, width: Int, height: Int, toWidth: Int, toHeight: Int) -> Data? {
        guard width > 0, height > 0, toWidth > 0, toHeight > 0, plane.count == width * height else {
            return nil
        }
        if width == toWidth && height == toHeight {
            return plane
        }
        let bytes = [UInt8](plane)
        var out = [UInt8](repeating: 0, count: toWidth * toHeight)
        for row in 0..<toHeight {
            // Centre of the destination pixel, in source pixels.
            let sourceRow = min(height - 1, (2 * row + 1) * height / (2 * toHeight))
            for col in 0..<toWidth {
                let sourceCol = min(width - 1, (2 * col + 1) * width / (2 * toWidth))
                out[row * toWidth + col] = bytes[sourceRow * width + sourceCol]
            }
        }
        return Data(out)
    }
}
