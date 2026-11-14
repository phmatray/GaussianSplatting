import Combine
import Foundation
import SplatCaptureKit

/// A .gscap in the Documents directory.
struct Recording: Identifiable, Equatable {
    let url: URL
    var name: String
    var mode: String
    var created: Date?
    var size: Int64
    var frames: Int
    var duration: Double
    var truncated: Bool
    /// False when the file could not be read as a capture.
    var readable: Bool

    var id: String {
        return url.lastPathComponent
    }
}

/// Settings, pairing and the list of recordings. Used from the main thread
/// only (SwiftUI and callbacks that `ServerLink` delivers on the main queue).
final class AppModel: ObservableObject {
    @Published private(set) var pairing: Pairing?
    /// nil while unknown or being tested.
    @Published private(set) var serverReachable: Bool?
    @Published private(set) var serverProblem: String?

    @Published var mode: CaptureMode {
        didSet { defaults.set(mode.rawValue, forKey: Keys.mode) }
    }
    @Published var captureName: String {
        didSet { defaults.set(captureName, forKey: Keys.captureName) }
    }
    @Published var recordOffline: Bool {
        didSet { defaults.set(recordOffline, forKey: Keys.recordOffline) }
    }
    @Published var options: CaptureOptions {
        didSet {
            defaults.set(options.colorWidth, forKey: Keys.colorWidth)
            defaults.set(options.smoothDepth, forKey: Keys.smoothDepth)
            defaults.set(options.showMesh, forKey: Keys.showMesh)
            defaults.set(options.personMask, forKey: Keys.personMask)
        }
    }

    @Published private(set) var recordings: [Recording] = []
    /// `Recording.id` of the file being uploaded.
    @Published private(set) var uploadingID: String?
    @Published private(set) var uploadProgress: Double = 0
    @Published var uploadMessage: String?

    let link: ServerLink
    let engine: CaptureEngine

    private let defaults = UserDefaults.standard
    private let scanQueue = DispatchQueue(label: "be.splatlive.capture.recordings", qos: .utility)

    private enum Keys {
        static let pairingURI = "pairingURI"
        static let mode = "mode"
        static let captureName = "captureName"
        static let recordOffline = "recordOffline"
        static let colorWidth = "colorWidth"
        static let smoothDepth = "smoothDepth"
        static let showMesh = "showMesh"
        static let personMask = "personMask"
    }

    init() {
        let defaults = UserDefaults.standard
        mode = CaptureMode(rawValue: defaults.string(forKey: Keys.mode) ?? "") ?? .room
        captureName = defaults.string(forKey: Keys.captureName) ?? "Capture"
        recordOffline = defaults.bool(forKey: Keys.recordOffline)
        var stored = CaptureOptions()
        if defaults.object(forKey: Keys.colorWidth) != nil {
            let width = defaults.integer(forKey: Keys.colorWidth)
            if ColorGeometry.widthChoices.contains(width) {
                stored.colorWidth = width
            }
        }
        stored.smoothDepth = defaults.bool(forKey: Keys.smoothDepth)
        stored.showMesh = defaults.bool(forKey: Keys.showMesh)
        if defaults.object(forKey: Keys.personMask) != nil {
            stored.personMask = defaults.bool(forKey: Keys.personMask)
        }
        options = stored

        let link = ServerLink()
        self.link = link
        engine = CaptureEngine(link: link)

        if let uri = defaults.string(forKey: Keys.pairingURI), let saved = try? Pairing(uri: uri) {
            pairing = saved
            link.use(pairing: saved)
        }

        link.onUploadProgress = { [weak self] fraction in
            self?.uploadProgress = fraction
        }
        link.onUploadFinished = { [weak self] problem in
            guard let self = self else {
                return
            }
            self.uploadingID = nil
            self.uploadMessage = problem ?? "Fichier envoyé au serveur."
        }
        engine.onRecordingFinished = { [weak self] in
            self?.refreshRecordings()
        }
    }

    // MARK: Pairing

    /// Pairs with the server named by a scanned or typed URI. Returns nil
    /// on success, otherwise a French explanation.
    func pair(with text: String) -> String? {
        let parsed: Pairing
        do {
            parsed = try Pairing(uri: text)
        } catch let error as PairingError {
            return AppModel.explain(error)
        } catch {
            return "Ce code n'est pas un code de jumelage Splat Live."
        }
        defaults.set(parsed.uri, forKey: Keys.pairingURI)
        pairing = parsed
        link.use(pairing: parsed)
        checkServer()
        return nil
    }

    func forgetPairing() {
        defaults.removeObject(forKey: Keys.pairingURI)
        pairing = nil
        serverReachable = nil
        serverProblem = nil
        link.use(pairing: nil)
    }

