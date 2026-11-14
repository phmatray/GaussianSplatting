import Foundation

// The capture file `.gscap`: 8 bytes "GSCAP1\0\0", a u32 length and that many
// bytes of JSON, then for every frame a u32 length and the frame exactly as
// on the wire. Lengths are little-endian.
//
// The JSON holds the `hello` fields without `type` and `token` (protocol,
// device, app, mode, name) plus `created` (RFC 3339) and optionally
// `region` ({"center":[x,y,z],"radius":r}) and `floor` ({"y":h}).
//
// The phone reserves `GscapWriter.headerPadding` bytes for the JSON and
// fills the rest with spaces, so that it can rewrite the header in place
// when the recording ends (region and floor are usually known only after
// the first frames). Trailing spaces are valid JSON for every parser.

public enum GscapError: Error, Equatable {
    case badFileMagic
    case badHeader
    case badLength
    case headerTooLarge
    case cannotCreate

    public var code: String {
        switch self {
        case .badFileMagic: return "bad_file_magic"
        case .badHeader: return "bad_header"
        case .badLength: return "bad_length"
        case .headerTooLarge: return "header_too_large"
        case .cannotCreate: return "cannot_create"
        }
    }
}

public struct GscapHeader: Equatable {
    public var protocolVersion: Int
    public var device: String
    public var app: String
    public var mode: String
    public var name: String
    /// RFC 3339, for example "2026-11-14T09:30:00Z".
    public var created: String
    public var regionCenter: [Float]?
    public var regionRadius: Float?
    public var floorY: Float?

    public init(
        protocolVersion: Int = captureProtocolVersion,
        device: String,
        app: String,
        mode: String,
        name: String,
        created: String,
        regionCenter: [Float]? = nil,
        regionRadius: Float? = nil,
        floorY: Float? = nil
    ) {
        self.protocolVersion = protocolVersion
        self.device = device
        self.app = app
        self.mode = mode
        self.name = name
        self.created = created
        self.regionCenter = regionCenter
        self.regionRadius = regionRadius
        self.floorY = floorY
    }

    /// The current time in the form of `created`.
    public static func timestampNow() -> String {
        return ISO8601DateFormatter().string(from: Date())
    }

    public var jsonObject: [String: Any] {
        var object: [String: Any] = [
            "protocol": protocolVersion,
            "device": device,
            "app": app,
            "mode": mode,
            "name": name,
            "created": created,
        ]
        if let center = regionCenter, center.count == 3, let radius = regionRadius {
            object["region"] = [
                "center": center.map { ClientMessage.number($0) },
                "radius": ClientMessage.number(radius),
            ] as [String: Any]
        }
        if let y = floorY {
            object["floor"] = ["y": ClientMessage.number(y)] as [String: Any]
        }
        return object
    }

    /// Lenient: missing fields get defaults, unknown ones are ignored, and
    /// `floor` may be `{"y": h}` or a bare number.
    public init(jsonObject object: [String: Any]) {
        protocolVersion = (object["protocol"] as? NSNumber)?.intValue ?? 0
        device = (object["device"] as? String) ?? ""
        app = (object["app"] as? String) ?? ""
        mode = (object["mode"] as? String) ?? ""
        name = (object["name"] as? String) ?? ""
        created = (object["created"] as? String) ?? ""
        regionCenter = nil
        regionRadius = nil
        floorY = nil
        if let region = object["region"] as? [String: Any],
           let center = region["center"] as? [NSNumber], center.count == 3,
           let radius = region["radius"] as? NSNumber {
            regionCenter = center.map { $0.floatValue }
            regionRadius = radius.floatValue
        }
        if let floor = object["floor"] as? [String: Any], let y = floor["y"] as? NSNumber {
            floorY = y.floatValue
        } else if let y = object["floor"] as? NSNumber {
            floorY = y.floatValue
        }
    }
}

public enum GscapFile {
    public static let magic: [UInt8] = [0x47, 0x53, 0x43, 0x41, 0x50, 0x31, 0x00, 0x00]  // "GSCAP1\0\0"
    public static let maxHeaderJSON = 1 << 20
    public static let maxFrameBytes = 64 << 20
    public static let fileExtension = "gscap"

