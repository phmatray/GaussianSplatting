import Foundation

// The capture frame of docs/ARCHITECTURE.md ("Real capture: the phone
// companion"): what travels in one WebSocket binary message and in one
// record of a .gscap file. Little-endian, 128-byte header:
//
//   offset size field
//        0    4 magic "GSCF"
//        4    1 version, 1
//        5    1 flags: bit 0 confidence present, bit 1 depth, confidence and
//               mask are raw-deflate compressed, bit 2 mask present
//        6    1 tracking: 0 not available, 1 limited, 2 normal
//        7    1 display orientation: 0 sensor native (landscape), 1 portrait,
//               2 landscape flipped, 3 portrait upside down
//        8    4 frame index u32
//       12    4 reserved, 0
//       16    8 timestamp f64, seconds
//       24   64 world_from_cam, 16 x f32, column-major
//       88   16 fx, fy, cx, cy as f32, in pixels of the colour image as sent
//      104    4 colour width, height as u16
//      108    4 colour byte length u32 (a JPEG)
//      112    4 depth width, height as u16
//      116    4 depth byte length u32
//      120    4 confidence byte length u32 (0 if absent)
//      124    4 mask byte length u32 (0 if absent)
//      128    . colour, then depth, then confidence, then mask
//
// The reference implementation is tools/gscap.py; both are tested against
// the files in ios/SplatCapture/Fixtures.

/// Why a frame was refused. `code` is the identifier used by the fixtures
/// and by the Python reference.
public enum CaptureFormatError: Error, Equatable {
    case tooShort
    case badMagic
    case badVersion
    case badFlags
    case badTracking
    case badOrientation
    case badDimensions
    case badLength
    case badDeflate
    case badValue

    public var code: String {
        switch self {
        case .tooShort: return "too_short"
        case .badMagic: return "bad_magic"
        case .badVersion: return "bad_version"
        case .badFlags: return "bad_flags"
        case .badTracking: return "bad_tracking"
        case .badOrientation: return "bad_orientation"
        case .badDimensions: return "bad_dimensions"
        case .badLength: return "bad_length"
        case .badDeflate: return "bad_deflate"
        case .badValue: return "bad_value"
        }
    }
}

public struct CaptureFrame: Equatable {
    public static let headerSize = 128
    public static let magic: [UInt8] = [0x47, 0x53, 0x43, 0x46]  // "GSCF"
    public static let version: UInt8 = 1

    public static let flagConfidence: UInt8 = 1
    public static let flagDeflate: UInt8 = 2
    public static let flagMask: UInt8 = 4

    /// Implementation limit shared with tools/gscap.py: a depth map has at
    /// most 2^22 pixels (ARKit delivers 256 x 192). Without it a small
    /// deflated frame could announce 65535 x 65535 pixels and make a reader
    /// allocate 8.6 GB.
    public static let maxDepthPixels = 1 << 22

    public static let trackingNotAvailable: UInt8 = 0
    public static let trackingLimited: UInt8 = 1
    public static let trackingNormal: UInt8 = 2

    public var index: UInt32
    /// Seconds (ARKit frame time).
    public var timestamp: Double
    /// 16 values, column-major: camera space to world.
    public var worldFromCam: [Float]
    public var fx: Float
    public var fy: Float
    public var cx: Float
    public var cy: Float
    public var tracking: UInt8
    public var orientation: UInt8
    public var colorWidth: Int
    public var colorHeight: Int
    /// A JPEG; may be empty (no colour image).
    public var color: Data
    public var depthWidth: Int
    public var depthHeight: Int
    /// u16 little-endian millimeters, row-major from the top left, 0 = no
    /// value. Always held uncompressed: 2 * width * height bytes.
    public var depth: Data
    /// One byte per depth pixel (0 low, 1 medium, 2 high), or nil.
    public var confidence: Data?
    /// One byte per depth pixel (0 background, 255 person), or nil.
    public var mask: Data?
    /// Whether the planes are (to be) deflated on the wire.
    public var compressed: Bool

