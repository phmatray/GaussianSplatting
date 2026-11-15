import Foundation
import XCTest
import SplatCaptureKit

final class PairingTests: XCTestCase {
    func testDocumentedExample() throws {
        let fingerprint = String(repeating: "ab", count: 32)
        let token = String(repeating: "0f", count: 16)
        let pairing = try Pairing(uri: "splatlive://pair?host=192.168.1.20&port=7879&fp=\(fingerprint)&token=\(token)")
        XCTAssertEqual(pairing.host, "192.168.1.20")
        XCTAssertEqual(pairing.port, 7879)
        XCTAssertEqual(pairing.fingerprint, fingerprint)
        XCTAssertEqual(pairing.token, token)
        XCTAssertEqual(pairing.ingestURLString, "wss://192.168.1.20:7879/ingest")
        XCTAssertEqual(pairing.capturesURLString, "https://192.168.1.20:7879/api/captures")
        XCTAssertEqual(pairing.ingestURL?.scheme, "wss")
        XCTAssertEqual(pairing.capturesURL?.port, 7879)
        XCTAssertEqual(try Pairing(uri: pairing.uri), pairing)
    }

    func testFixtureCases() throws {
        try Fixtures.require()
        let cases = try XCTUnwrap(Fixtures.json("pairing_uris.json") as? [[String: Any]])
        XCTAssertGreaterThan(cases.count, 40)
        for entry in cases {
            let uri = try XCTUnwrap(entry["uri"] as? String)
            if (entry["ok"] as? Bool) == true {
                let pairing: Pairing
                do {
                    pairing = try Pairing(uri: uri)
                } catch {
                    XCTFail("refused a valid URI (\(error)): \(uri)")
                    continue
                }
                XCTAssertEqual(pairing.host, entry["host"] as? String, uri)
                XCTAssertEqual(pairing.port, integer(entry["port"]), uri)
                XCTAssertEqual(pairing.fingerprint, entry["fp"] as? String, uri)
                XCTAssertEqual(pairing.token, entry["token"] as? String, uri)
                XCTAssertEqual(pairing.ingestURLString, entry["ingest_url"] as? String, uri)
                XCTAssertEqual(pairing.capturesURLString, entry["captures_url"] as? String, uri)
                XCTAssertNotNil(pairing.ingestURL, uri)
                XCTAssertEqual(try Pairing(uri: pairing.uri), pairing, uri)
            } else {
                XCTAssertThrowsError(try Pairing(uri: uri), uri) { error in
                    XCTAssertEqual((error as? PairingError)?.code, entry["error"] as? String, uri)
                }
            }
        }
    }

    func testFingerprint() throws {
        XCTAssertEqual(
            CertificateFingerprint.sha256Hex(Data("abc".utf8)),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        XCTAssertEqual(
            Pairing.normalisedFingerprint("AB:" + String(repeating: "cd:", count: 30) + "EF"),
            "ab" + String(repeating: "cd", count: 30) + "ef")
        XCTAssertNil(Pairing.normalisedFingerprint("abcd"))

        try Fixtures.require()
        let entry = try XCTUnwrap(Fixtures.json("fingerprint.json") as? [String: Any])
        let hex = Array(try XCTUnwrap(entry["der_hex"] as? String).utf8)
        var der = Data()
        var i = 0
        while i + 1 < hex.count {
            let pair = String(decoding: hex[i...(i + 1)], as: UTF8.self)
            der.append(try XCTUnwrap(UInt8(pair, radix: 16)))
            i += 2
        }
        XCTAssertEqual(der.count, 300)
        XCTAssertEqual(CertificateFingerprint.sha256Hex(der), entry["sha256"] as? String)
    }
}

final class MessageTests: XCTestCase {
    private func parsed(_ text: String) throws -> NSDictionary {
        let object = try JSONSerialization.jsonObject(with: Data(text.utf8), options: [])
        return try XCTUnwrap(object as? NSDictionary, text)
    }

    func testHelloMatchesTheSpecification() throws {
        let text = ClientMessage.hello(
            token: "0123456789abcdef0123456789abcdef", device: "iPhone17,2", app: "0.1.0", mode: .room, name: "Salon")
        let expected: NSDictionary = [
            "type": "hello", "protocol": 1, "token": "0123456789abcdef0123456789abcdef",
            "device": "iPhone17,2", "app": "0.1.0", "mode": "room", "name": "Salon",
        ]
        XCTAssertEqual(try parsed(text), expected)
        XCTAssertEqual(try parsed(ClientMessage.pause), ["type": "pause"] as NSDictionary)
        XCTAssertEqual(try parsed(ClientMessage.resume), ["type": "resume"] as NSDictionary)
        XCTAssertEqual(try parsed(ClientMessage.reset), ["type": "reset"] as NSDictionary)
        XCTAssertEqual(try parsed(ClientMessage.finish), ["type": "finish"] as NSDictionary)
        // Values JSON cannot express are sent as 0 instead of crashing.
        XCTAssertEqual(try parsed(ClientMessage.floor(y: Float.nan)), ["type": "floor", "y": 0] as NSDictionary)
    }