    /// Magic, length and JSON; the JSON is padded with spaces to `padTo` bytes.
    public static func encodeHeader(_ header: GscapHeader, padTo: Int = 0) throws -> Data {
        let object = header.jsonObject
        guard JSONSerialization.isValidJSONObject(object) else {
            throw GscapError.badHeader
        }
        var json = try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
        if json.count < padTo {
            json.append(Data(repeating: 0x20, count: padTo - json.count))
        }
        guard json.count <= maxHeaderJSON else {
            throw GscapError.headerTooLarge
        }
        var out = Data()
        out.append(contentsOf: magic)
        out.appendU32LE(UInt32(json.count))
        out.append(json)
        return out
    }
}

/// Writes a `.gscap`. Not thread-safe: use it from one queue.
public final class GscapWriter {
    /// Bytes reserved for the JSON header.
    public static let headerPadding = 1024

    public let url: URL
    public private(set) var frameCount = 0
    public private(set) var byteCount = 0
    private let handle: FileHandle

    public init(url: URL, header: GscapHeader) throws {
        let head = try GscapFile.encodeHeader(header, padTo: GscapWriter.headerPadding)
        guard head.count == GscapFile.magic.count + 4 + GscapWriter.headerPadding else {
            throw GscapError.headerTooLarge
        }
        guard FileManager.default.createFile(atPath: url.path, contents: head) else {
            throw GscapError.cannotCreate
        }
        let handle = try FileHandle(forWritingTo: url)
        _ = try handle.seekToEnd()
        self.handle = handle
        self.url = url
        self.byteCount = head.count
    }

    /// Appends one encoded frame (the bytes of `CaptureFrame.encoded()`).
    public func append(_ frame: Data) throws {
        guard frame.count >= CaptureFrame.headerSize, frame.count <= GscapFile.maxFrameBytes else {
            throw GscapError.badLength
        }
        var record = Data()
        record.reserveCapacity(4 + frame.count)
        record.appendU32LE(UInt32(frame.count))
        record.append(frame)
        do {
            try handle.write(contentsOf: record)
        } catch {
            // Half a record (storage full) would make every later record
            // unreadable: cut the file back to its last complete frame.
            try? handle.truncate(atOffset: UInt64(byteCount))
            _ = try? handle.seekToEnd()
            throw error
        }
        frameCount += 1
        byteCount += record.count
    }

    /// Replaces the header in place (same reserved size) and returns to the end.
    public func rewriteHeader(_ header: GscapHeader) throws {
        let head = try GscapFile.encodeHeader(header, padTo: GscapWriter.headerPadding)
        guard head.count == GscapFile.magic.count + 4 + GscapWriter.headerPadding else {
            throw GscapError.headerTooLarge
        }
        try handle.seek(toOffset: 0)
        try handle.write(contentsOf: head)
        _ = try handle.seekToEnd()
    }

    public func close() throws {
        try handle.close()
    }
}

/// Reads a `.gscap` frame by frame.
public final class GscapReader {
    public let header: GscapHeader
    /// The header exactly as parsed (all keys).
    public let headerObject: [String: Any]
    /// File offset of the first frame record.
    public let framesOffset: UInt64
    public let fileSize: UInt64
    /// True once the end was reached in the middle of a frame record: a
    /// recording cut short. The complete frames remain valid.
    public private(set) var truncated = false
    private let handle: FileHandle
    private var position: UInt64