    public init(
        index: UInt32,
        timestamp: Double,
        worldFromCam: [Float],
        fx: Float,
        fy: Float,
        cx: Float,
        cy: Float,
        tracking: UInt8,
        orientation: UInt8,
        colorWidth: Int,
        colorHeight: Int,
        color: Data,
        depthWidth: Int,
        depthHeight: Int,
        depth: Data,
        confidence: Data? = nil,
        mask: Data? = nil,
        compressed: Bool = false
    ) {
        self.index = index
        self.timestamp = timestamp
        self.worldFromCam = worldFromCam
        self.fx = fx
        self.fy = fy
        self.cx = cx
        self.cy = cy
        self.tracking = tracking
        self.orientation = orientation
        self.colorWidth = colorWidth
        self.colorHeight = colorHeight
        self.color = color
        self.depthWidth = depthWidth
        self.depthHeight = depthHeight
        self.depth = depth
        self.confidence = confidence
        self.mask = mask
        self.compressed = compressed
    }

    public var flags: UInt8 {
        var value: UInt8 = 0
        if confidence != nil { value |= CaptureFrame.flagConfidence }
        if compressed { value |= CaptureFrame.flagDeflate }
        if mask != nil { value |= CaptureFrame.flagMask }
        return value
    }

    // MARK: Encoding

    /// The frame as it travels on the wire.
    ///
    /// If `compressed` is set but the compressor fails on any plane, the
    /// frame is written uncompressed (and says so in its flags) instead of
    /// being lost.
    public func encoded() throws -> Data {
        guard worldFromCam.count == 16 else { throw CaptureFormatError.badValue }
        guard (0...65535).contains(colorWidth), (0...65535).contains(colorHeight) else {
            throw CaptureFormatError.badValue
        }
        guard (1...65535).contains(depthWidth), (1...65535).contains(depthHeight) else {
            throw CaptureFormatError.badDimensions
        }
        let pixels = depthWidth * depthHeight
        guard pixels <= CaptureFrame.maxDepthPixels else { throw CaptureFormatError.badDimensions }
        guard depth.count == 2 * pixels else { throw CaptureFormatError.badLength }
        if let confidence = confidence, confidence.count != pixels { throw CaptureFormatError.badLength }
        if let mask = mask, mask.count != pixels { throw CaptureFormatError.badLength }
        guard tracking <= 2 else { throw CaptureFormatError.badTracking }
        guard orientation <= 3 else { throw CaptureFormatError.badOrientation }
        guard color.count <= Int(UInt32.max) else { throw CaptureFormatError.badValue }

        var depthBytes = depth
        var confidenceBytes = confidence ?? Data()
        var maskBytes = mask ?? Data()
        var deflated = false
        if compressed {
            // An absent plane stays empty; a present one must compress.
            let packedDepth: Data? = RawDeflate.compress(depth)
            let packedConfidence: Data? = confidence == nil ? Data() : RawDeflate.compress(confidenceBytes)
            let packedMask: Data? = mask == nil ? Data() : RawDeflate.compress(maskBytes)
            if let d = packedDepth, let c = packedConfidence, let m = packedMask {
                depthBytes = d
                confidenceBytes = c
                maskBytes = m
                deflated = true
            }
        }

        var flagByte: UInt8 = 0
        if confidence != nil { flagByte |= CaptureFrame.flagConfidence }
        if deflated { flagByte |= CaptureFrame.flagDeflate }
        if mask != nil { flagByte |= CaptureFrame.flagMask }

        var out = Data()
        out.reserveCapacity(
            CaptureFrame.headerSize + color.count + depthBytes.count + confidenceBytes.count + maskBytes.count)
        out.append(contentsOf: CaptureFrame.magic)                  //   0
        out.append(CaptureFrame.version)                            //   4
        out.append(flagByte)                                        //   5
        out.append(tracking)                                        //   6
        out.append(orientation)                                     //   7
        out.appendU32LE(index)                                      //   8
        out.appendU32LE(0)                                          //  12 reserved
        out.appendU64LE(timestamp.bitPattern)                       //  16
        for value in worldFromCam {                                 //  24
            out.appendU32LE(value.bitPattern)
        }
        out.appendU32LE(fx.bitPattern)                              //  88
        out.appendU32LE(fy.bitPattern)                              //  92
        out.appendU32LE(cx.bitPattern)                              //  96
        out.appendU32LE(cy.bitPattern)                              // 100
        out.appendU16LE(UInt16(colorWidth))                         // 104
        out.appendU16LE(UInt16(colorHeight))                        // 106
        out.appendU32LE(UInt32(color.count))                        // 108
        out.appendU16LE(UInt16(depthWidth))                         // 112
        out.appendU16LE(UInt16(depthHeight))                        // 114
        out.appendU32LE(UInt32(depthBytes.count))                   // 116
        out.appendU32LE(UInt32(confidenceBytes.count))              // 120
        out.appendU32LE(UInt32(maskBytes.count))                    // 124
        precondition(out.count == CaptureFrame.headerSize)
        out.append(color)                                           // 128
        out.append(depthBytes)
        out.append(confidenceBytes)
        out.append(maskBytes)
        return out
    }

