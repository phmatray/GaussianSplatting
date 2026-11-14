import CryptoKit
import Foundation

/// Why a pairing URI was refused. `code` is the identifier used by the
/// fixtures and by the Python reference (tools/gscap.py).
public enum PairingError: Error, Equatable {
    case badScheme
    case missingHost
    case missingPort
    case missingFingerprint
    case missingToken
    case duplicateParameter
    case badEncoding
    case badHost
    case badPort
    case badFingerprint
    case badToken

    public var code: String {
        switch self {
        case .badScheme: return "bad_scheme"
        case .missingHost: return "missing_host"
        case .missingPort: return "missing_port"
        case .missingFingerprint: return "missing_fp"
        case .missingToken: return "missing_token"
        case .duplicateParameter: return "duplicate_parameter"
        case .badEncoding: return "bad_encoding"
        case .badHost: return "bad_host"
        case .badPort: return "bad_port"
        case .badFingerprint: return "bad_fp"
        case .badToken: return "bad_token"
        }
    }
}

/// What the QR code of the desktop app says:
/// `splatlive://pair?host=<ip>&port=<lan port>&fp=<sha256>&token=<t>`.
public struct Pairing: Equatable {
    public static let prefix = "splatlive://pair"

    public let host: String
    public let port: Int
    /// SHA-256 of the server certificate (DER), 64 lowercase hex digits.
    public let fingerprint: String
    /// 32 lowercase hex digits.
    public let token: String

    public init(host: String, port: Int, fingerprint: String, token: String) {
        self.host = host
        self.port = port
        self.fingerprint = fingerprint
        self.token = token
    }

    /// Parses a pairing URI. The rules are those of `parse_pairing_uri` in
    /// tools/gscap.py, implemented on bytes so that both agree on any input:
    ///
    /// 1. Leading and trailing spaces, tabs, CR and LF are ignored.
    /// 2. The text is split at the first "?"; what precedes must be
    ///    "splatlive://pair" (A-Z compared without case).
    /// 3. The query is split on "&" (empty items skipped), each item at its
    ///    first "=", and both halves are percent-decoded.
    /// 4. host, port, fp and token are required and may appear only once;
    ///    other parameters are ignored.
    /// 5. host: optional brackets removed; letters, digits, "." and "-", or
    ///    an IPv6 literal (hex digits, "." and at least two ":"); lowercased.
    /// 6. port: 1 to 5 digits, 1...65535.
    /// 7. fp: colons removed, 64 hex digits, lowercased.
    /// 8. token: 32 hex digits, lowercased.
    public init(uri: String) throws {
        var bytes = Array(uri.utf8)
        while let last = bytes.last, Pairing.isBlank(last) {
            bytes.removeLast()
        }
        while let first = bytes.first, Pairing.isBlank(first) {
            bytes.removeFirst()
        }
        guard let mark = bytes.firstIndex(of: 0x3F) else {  // "?"
            throw PairingError.badScheme
        }
        let head = Pairing.asciiLowercased(Array(bytes[0..<mark]))
        guard head == Array(Pairing.prefix.utf8) else {
            throw PairingError.badScheme
        }

        var values: [String: [UInt8]] = [:]
        let query = bytes[(mark + 1)...]
        for item in query.split(separator: 0x26, omittingEmptySubsequences: true) {  // "&"
            let rawKey: [UInt8]
            let rawValue: [UInt8]
            if let equals = item.firstIndex(of: 0x3D) {  // "="
                rawKey = Array(item[item.startIndex..<equals])
                rawValue = Array(item[(equals + 1)..<item.endIndex])
            } else {
                rawKey = Array(item)
                rawValue = []
            }
            let keyBytes = try Pairing.percentDecoded(rawKey)
            let valueBytes = try Pairing.percentDecoded(rawValue)
            guard let key = String(bytes: keyBytes, encoding: .utf8),
                  String(bytes: valueBytes, encoding: .utf8) != nil else {
                throw PairingError.badEncoding
            }
            if key == "host" || key == "port" || key == "fp" || key == "token" {
                if values[key] != nil {
                    throw PairingError.duplicateParameter
                }
                values[key] = valueBytes
            }
        }
        guard var hostBytes = values["host"] else { throw PairingError.missingHost }
        guard let portBytes = values["port"] else { throw PairingError.missingPort }
        guard let fingerprintBytes = values["fp"] else { throw PairingError.missingFingerprint }
        guard let tokenBytes = values["token"] else { throw PairingError.missingToken }

        // Host.
        if hostBytes.count >= 2, hostBytes.first == 0x5B, hostBytes.last == 0x5D {  // "[" "]"
            hostBytes = Array(hostBytes[1..<(hostBytes.count - 1)])
        }
        guard !hostBytes.isEmpty, hostBytes.count <= 253 else { throw PairingError.badHost }
        if hostBytes.contains(0x3A) {  // ":" means an IPv6 literal
            var colons = 0
            for byte in hostBytes {
                guard Pairing.isHex(byte) || byte == 0x3A || byte == 0x2E else { throw PairingError.badHost }
                if byte == 0x3A {
                    colons += 1
                }
            }
            // An IPv6 literal has at least two colons; "1.2.3.4:7879" is a mistake.
            guard colons >= 2 else { throw PairingError.badHost }
        } else {
            for byte in hostBytes {
                guard Pairing.isLetterOrDigit(byte) || byte == 0x2E || byte == 0x2D else {
                    throw PairingError.badHost
                }
            }
            let first = hostBytes[0]
            let last = hostBytes[hostBytes.count - 1]
            guard first != 0x2E, first != 0x2D, last != 0x2E, last != 0x2D else { throw PairingError.badHost }
        }

        // Port.
        guard (1...5).contains(portBytes.count) else { throw PairingError.badPort }
        var portValue = 0
        for byte in portBytes {
            guard byte >= 0x30, byte <= 0x39 else { throw PairingError.badPort }
            portValue = portValue * 10 + Int(byte - 0x30)
        }
        guard (1...65535).contains(portValue) else { throw PairingError.badPort }

        // Fingerprint and token.
        guard let fingerprint = Pairing.normalisedFingerprint(fingerprintBytes) else {
            throw PairingError.badFingerprint
        }
        guard tokenBytes.count == 32, tokenBytes.allSatisfy({ Pairing.isHex($0) }) else {
            throw PairingError.badToken
        }

        self.host = String(decoding: Pairing.asciiLowercased(hostBytes), as: UTF8.self)
        self.port = portValue
        self.fingerprint = fingerprint
        self.token = String(decoding: Pairing.asciiLowercased(tokenBytes), as: UTF8.self)
    }

