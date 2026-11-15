import CoreImage
import CoreVideo
import Foundation
import SplatCaptureKit
import UIKit

/// A plane copied out of a CVPixelBuffer, rows still padded to `bytesPerRow`.
struct PlaneCopy {
    var data: Data
    var width: Int
    var height: Int
    var bytesPerRow: Int
}

/// Everything one capture frame needs, copied out of the ARFrame so that the
/// ARFrame itself is never kept (ARKit stops delivering frames when its
/// buffers are held).
struct RawCapture {
    var index: UInt32
    var timestamp: Double
    /// world_from_cam, 16 floats, column-major: `frame.camera.transform` as is.
    var pose: [Float]
    /// Intrinsics and size of the sensor image.
    var geometry: ColorGeometry
    var tracking: UInt8
    var orientation: UInt8
    /// A private copy of the camera image (YCbCr).
    var image: CVPixelBuffer
    /// Float32 meters.
    var depth: PlaneCopy
    /// ARConfidenceLevel: 0 low, 1 medium, 2 high.
    var confidence: PlaneCopy?
    /// ARKit's person segmentation: 0 or 255.
    var mask: PlaneCopy?
}

/// Turns a `RawCapture` into the bytes of a capture frame. `encode` is
/// called on the capture queue only; the copy helpers are called on the main
/// thread from the ARSession delegate.
final class FrameEncoder {
    private let context = CIContext()
    private let jpegQuality: CGFloat = 0.8

    /// JPEG at the wanted width with intrinsics scaled to it, depth to
    /// millimeters, planes deflated, 128-byte header. nil if anything fails.
    func encode(_ raw: RawCapture, targetWidth: Int) -> Data? {
        let wanted = raw.geometry.scaled(toWidth: targetWidth)
        guard let jpeg = jpegData(from: raw.image, width: wanted.width, height: wanted.height) else {
            return nil
        }
        // The header describes the image actually produced.
        var geometry = wanted
        if jpeg.width != wanted.width || jpeg.height != wanted.height {
            let sx = Float(jpeg.width) / Float(max(1, raw.geometry.width))
            let sy = Float(jpeg.height) / Float(max(1, raw.geometry.height))
            geometry = ColorGeometry(
                width: jpeg.width, height: jpeg.height,
                fx: raw.geometry.fx * sx, fy: raw.geometry.fy * sy,
                cx: raw.geometry.cx * sx, cy: raw.geometry.cy * sy)
        }

        let depthWidth = raw.depth.width
        let depthHeight = raw.depth.height
        guard let depth = DepthConversion.plane(
            fromMeters: raw.depth.data, width: depthWidth, height: depthHeight, bytesPerRow: raw.depth.bytesPerRow)
        else {
            return nil
        }

        var confidence: Data?
        if let plane = raw.confidence, plane.width == depthWidth, plane.height == depthHeight {
            confidence = DepthConversion.tightBytes(
                plane.data, width: plane.width, height: plane.height, bytesPerRow: plane.bytesPerRow)
        }

        var mask: Data?
        if let plane = raw.mask,
           let tight = DepthConversion.tightBytes(
               plane.data, width: plane.width, height: plane.height, bytesPerRow: plane.bytesPerRow) {
            // The segmentation buffer normally has the size of the depth map.
            mask = DepthConversion.resampleNearest(
                tight, width: plane.width, height: plane.height, toWidth: depthWidth, toHeight: depthHeight)
        }

        let frame = CaptureFrame(
            index: raw.index,
            timestamp: raw.timestamp,
            worldFromCam: raw.pose,
            fx: geometry.fx,
            fy: geometry.fy,
            cx: geometry.cx,
            cy: geometry.cy,
            tracking: raw.tracking,
            orientation: raw.orientation,
            colorWidth: geometry.width,
            colorHeight: geometry.height,
            color: jpeg.data,
            depthWidth: depthWidth,
            depthHeight: depthHeight,
            depth: depth,
            confidence: confidence,
            mask: mask,
            compressed: true
        )
        return try? frame.encoded()
    }