    // MARK: Decoding

    /// The fields of the 128-byte header, validated.
    public struct Header: Equatable {
        public var flags: UInt8
        public var tracking: UInt8
        public var orientation: UInt8
        public var index: UInt32
        public var reserved: UInt32
        public var timestamp: Double
        public var worldFromCam: [Float]
        public var fx: Float
        public var fy: Float
        public var cx: Float
        public var cy: Float
        public var colorWidth: Int
        public var colorHeight: Int
        public var colorLength: Int
        public var depthWidth: Int
        public var depthHeight: Int
        public var depthLength: Int
        public var confidenceLength: Int
        public var maskLength: Int

        public var totalLength: Int {
            return CaptureFrame.headerSize + colorLength + depthLength + confidenceLength + maskLength
        }
    }

    /// Parses and checks the header. `bytes` may hold more than the header.
    public static func parseHeader(_ bytes: [UInt8]) throws -> Header {
        guard bytes.count >= headerSize else { throw CaptureFormatError.tooShort }
        guard Array(bytes[0..<4]) == magic else { throw CaptureFormatError.badMagic }
        guard bytes[4] == version else { throw CaptureFormatError.badVersion }
        let flags = bytes[5]
        let knownFlags: UInt8 = flagConfidence | flagDeflate | flagMask
        guard (flags & ~knownFlags) == 0 else { throw CaptureFormatError.badFlags }
        let tracking = bytes[6]
        guard tracking <= 2 else { throw CaptureFormatError.badTracking }
        let orientation = bytes[7]
        guard orientation <= 3 else { throw CaptureFormatError.badOrientation }

        var pose = [Float]()
        pose.reserveCapacity(16)
        for i in 0..<16 {
            pose.append(Float(bitPattern: readU32LE(bytes, 24 + 4 * i)))
        }
        let header = Header(
            flags: flags,
            tracking: tracking,
            orientation: orientation,
            index: readU32LE(bytes, 8),
            reserved: readU32LE(bytes, 12),
            timestamp: Double(bitPattern: readU64LE(bytes, 16)),
            worldFromCam: pose,
            fx: Float(bitPattern: readU32LE(bytes, 88)),
            fy: Float(bitPattern: readU32LE(bytes, 92)),
            cx: Float(bitPattern: readU32LE(bytes, 96)),
            cy: Float(bitPattern: readU32LE(bytes, 100)),
            colorWidth: Int(readU16LE(bytes, 104)),
            colorHeight: Int(readU16LE(bytes, 106)),
            colorLength: Int(readU32LE(bytes, 108)),
            depthWidth: Int(readU16LE(bytes, 112)),
            depthHeight: Int(readU16LE(bytes, 114)),
            depthLength: Int(readU32LE(bytes, 116)),
            confidenceLength: Int(readU32LE(bytes, 120)),
            maskLength: Int(readU32LE(bytes, 124))
        )

        guard header.depthWidth >= 1, header.depthHeight >= 1 else { throw CaptureFormatError.badDimensions }
        let pixels = header.depthWidth * header.depthHeight
        guard pixels <= maxDepthPixels else { throw CaptureFormatError.badDimensions }
        let hasConfidence = (flags & flagConfidence) != 0
        let hasMask = (flags & flagMask) != 0
        guard hasConfidence == (header.confidenceLength != 0) else { throw CaptureFormatError.badLength }
        guard hasMask == (header.maskLength != 0) else { throw CaptureFormatError.badLength }
        if (flags & flagDeflate) == 0 {
            guard header.depthLength == 2 * pixels else { throw CaptureFormatError.badLength }
            if hasConfidence, header.confidenceLength != pixels { throw CaptureFormatError.badLength }
            if hasMask, header.maskLength != pixels { throw CaptureFormatError.badLength }
        } else if header.depthLength == 0 {
            throw CaptureFormatError.badLength
        }
        return header
    }

