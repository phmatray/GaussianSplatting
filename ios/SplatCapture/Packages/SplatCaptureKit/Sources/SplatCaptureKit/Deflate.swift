import Compression
import Foundation

/// Raw deflate (RFC 1951, no zlib header, no checksum) with Apple's
/// Compression framework: `COMPRESSION_ZLIB` is exactly that format. It is
/// what `zlib.decompress(data, wbits=-15)` reads in Python.
public enum RawDeflate {
    /// Deflates `input`. Returns nil for empty input or if the compressor
    /// fails (the caller then sends the plane uncompressed).
    public static func compress(_ input: Data) -> Data? {
        if input.isEmpty {
            return nil
        }
        // Deflate can grow incompressible data by a few bytes per block.
        let capacity = input.count + input.count / 8 + 256
        let destination = UnsafeMutablePointer<UInt8>.allocate(capacity: capacity)
        defer { destination.deallocate() }
        let written: Int = input.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> Int in
            guard let source = raw.bindMemory(to: UInt8.self).baseAddress else {
                return 0
            }
            return compression_encode_buffer(destination, capacity, source, input.count, nil, COMPRESSION_ZLIB)
        }
        if written <= 0 {
            return nil
        }
        return Data(bytes: destination, count: written)
    }

    /// Inflates one stream that must give exactly `expectedSize` bytes.
    /// Returns nil if it gives fewer or more, or is not a deflate stream.
    public static func decompress(_ input: Data, expectedSize: Int) -> Data? {
        if input.isEmpty || expectedSize <= 0 {
            return nil
        }
        // One spare byte so that a stream that is too long is noticed.
        let capacity = expectedSize + 1
        let destination = UnsafeMutablePointer<UInt8>.allocate(capacity: capacity)
        defer { destination.deallocate() }
        let written: Int = input.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> Int in
            guard let source = raw.bindMemory(to: UInt8.self).baseAddress else {
                return 0
            }
            return compression_decode_buffer(destination, capacity, source, input.count, nil, COMPRESSION_ZLIB)
        }
        if written != expectedSize {
            return nil
        }
        return Data(bytes: destination, count: written)
    }
}