    /// "host:port", with brackets around an IPv6 literal.
    public var authority: String {
        if host.contains(":") {
            return "[\(host)]:\(port)"
        }
        return "\(host):\(port)"
    }

    public var ingestURLString: String { return "wss://\(authority)/ingest" }
    public var capturesURLString: String { return "https://\(authority)/api/captures" }
    public var infoURLString: String { return "https://\(authority)/api/info" }

    public var ingestURL: URL? { return URL(string: ingestURLString) }
    public var capturesURL: URL? { return URL(string: capturesURLString) }
    public var infoURL: URL? { return URL(string: infoURLString) }

    /// The canonical URI of this pairing.
    public var uri: String {
        return "\(Pairing.prefix)?host=\(host)&port=\(port)&fp=\(fingerprint)&token=\(token)"
    }

    /// 64 lowercase hex digits from hex text that may contain colons and
    /// upper case (the form `/api/info` shows), or nil.
    public static func normalisedFingerprint(_ text: String) -> String? {
        return normalisedFingerprint(Array(text.utf8))
    }

    static func normalisedFingerprint(_ bytes: [UInt8]) -> String? {
        let digits = bytes.filter { $0 != 0x3A }
        guard digits.count == 64, digits.allSatisfy({ isHex($0) }) else {
            return nil
        }
        return String(decoding: asciiLowercased(digits), as: UTF8.self)
    }

    // MARK: Byte helpers

    static func isBlank(_ byte: UInt8) -> Bool {
        return byte == 0x20 || byte == 0x09 || byte == 0x0D || byte == 0x0A
    }

    static func isHex(_ byte: UInt8) -> Bool {
        return (byte >= 0x30 && byte <= 0x39) || (byte >= 0x41 && byte <= 0x46) || (byte >= 0x61 && byte <= 0x66)
    }

    static func isLetterOrDigit(_ byte: UInt8) -> Bool {
        return (byte >= 0x30 && byte <= 0x39) || (byte >= 0x41 && byte <= 0x5A) || (byte >= 0x61 && byte <= 0x7A)
    }

    static func hexValue(_ byte: UInt8) -> UInt8? {
        if byte >= 0x30 && byte <= 0x39 { return byte - 0x30 }
        if byte >= 0x41 && byte <= 0x46 { return byte - 0x41 + 10 }
        if byte >= 0x61 && byte <= 0x66 { return byte - 0x61 + 10 }
        return nil
    }

    /// Lowercases A-Z only.
    static func asciiLowercased(_ bytes: [UInt8]) -> [UInt8] {
        return bytes.map { ($0 >= 0x41 && $0 <= 0x5A) ? $0 + 0x20 : $0 }
    }

    /// Replaces every "%XX" by its byte; a "%" without two hex digits is an error.
    static func percentDecoded(_ bytes: [UInt8]) throws -> [UInt8] {
        var out = [UInt8]()
        out.reserveCapacity(bytes.count)
        var i = 0
        while i < bytes.count {
            if bytes[i] == 0x25 {  // "%"
                guard i + 2 < bytes.count,
                      let high = hexValue(bytes[i + 1]),
                      let low = hexValue(bytes[i + 2]) else {
                    throw PairingError.badEncoding
                }
                out.append(high * 16 + low)
                i += 3
            } else {
                out.append(bytes[i])
                i += 1
            }
        }
        return out
    }
}

/// The form of `fp`: SHA-256 of a certificate's DER encoding.
public enum CertificateFingerprint {
    public static func sha256Hex(_ der: Data) -> String {
        return SHA256.hash(data: der).map { String(format: "%02x", $0) }.joined()
    }
}