    /// Scales the camera image and compresses it. Rendering into an explicit
    /// rectangle guarantees the pixel size, whatever the rounding of the scale.
    private func jpegData(from buffer: CVPixelBuffer, width: Int, height: Int) -> (data: Data, width: Int, height: Int)? {
        let sourceWidth = CVPixelBufferGetWidth(buffer)
        let sourceHeight = CVPixelBufferGetHeight(buffer)
        guard sourceWidth > 0, sourceHeight > 0, width > 0, height > 0 else {
            return nil
        }
        var image = CIImage(cvPixelBuffer: buffer)
        if width != sourceWidth || height != sourceHeight {
            let sx = CGFloat(width) / CGFloat(sourceWidth)
            let sy = CGFloat(height) / CGFloat(sourceHeight)
            // Clamping first keeps the last row and column from blending with nothing.
            image = image.clampedToExtent().transformed(by: CGAffineTransform(scaleX: sx, y: sy))
        }
        let rect = CGRect(x: 0, y: 0, width: width, height: height)
        guard let rendered = context.createCGImage(image, from: rect),
              let data = UIImage(cgImage: rendered).jpegData(compressionQuality: jpegQuality) else {
            return nil
        }
        return (data, rendered.width, rendered.height)
    }

    // MARK: Copies (main thread, inside the ARSession delegate call)

    /// A deep copy of a pixel buffer, planar or not.
    static func copy(_ source: CVPixelBuffer) -> CVPixelBuffer? {
        let attributes: [String: Any] = [kCVPixelBufferIOSurfacePropertiesKey as String: [String: Any]()]
        var created: CVPixelBuffer?
        let status = CVPixelBufferCreate(
            kCFAllocatorDefault,
            CVPixelBufferGetWidth(source),
            CVPixelBufferGetHeight(source),
            CVPixelBufferGetPixelFormatType(source),
            attributes as CFDictionary,
            &created)
        guard status == kCVReturnSuccess, let copy = created else {
            return nil
        }
        // Colour space and YCbCr matrix travel as attachments.
        CVBufferPropagateAttachments(source, copy)

        CVPixelBufferLockBaseAddress(source, .readOnly)
        CVPixelBufferLockBaseAddress(copy, [])
        defer {
            CVPixelBufferUnlockBaseAddress(copy, [])
            CVPixelBufferUnlockBaseAddress(source, .readOnly)
        }
        if CVPixelBufferIsPlanar(source) {
            for plane in 0..<CVPixelBufferGetPlaneCount(source) {
                guard let from = CVPixelBufferGetBaseAddressOfPlane(source, plane),
                      let to = CVPixelBufferGetBaseAddressOfPlane(copy, plane) else {
                    return nil
                }
                copyRows(
                    from: from, fromBytesPerRow: CVPixelBufferGetBytesPerRowOfPlane(source, plane),
                    to: to, toBytesPerRow: CVPixelBufferGetBytesPerRowOfPlane(copy, plane),
                    rows: min(CVPixelBufferGetHeightOfPlane(source, plane), CVPixelBufferGetHeightOfPlane(copy, plane)))
            }
        } else {
            guard let from = CVPixelBufferGetBaseAddress(source), let to = CVPixelBufferGetBaseAddress(copy) else {
                return nil
            }
            copyRows(
                from: from, fromBytesPerRow: CVPixelBufferGetBytesPerRow(source),
                to: to, toBytesPerRow: CVPixelBufferGetBytesPerRow(copy),
                rows: min(CVPixelBufferGetHeight(source), CVPixelBufferGetHeight(copy)))
        }
        return copy
    }

    private static func copyRows(
        from: UnsafeMutableRawPointer, fromBytesPerRow: Int,
        to: UnsafeMutableRawPointer, toBytesPerRow: Int,
        rows: Int
    ) {
        if fromBytesPerRow == toBytesPerRow {
            memcpy(to, from, rows * fromBytesPerRow)
            return
        }
        let count = min(fromBytesPerRow, toBytesPerRow)
        for row in 0..<rows {
            memcpy(to + row * toBytesPerRow, from + row * fromBytesPerRow, count)
        }
    }

    /// The bytes of a single-plane buffer (depth, confidence, mask) if it
    /// has the expected pixel format.
    static func copyPlane(_ buffer: CVPixelBuffer, format: OSType) -> PlaneCopy? {
        guard CVPixelBufferGetPixelFormatType(buffer) == format, !CVPixelBufferIsPlanar(buffer) else {
            return nil
        }
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(buffer) else {
            return nil
        }
        let height = CVPixelBufferGetHeight(buffer)
        let bytesPerRow = CVPixelBufferGetBytesPerRow(buffer)
        return PlaneCopy(
            data: Data(bytes: base, count: height * bytesPerRow),
            width: CVPixelBufferGetWidth(buffer),
            height: height,
            bytesPerRow: bytesPerRow)
    }
}