    func testClientFixtures() throws {
        try Fixtures.require()
        let all = try XCTUnwrap(Fixtures.json("messages.json") as? [String: Any])
        let cases = try XCTUnwrap(all["client"] as? [[String: Any]])
        XCTAssertGreaterThanOrEqual(cases.count, 8)
        for entry in cases {
            let build = try XCTUnwrap(entry["build"] as? String)
            let args = try XCTUnwrap(entry["args"] as? [String: Any])
            let expected = try XCTUnwrap(entry["json"] as? NSDictionary)
            let text: String
            switch build {
            case "hello":
                text = ClientMessage.hello(
                    token: try XCTUnwrap(args["token"] as? String),
                    device: try XCTUnwrap(args["device"] as? String),
                    app: try XCTUnwrap(args["app"] as? String),
                    mode: try XCTUnwrap(CaptureMode(rawValue: try XCTUnwrap(args["mode"] as? String))),
                    name: try XCTUnwrap(args["name"] as? String))
            case "region":
                let center = try XCTUnwrap(args["center"] as? [NSNumber]).map { $0.floatValue }
                let radius = try XCTUnwrap(args["radius"] as? NSNumber).floatValue
                text = ClientMessage.region(center: center, radius: radius)
            case "floor":
                text = ClientMessage.floor(y: try XCTUnwrap(args["y"] as? NSNumber).floatValue)
            case "pause":
                text = ClientMessage.pause
            case "resume":
                text = ClientMessage.resume
            case "reset":
                text = ClientMessage.reset
            case "finish":
                text = ClientMessage.finish
            default:
                XCTFail("unknown message \(build)")
                continue
            }
            XCTAssertEqual(try parsed(text), expected, build)
        }
    }

    func testServerFixtures() throws {
        try Fixtures.require()
        let all = try XCTUnwrap(Fixtures.json("messages.json") as? [String: Any])
        let cases = try XCTUnwrap(all["server"] as? [[String: Any]])
        XCTAssertGreaterThanOrEqual(cases.count, 20)
        for entry in cases {
            let text = try XCTUnwrap(entry["text"] as? String)
            let message = ServerMessage.parse(text)
            guard let expect = entry["expect"] as? [String: Any] else {
                XCTAssertNil(message, text)
                continue
            }
            let kind = try XCTUnwrap(expect["kind"] as? String)
            guard let message = message else {
                XCTFail("dropped a message that should parse as \(kind): \(text)")
                continue
            }
            switch message {
            case .welcome(let protocolVersion, let maxInflight):
                XCTAssertEqual(kind, "welcome", text)
                XCTAssertEqual(protocolVersion, integer(expect["protocol"]), text)
                XCTAssertEqual(maxInflight, integer(expect["max_inflight"]), text)
            case .ack(let frame, let used):
                XCTAssertEqual(kind, "ack", text)
                XCTAssertEqual(UInt64(frame), (expect["frame"] as? NSNumber)?.uint64Value, text)
                XCTAssertEqual(used, expect["used"] as? Bool, text)
            case .progress(let frames, let splats):
                XCTAssertEqual(kind, "progress", text)
                XCTAssertEqual(frames, integer(expect["frames"]), text)
                XCTAssertEqual(splats, integer(expect["splats"]), text)
            case .error(let code, let errorText):
                XCTAssertEqual(kind, "error", text)
                XCTAssertEqual(code, expect["code"] as? String, text)
                XCTAssertEqual(errorText, expect["message"] as? String, text)
            case .unknown(let type):
                XCTAssertEqual(kind, "unknown", text)
                XCTAssertEqual(type, expect["type"] as? String, text)
            }
        }
    }

    func testServerMessagesByHand() {
        XCTAssertEqual(
            ServerMessage.parse("{\"type\":\"welcome\",\"protocol\":1,\"max_inflight\":4}"),
            .welcome(protocolVersion: 1, maxInflight: 4))
        XCTAssertEqual(ServerMessage.parse("{\"type\":\"ack\",\"frame\":12,\"used\":true}"), .ack(frame: 12, used: true))
        XCTAssertEqual(
            ServerMessage.parse("{\"type\":\"progress\",\"frames\":3,\"splats\":4500}"),
            .progress(frames: 3, splats: 4500))
        XCTAssertEqual(
            ServerMessage.parse("{\"type\":\"error\",\"code\":\"bad_token\",\"message\":\"no\"}"),
            .error(code: "bad_token", message: "no"))
        XCTAssertNil(ServerMessage.parse("nonsense"))
    }
}