    /// Parses one frame, inflating its planes if needed.
    ///
    /// The four lengths are `Int` (64 bits) here, so their sum cannot wrap
    /// around the way a `UInt32` sum would.
    public static func decode(_ data: Data) throws -> CaptureFrame {
        let bytes = [UInt8](data)
        let header = try parseHeader(bytes)
        guard bytes.count == header.totalLength else { throw CaptureFormatError.badLength }

        var position = headerSize
        let color = Data(bytes[position..<(position + header.colorLength)])
        position += header.colorLength
        var depth = Data(bytes[position..<(position + header.depthLength)])
        position += header.depthLength
        var confidence = Data(bytes[position..<(position + header.confidenceLength)])
        position += header.confidenceLength
        var mask = Data(bytes[position..<(position + header.maskLength)])

        let pixels = header.depthWidth * header.depthHeight
        let deflated = (header.flags & flagDeflate) != 0
        if deflated {
            guard let inflated = RawDeflate.decompress(depth, expectedSize: 2 * pixels) else {
                throw CaptureFormatError.badDeflate
            }
            depth = inflated
            if !confidence.isEmpty {
                guard let inflated = RawDeflate.decompress(confidence, expectedSize: pixels) else {
                    throw CaptureFormatError.badDeflate
                }
                confidence = inflated
            }
            if !mask.isEmpty {
                guard let inflated = RawDeflate.decompress(mask, expectedSize: pixels) else {
                    throw CaptureFormatError.badDeflate
                }
                mask = inflated
            }
        }

        return CaptureFrame(
            index: header.index,
            timestamp: header.timestamp,
            worldFromCam: header.worldFromCam,
            fx: header.fx,
            fy: header.fy,
            cx: header.cx,
            cy: header.cy,
            tracking: header.tracking,
            orientation: header.orientation,
            colorWidth: header.colorWidth,
            colorHeight: header.colorHeight,
            color: color,
            depthWidth: header.depthWidth,
            depthHeight: header.depthHeight,
            depth: depth,
            confidence: (header.flags & flagConfidence) != 0 ? confidence : nil,
            mask: (header.flags & flagMask) != 0 ? mask : nil,
            compressed: deflated
        )
    }

    /// The depth plane as millimeters, row-major.
    public var depthMillimeters: [UInt16] {
        let bytes = [UInt8](depth)
        var values = [UInt16]()
        values.reserveCapacity(bytes.count / 2)
        var i = 0
        while i + 1 < bytes.count {
            values.append(readU16LE(bytes, i))
            i += 2
        }
        return values
    }
}

// MARK: Little-endian helpers

extension Data {
    mutating func appendU16LE(_ value: UInt16) {
        append(UInt8(truncatingIfNeeded: value))
        append(UInt8(truncatingIfNeeded: value >> 8))
    }

    mutating func appendU32LE(_ value: UInt32) {
        append(UInt8(truncatingIfNeeded: value))
        append(UInt8(truncatingIfNeeded: value >> 8))
        append(UInt8(truncatingIfNeeded: value >> 16))
        append(UInt8(truncatingIfNeeded: value >> 24))
    }

    mutating func appendU64LE(_ value: UInt64) {
        appendU32LE(UInt32(truncatingIfNeeded: value))
        appendU32LE(UInt32(truncatingIfNeeded: value >> 32))
    }
}

func readU16LE(_ bytes: [UInt8], _ offset: Int) -> UInt16 {
    return UInt16(bytes[offset]) | (UInt16(bytes[offset + 1]) << 8)
}

func readU32LE(_ bytes: [UInt8], _ offset: Int) -> UInt32 {
    return UInt32(bytes[offset])
        | (UInt32(bytes[offset + 1]) << 8)
        | (UInt32(bytes[offset + 2]) << 16)
        | (UInt32(bytes[offset + 3]) << 24)
}

func readU64LE(_ bytes: [UInt8], _ offset: Int) -> UInt64 {
    return UInt64(readU32LE(bytes, offset)) | (UInt64(readU32LE(bytes, offset + 4)) << 32)
}