    /// Tests whether the paired server answers (this is also what makes iOS
    /// ask for the local network permission the first time).
    func checkServer() {
        guard let tested = pairing else {
            serverReachable = nil
            serverProblem = nil
            return
        }
        serverReachable = nil
        serverProblem = nil
        link.probe { [weak self] reachable, problem in
            guard let self = self, self.pairing == tested else {
                return
            }
            self.serverReachable = reachable
            self.serverProblem = reachable ? nil : problem
        }
    }

    /// The status line: "Jumelé avec <host> : joignable".
    var pairingStatus: String {
        guard let pairing = pairing else {
            return "Aucun serveur jumelé"
        }
        switch serverReachable {
        case .some(true):
            return "Jumelé avec \(pairing.host) : joignable"
        case .some(false):
            return "Jumelé avec \(pairing.host) : injoignable"
        case .none:
            return "Jumelé avec \(pairing.host) : vérification en cours"
        }
    }

    static func explain(_ error: PairingError) -> String {
        switch error {
        case .badScheme:
            return "Ce code n'est pas un code de jumelage Splat Live (il doit commencer par splatlive://pair?)."
        case .missingHost, .missingPort, .missingFingerprint, .missingToken, .duplicateParameter, .badEncoding:
            return "Code de jumelage incomplet ou abîmé. Affichez-le de nouveau sur l'ordinateur."
        case .badHost:
            return "L'adresse du serveur dans le code est invalide."
        case .badPort:
            return "Le port du serveur dans le code est invalide."
        case .badFingerprint:
            return "L'empreinte du certificat dans le code est invalide (64 chiffres hexadécimaux attendus)."
        case .badToken:
            return "Le jeton dans le code est invalide (32 chiffres hexadécimaux attendus)."
        }
    }

    // MARK: Recordings

    /// Reads the Documents directory again (off the main thread).
    func refreshRecordings() {
        let directory = CaptureEngine.documentsDirectory
        let busy = engine.recordingURL
        scanQueue.async {
            let found = AppModel.scan(directory: directory, excluding: busy)
            DispatchQueue.main.async {
                self.recordings = found
            }
        }
    }

    private static func scan(directory: URL, excluding busy: URL?) -> [Recording] {
        let files = (try? FileManager.default.contentsOfDirectory(
            at: directory, includingPropertiesForKeys: nil, options: [.skipsHiddenFiles])) ?? []
        let dates = ISO8601DateFormatter()
        var found = [Recording]()
        for url in files where url.pathExtension.lowercased() == GscapFile.fileExtension {
            if let busy = busy, busy.lastPathComponent == url.lastPathComponent {
                continue
            }
            let attributes = try? FileManager.default.attributesOfItem(atPath: url.path)
            let size = (attributes?[.size] as? NSNumber)?.int64Value ?? 0
            let modified = attributes?[.modificationDate] as? Date
            if let summary = try? GscapSummary.read(url: url) {
                found.append(Recording(
                    url: url,
                    name: summary.header.name.isEmpty ? url.deletingPathExtension().lastPathComponent : summary.header.name,
                    mode: summary.header.mode,
                    created: dates.date(from: summary.header.created) ?? modified,
                    size: size,
                    frames: summary.frameCount,
                    duration: summary.duration,
                    truncated: summary.truncated,
                    readable: true))
            } else {
                found.append(Recording(
                    url: url,
                    name: url.deletingPathExtension().lastPathComponent,
                    mode: "",
                    created: modified,
                    size: size,
                    frames: 0,
                    duration: 0,
                    truncated: false,
                    readable: false))
            }
        }
        return found.sorted { ($0.created ?? Date.distantPast) > ($1.created ?? Date.distantPast) }
    }

    func delete(_ recording: Recording) {
        try? FileManager.default.removeItem(at: recording.url)
        recordings.removeAll { $0.id == recording.id }
    }

    /// "Envoyer au serveur": POST to /api/captures.
    func upload(_ recording: Recording) {
        guard uploadingID == nil else {
            return
        }
        guard pairing != nil else {
            uploadMessage = "Aucun serveur jumelé. Jumelez d'abord l'application dans l'onglet Réglages."
            return
        }
        uploadMessage = nil
        uploadProgress = 0
        uploadingID = recording.id
        link.upload(file: recording.url)
    }

    func cancelUpload() {
        link.cancelUpload()
    }

    static func modeLabel(_ mode: CaptureMode) -> String {
        switch mode {
        case .room: return "Pièce"
        case .object: return "Objet"
        case .person: return "Personne"
        case .live: return "Direct"
        }
    }

    static func modeLabel(rawValue: String) -> String {
        guard let mode = CaptureMode(rawValue: rawValue) else {
            return rawValue
        }
        return modeLabel(mode)
    }
}