    public init(url: URL) throws {
        let handle = try FileHandle(forReadingFrom: url)
        var size: UInt64 = 0
        var object: [String: Any] = [:]
        var offset: UInt64 = 0
        do {
            size = try handle.seekToEnd()
            try handle.seek(toOffset: 0)
            let magic = try GscapReader.read(handle, count: GscapFile.magic.count)
            guard [UInt8](magic) == GscapFile.magic else {
                throw GscapError.badFileMagic
            }
            let lengthData = try GscapReader.read(handle, count: 4)
            guard lengthData.count == 4 else {
                throw GscapError.badHeader
            }
            let length = Int(readU32LE([UInt8](lengthData), 0))
            guard length <= GscapFile.maxHeaderJSON else {
                throw GscapError.badHeader
            }
            let json = try GscapReader.read(handle, count: length)
            guard json.count == length else {
                throw GscapError.badHeader
            }
            // The padding (spaces) after the JSON text is dropped before parsing.
            var jsonBytes = [UInt8](json)
            while let last = jsonBytes.last, last == 0x20 || last == 0x0A || last == 0x0D || last == 0x09 {
                jsonBytes.removeLast()
            }
            guard let parsed = try? JSONSerialization.jsonObject(with: Data(jsonBytes), options: []),
                  let fields = parsed as? [String: Any] else {
                throw GscapError.badHeader
            }
            object = fields
            offset = UInt64(GscapFile.magic.count + 4 + length)
        } catch {
            try? handle.close()
            throw error
        }
        self.headerObject = object
        self.header = GscapHeader(jsonObject: object)
        self.framesOffset = offset
        self.fileSize = size
        self.handle = handle
        self.position = offset
    }

    /// The next frame record, or nil at the end of the file (also when the
    /// file ends inside a record; `truncated` then becomes true).
    public func nextFrame() throws -> Data? {
        guard let length = try nextRecordLength() else {
            return nil
        }
        let data = try GscapReader.read(handle, count: length)
        guard data.count == length else {
            truncated = true
            position = fileSize
            return nil
        }
        position += UInt64(4 + length)
        return data
    }

    /// Skips the next frame; returns its first `prefix` bytes (enough for
    /// the 128-byte header) or nil at the end.
    public func skipFrame(readingPrefix prefix: Int = 0) throws -> Data? {
        guard let length = try nextRecordLength() else {
            return nil
        }
        let head = try GscapReader.read(handle, count: min(prefix, length))
        position += UInt64(4 + length)
        try handle.seek(toOffset: position)
        return head
    }

    public func rewind() throws {
        truncated = false
        position = framesOffset
        try handle.seek(toOffset: position)
    }

    public func close() {
        try? handle.close()
    }

    /// Reads the length of the record at `position` and checks that the
    /// record fits in the file. Leaves the file at the start of the frame.
    private func nextRecordLength() throws -> Int? {
        if position >= fileSize {
            return nil
        }
        if position + 4 > fileSize {
            truncated = true
            position = fileSize
            return nil
        }
        try handle.seek(toOffset: position)
        let lengthData = try GscapReader.read(handle, count: 4)
        let lengthBytes = [UInt8](lengthData)
        guard lengthBytes.count == 4 else {
            truncated = true
            position = fileSize
            return nil
        }
        let length = Int(readU32LE(lengthBytes, 0))
        guard length >= CaptureFrame.headerSize, length <= GscapFile.maxFrameBytes else {
            throw GscapError.badLength
        }
        if position + UInt64(4 + length) > fileSize {
            truncated = true
            position = fileSize
            return nil
        }
        return length
    }

    /// Up to `count` bytes (fewer only at the end of the file).
    private static func read(_ handle: FileHandle, count: Int) throws -> Data {
        var out = Data()
        while out.count < count {
            guard let chunk = try handle.read(upToCount: count - out.count), !chunk.isEmpty else {
                break
            }
            out.append(chunk)
        }
        return out
    }
}

/// What the list of recordings shows about a file.
public struct GscapSummary: Equatable {
    public var header: GscapHeader
    public var frameCount: Int
    public var fileSize: UInt64
    /// Seconds between the first and the last frame.
    public var duration: Double
    public var truncated: Bool

    /// Walks the records without reading the images.
    public static func read(url: URL) throws -> GscapSummary {
        let reader = try GscapReader(url: url)
        defer { reader.close() }
        var count = 0
        var first: Double?
        var last: Double?
        while let head = try reader.skipFrame(readingPrefix: 24) {
            count += 1
            if head.count == 24 {
                let time = Double(bitPattern: readU64LE([UInt8](head), 16))
                if first == nil {
                    first = time
                }
                last = time
            }
        }
        var duration = 0.0
        if let first = first, let last = last, last >= first, (last - first).isFinite {
            duration = last - first
        }
        return GscapSummary(
            header: reader.header,
            frameCount: count,
            fileSize: reader.fileSize,
            duration: duration,
            truncated: reader.truncated
        )
    }
}
