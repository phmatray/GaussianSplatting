import ARKit
import Combine
import CoreVideo
import Foundation
import SplatCaptureKit
import UIKit

/// Capture settings chosen in the Réglages tab.
struct CaptureOptions: Equatable {
    /// Width of the colour image as sent: 640, 960, 1440, or 0 for the full sensor image.
    var colorWidth = 960
    /// Use ARKit's temporally smoothed depth instead of the raw depth.
    var smoothDepth = false
    /// Show the mesh ARKit reconstructs, to see what has been covered.
    var showMesh = false
    /// Send ARKit's person mask in the Personne and Direct modes.
    var personMask = true
}

struct CaptureStats: Equatable {
    /// Frames handed to the WebSocket.
    var sent = 0
    /// Frames the server acknowledged, and how many of those it used.
    var acknowledged = 0
    var usedByServer = 0
    /// Frames worth sending that were dropped (flow control, slow network).
    var skipped = 0
    /// Last `progress` message of the server.
    var serverFrames = 0
    var serverSplats = 0
    /// Offline recording.
    var recorded = 0
    var recordedBytes: Int64 = 0
}

enum CapturePhase: Equatable {
    case idle
    case running
    case paused
    /// "Terminer" was pressed; waiting for the last frames and acks.
    case finishing
}

enum CaptureTarget: Equatable {
    case none
    case server
    case file
}

/// The ARKit session and what happens to its frames.
///
/// Threading: ARKit calls the delegate on the main thread and `ServerLink`
/// reports on the main thread, so all state here belongs to the main thread.
/// Only JPEG encoding, compression and file writes run elsewhere, on one
/// serial queue; they receive copies and report back on the main thread.
final class CaptureEngine: NSObject, ObservableObject, ARSessionDelegate {
    /// LiDAR scanner present (and ARKit available).
    static var deviceHasLiDAR: Bool {
        return ARWorldTrackingConfiguration.isSupported
            && ARWorldTrackingConfiguration.supportsFrameSemantics(.sceneDepth)
    }

    let session = ARSession()
    let link: ServerLink

    @Published private(set) var phase: CapturePhase = .idle
    @Published private(set) var target: CaptureTarget = .none
    @Published private(set) var linkState: LinkState = .disconnected
    @Published private(set) var stats = CaptureStats()
    /// 0 not available, 1 limited, 2 normal (the byte of the frame header).
    @Published private(set) var trackingCode: UInt8 = 0
    @Published private(set) var trackingAdvice = ""
    /// One line for the user: an error or what just happened.
    @Published var notice: String?
    @Published private(set) var regionCenter: SIMD3<Float>?
    @Published var regionRadius: Float = 0.6
    @Published private(set) var floorY: Float?

    /// Called when a recording file was closed (the list must be refreshed).
    var onRecordingFinished: (() -> Void)?
    /// The file being written, if any (hidden from the list of recordings).
    private(set) var recordingURL: URL?

    private var options = CaptureOptions()
    private var mode: CaptureMode = .room
    private var captureName = ""
    private var previewRunning = false
    private var selector = KeyframeSelector(mode: .room)
    private var flow = FlowControl()
    private var nextIndex: UInt32 = 0
    private var orientation: UInt8 = DisplayOrientation.portrait
    private var everWelcomed = false
    private var finishSent = false
    private var pendingEncodes = 0
    /// Grows at every "Réinitialiser": a frame captured before it carries a
    /// pose in the old world origin and must not reach the server or the file.
    private var generation = 0
    /// Camera frames to ignore after a reset (ARKit may already have queued
    /// a few with the old world origin).
    private var framesToDrop = 0
    private var floorPlanes: [UUID: Float] = [:]
    private var writer: GscapWriter?
    private var fileHeader: GscapHeader?

    private let encodeQueue = DispatchQueue(label: "be.splatlive.capture.encode", qos: .userInitiated)
    private let encoder = FrameEncoder()

    init(link: ServerLink) {
        self.link = link
        super.init()
        session.delegate = self
        link.onState = { [weak self] state in
            self?.linkStateChanged(state)
        }
        link.onMessage = { [weak self] message in
            self?.handle(message)
        }
    }

    // MARK: Preview (the AR session runs while the capture tab is visible)

    func startPreview(options: CaptureOptions, mode: CaptureMode) {
        self.options = options
        if phase == .idle {
            self.mode = mode
        }
        previewRunning = true
        applyConfiguration(reset: false)
    }

    func stopPreview() {
        guard previewRunning else {
            return
        }
        if phase == .running {
            pause()
        }
        previewRunning = false
        session.pause()
    }

    /// Settings or mode changed while the preview is on.
    func update(options: CaptureOptions, mode: CaptureMode) {
        self.options = options
        if phase == .idle {
            self.mode = mode
        }
        applyConfiguration(reset: false)
    }

    private func applyConfiguration(reset: Bool) {
        guard previewRunning, CaptureEngine.deviceHasLiDAR else {
            return
        }
        let configuration = ARWorldTrackingConfiguration()
        configuration.planeDetection = [.horizontal]
        var semantics: ARConfiguration.FrameSemantics = [.sceneDepth]
        if options.smoothDepth, ARWorldTrackingConfiguration.supportsFrameSemantics(.smoothedSceneDepth) {
            semantics.insert(.smoothedSceneDepth)
        }
        if options.personMask, mode == .person || mode == .live {
            // Only when the device can do both at once; otherwise no mask.
            var withMask = semantics
            withMask.insert(.personSegmentationWithDepth)
            if ARWorldTrackingConfiguration.supportsFrameSemantics(withMask) {
                semantics = withMask
            }
        }
        configuration.frameSemantics = semantics
        if options.showMesh, ARWorldTrackingConfiguration.supportsSceneReconstruction(.mesh) {
            configuration.sceneReconstruction = .mesh
        }
        if reset {
            session.run(configuration, options: [.resetTracking, .removeExistingAnchors, .resetSceneReconstruction])
        } else {
            session.run(configuration)
        }
    }

    // MARK: Start, pause, reset, finish

    /// Starts a capture: to the paired server, or to a file when `offline`
    /// is set or no server is paired. If the server cannot be reached the
    /// capture falls back to a file by itself.
    func start(mode: CaptureMode, name: String, pairing: Pairing?, offline: Bool) {
        guard phase == .idle, CaptureEngine.deviceHasLiDAR else {
            return
        }
        self.mode = mode
        captureName = name
        selector = KeyframeSelector(mode: mode)
        stats = CaptureStats()
        notice = nil
        everWelcomed = false
        finishSent = false
        flow.disconnect()
        applyConfiguration(reset: false)

        if let pairing = pairing, !offline {
            target = .server
            phase = .running
            link.connect(hello: ClientMessage.hello(
                token: pairing.token,
                device: CaptureEngine.deviceIdentifier(),
                app: CaptureEngine.appVersion(),
                mode: mode,
                name: name))
        } else if startFile() {
            phase = .running
            if pairing == nil && !offline {
                notice = "Aucun serveur jumelé : la capture est enregistrée sur l'appareil (onglet Enregistrements)."
            }
        }
    }

    func pause() {
        guard phase == .running else {
            return
        }
        phase = .paused
        if target == .server, flow.isOpen {
            link.send(text: ClientMessage.pause)
        }
    }

    func resume() {
        guard phase == .paused else {
            return
        }
        phase = .running
        if target == .server, flow.isOpen {
            link.send(text: ClientMessage.resume)
        }
    }

    /// Throws away what was captured so far and starts again from here.
    func reset() {
        guard phase == .running || phase == .paused else {
            return
        }
        stats = CaptureStats()
        selector.reset()
        generation += 1
        framesToDrop = 3
        regionCenter = nil
        floorY = nil
        floorPlanes.removeAll()
        if target == .server, flow.isOpen {
            link.send(text: ClientMessage.reset)
        }
        if target == .file {
            discardFile()
            if !startFile() {
                phase = .idle
                target = .none
                return
            }
        }
        // A new world origin: the server (or the new file) starts clean too.
        applyConfiguration(reset: true)
    }

    func finish() {
        guard phase == .running || phase == .paused else {
            return
        }
        phase = .finishing
        // Runs after the frames still being encoded have been delivered.
        encodeQueue.async {
            DispatchQueue.main.async {
                self.completeFinish()
            }
        }
    }

    private func completeFinish() {
        guard phase == .finishing else {
            return
        }
        switch target {
        case .server:
            if flow.isOpen {
                link.send(text: ClientMessage.finish)
                finishSent = true
                if flow.inflight.isEmpty {
                    closeServerCapture()
                } else {
                    // Wait for the last acks, but not for ever.
                    DispatchQueue.main.asyncAfter(deadline: .now() + 4) {
                        if self.phase == .finishing {
                            self.closeServerCapture()
                        }
                    }
                }
            } else {
                closeServerCapture()
            }
        case .file:
            closeFile(announce: true)
        case .none:
            phase = .idle
        }
    }

    private func closeServerCapture() {
        let summary = "Capture terminée : \(stats.acknowledged) images reçues par le serveur."
        phase = .idle
        target = .none
        finishSent = false
        flow.disconnect()
        // Leaves the server a moment to send its last progress report, and
        // does not reconnect when the server closes first.
        link.disconnectSoon()
        notice = summary
    }

    // MARK: Region of interest and floor

    /// Places the region of interest (modes Objet and Personne).
    func setRegion(center: SIMD3<Float>) {
        guard center.x.isFinite, center.y.isFinite, center.z.isFinite else {
            return
        }
        regionCenter = center
        sendRegion()
    }

    /// Sends the region again (after the radius slider moved).
    func sendRegion() {
        updateFileHeader()
        guard let center = regionCenter, mode.usesRegion, target == .server, flow.isOpen else {
            return
        }
        link.send(text: ClientMessage.region(center: [center.x, center.y, center.z], radius: regionRadius))
    }

    private func sendFloor() {
        guard let y = floorY, target == .server, flow.isOpen else {
            return
        }
        link.send(text: ClientMessage.floor(y: y))
    }

    private func noteFloor(_ anchors: [ARAnchor]) {
        var changed = false
        for anchor in anchors {
            guard let plane = anchor as? ARPlaneAnchor, plane.alignment == .horizontal else {
                continue
            }
            if case .floor = plane.classification {
                floorPlanes[plane.identifier] = plane.transform.columns.3.y
                changed = true
            } else if floorPlanes.removeValue(forKey: plane.identifier) != nil {
                changed = true
            }
        }
        guard changed, let lowest = floorPlanes.values.min() else {
            return
        }
        // The lowest plane ARKit calls a floor; resend when it moves by 2 cm.
        if let current = floorY, abs(current - lowest) < 0.02 {
            return
        }
        floorY = lowest
        sendFloor()
        updateFileHeader()
    }

    // MARK: Server messages

    private func linkStateChanged(_ state: LinkState) {
        linkState = state
        switch state {
        case .connected, .connecting:
            break
        case .disconnected:
            flow.disconnect()
        case .failed(let reason):
            flow.disconnect()
            guard target == .server, phase == .running || phase == .paused || phase == .finishing else {
                return
            }
            if phase == .finishing {
                closeServerCapture()
                notice = "Connexion perdue avant la fin : \(reason)"
            } else if !everWelcomed {
                // Never connected for this capture: record to a file instead.
                link.disconnect()
                if startFile() {
                    notice = "Serveur injoignable, enregistrement sur l'appareil. \(reason)"
                } else {
                    phase = .idle
                    target = .none
                }
            } else if link.isRetrying {
                notice = "\(reason) Nouvelle tentative en cours."
            } else {
                // Nothing more can reach the server (it restarted with a new
                // token, for example): stop filming for nothing.
                if phase == .running {
                    phase = .paused
                }
                notice = "\(reason) Capture en pause : touchez Terminer."
            }
        }
    }

    private func handle(_ message: ServerMessage) {
        switch message {
        case .welcome(_, let maxInflight):
            flow.welcome(maxInflight: maxInflight)
            if everWelcomed {
                notice = "Connexion rétablie."
            }
            everWelcomed = true
            // A new connection knows nothing of what was said before.
            sendRegion()
            sendFloor()
            if phase == .paused {
                link.send(text: ClientMessage.pause)
            }
        case .ack(let frame, let used):
            if flow.acknowledge(frame) {
                stats.acknowledged += 1
                if used {
                    stats.usedByServer += 1
                }
            }
            if phase == .finishing, finishSent, flow.inflight.isEmpty {
                closeServerCapture()
            }
        case .progress(let frames, let splats):
            stats.serverFrames = frames
            stats.serverSplats = splats
        case .error(_, let detail):
            if !detail.isEmpty {
                notice = "Serveur : \(detail)"
            }
        case .unknown:
            break
        }
    }

    // MARK: Offline recording

    private func startFile() -> Bool {
        let header = GscapHeader(
            device: CaptureEngine.deviceIdentifier(),
            app: CaptureEngine.appVersion(),
            mode: mode.rawValue,
            name: captureName,
            created: GscapHeader.timestampNow())
        let url = CaptureEngine.newRecordingURL(name: captureName)
        do {
            writer = try GscapWriter(url: url, header: header)
        } catch {
            writer = nil
            notice = "Impossible de créer le fichier d'enregistrement. Le stockage est-il plein ?"
            return false
        }
        fileHeader = header
        recordingURL = url
        target = .file
        // A region placed or a floor found before the start goes in at once.
        updateFileHeader()
        return true
    }

    /// Writes region and floor into the header of the file being recorded as
    /// soon as they are known, so that a recording cut short (application
    /// killed, battery empty) still carries them.
    private func updateFileHeader() {
        guard target == .file, let writer = writer, var header = fileHeader else {
            return
        }
        if let center = regionCenter, mode.usesRegion {
            header.regionCenter = [center.x, center.y, center.z]
            header.regionRadius = regionRadius
        }
        header.floorY = floorY
        if header == fileHeader {
            return
        }
        fileHeader = header
        encodeQueue.async {
            do {
                try writer.rewriteHeader(header)
            } catch {
                // Not fatal: the header is written again when the recording ends.
            }
        }
    }

    /// Writes the final header (region and floor are known by now) and closes.
    private func closeFile(announce: Bool) {
        guard let writer = writer, var header = fileHeader else {
            phase = .idle
            target = .none
            return
        }
        if let center = regionCenter, mode.usesRegion {
            header.regionCenter = [center.x, center.y, center.z]
            header.regionRadius = regionRadius
        }
        header.floorY = floorY
        let finalHeader = header
        self.writer = nil
        fileHeader = nil
        let frames = stats.recorded
        encodeQueue.async {
            try? writer.rewriteHeader(finalHeader)
            try? writer.close()
            DispatchQueue.main.async {
                self.recordingURL = nil
                self.phase = .idle
                self.target = .none
                if announce {
                    self.notice = "Enregistrement terminé : \(frames) images. Il est dans l'onglet Enregistrements."
                }
                self.onRecordingFinished?()
            }
        }
    }

    private func discardFile() {
        guard let writer = writer, let url = recordingURL else {
            return
        }
        self.writer = nil
        fileHeader = nil
        recordingURL = nil
        encodeQueue.async {
            try? writer.close()
            try? FileManager.default.removeItem(at: url)
        }
    }

    // MARK: ARSessionDelegate

    func session(_ session: ARSession, didUpdate frame: ARFrame) {
        updateTracking(frame.camera)
        guard phase == .running else {
            return
        }
        if framesToDrop > 0 {
            framesToDrop -= 1
            return
        }
        if target == .server && !flow.isOpen {
            return  // not welcomed yet, or reconnecting
        }
        if trackingCode == CaptureFrame.trackingNotAvailable && mode != .live {
            return  // a pose that means nothing must not be fused
        }
        let pose = CaptureEngine.columnMajor(frame.camera.transform)
        guard pose.allSatisfy({ $0.isFinite }) else {
            return
        }
        let time = frame.timestamp
        guard selector.wants(pose: pose, time: time) else {
            return
        }
        let depthData: ARDepthData?
        if options.smoothDepth, let smoothed = frame.smoothedSceneDepth {
            depthData = smoothed
        } else {
            depthData = frame.sceneDepth
        }
        guard let sceneDepth = depthData else {
            return  // the first frames have no depth yet
        }

        // Flow control: a frame that cannot go out now is skipped, never queued.
        if pendingEncodes >= 2 || (target == .server && !flow.canSend) {
            stats.skipped += 1
            selector.markSkipped(time: time)
            return
        }

        // Copy what is needed; the ARFrame itself must not outlive this call.
        guard let image = FrameEncoder.copy(frame.capturedImage),
              let depth = FrameEncoder.copyPlane(sceneDepth.depthMap, format: kCVPixelFormatType_DepthFloat32) else {
            return
        }
        var confidence: PlaneCopy?
        if let map = sceneDepth.confidenceMap {
            confidence = FrameEncoder.copyPlane(map, format: kCVPixelFormatType_OneComponent8)
        }
        var mask: PlaneCopy?
        if let segmentation = frame.segmentationBuffer {
            mask = FrameEncoder.copyPlane(segmentation, format: kCVPixelFormatType_OneComponent8)
        }

        let index = nextIndex
        nextIndex &+= 1
        if target == .server {
            _ = flow.reserve(index)
        }
        selector.markSent(pose: pose, time: time)
        orientation = DisplayOrientation.code(worldFromCam: pose, previous: orientation)

        let intrinsics = frame.camera.intrinsics
        let resolution = frame.camera.imageResolution
        let raw = RawCapture(
            index: index,
            timestamp: time,
            pose: pose,
            geometry: ColorGeometry(
                width: Int(resolution.width),
                height: Int(resolution.height),
                fx: intrinsics.columns.0.x,
                fy: intrinsics.columns.1.y,
                cx: intrinsics.columns.2.x,
                cy: intrinsics.columns.2.y),
            tracking: trackingCode,
            orientation: orientation,
            image: image,
            depth: depth,
            confidence: confidence,
            mask: mask)

        pendingEncodes += 1
        let destination = target
        let targetWidth = options.colorWidth
        let fileWriter = writer
        let frameGeneration = generation
        let encoder = self.encoder
        encodeQueue.async {
            let data = encoder.encode(raw, targetWidth: targetWidth)
            let written: Bool
            if let data = data, destination == .file, let fileWriter = fileWriter {
                written = (try? fileWriter.append(data)) != nil
            } else {
                written = false
            }
            DispatchQueue.main.async {
                self.encoded(
                    index: index, data: data, destination: destination, written: written,
                    fileWriter: fileWriter, frameGeneration: frameGeneration)
            }
        }
    }

    /// Back on the main thread with the bytes of frame `index`.
    /// `fileWriter` is the file the frame was meant for, if any.
    private func encoded(
        index: UInt32, data: Data?, destination: CaptureTarget, written: Bool,
        fileWriter: GscapWriter?, frameGeneration: Int
    ) {
        pendingEncodes -= 1
        if frameGeneration != generation {
            // Captured before "Réinitialiser": dropped.
            flow.cancel(index)
            return
        }
        switch destination {
        case .server:
            // After a reconnection the frame no longer has a place in the window.
            guard flow.holds(index) else {
                stats.skipped += 1
                return
            }
            guard let data = data else {
                flow.cancel(index)
                stats.skipped += 1
                return
            }
            link.send(frame: data)
            stats.sent += 1
        case .file:
            // A frame for a file that was discarded meanwhile (Réinitialiser)
            // is neither counted nor reported.
            guard let current = writer, current === fileWriter else {
                return
            }
            if let data = data, written {
                stats.recorded += 1
                stats.recordedBytes += Int64(data.count + 4)
            } else if data != nil {
                notice = "Écriture impossible : le stockage est sans doute plein. Capture en pause, touchez Terminer."
                if phase == .running {
                    phase = .paused
                }
            }
        case .none:
            break
        }
    }

    func session(_ session: ARSession, didAdd anchors: [ARAnchor]) {
        noteFloor(anchors)
    }

    func session(_ session: ARSession, didUpdate anchors: [ARAnchor]) {
        noteFloor(anchors)
    }

    func session(_ session: ARSession, didRemove anchors: [ARAnchor]) {
        for anchor in anchors {
            floorPlanes.removeValue(forKey: anchor.identifier)
        }
    }

    func session(_ session: ARSession, didFailWithError error: Error) {
        if let arError = error as? ARError, arError.code == .cameraUnauthorized {
            notice = "L'accès à la caméra est refusé. Autorisez-le dans Réglages > SplatCapture > Appareil photo."
        } else {
            notice = "La session ARKit s'est arrêtée : \(error.localizedDescription)"
        }
        if phase == .running {
            pause()
        }
    }

    func sessionWasInterrupted(_ session: ARSession) {
        if phase == .running {
            pause()
            notice = "Capture en pause : l'application est passée à l'arrière-plan."
        }
    }

    func sessionInterruptionEnded(_ session: ARSession) {
        if phase == .paused {
            notice = "Suivi interrompu. Revenez vers une zone déjà filmée, puis touchez Reprendre."
        }
    }

    private func updateTracking(_ camera: ARCamera) {
        let code: UInt8
        let advice: String
        switch camera.trackingState {
        case .normal:
            code = CaptureFrame.trackingNormal
            advice = ""
        case .notAvailable:
            code = CaptureFrame.trackingNotAvailable
            advice = "Suivi indisponible. Patientez un instant."
        case .limited(let reason):
            code = CaptureFrame.trackingLimited
            switch reason {
            case .initializing:
                advice = "Initialisation : bougez doucement l'appareil."
            case .excessiveMotion:
                advice = "Bougez plus lentement."
            case .insufficientFeatures:
                advice = "Pas assez de détails ou de lumière. Visez une zone plus texturée, mieux éclairée."
            case .relocalizing:
                advice = "Reprise du suivi : revenez vers une zone déjà filmée."
            @unknown default:
                advice = "Suivi limité."
            }
        @unknown default:
            code = CaptureFrame.trackingLimited
            advice = "Suivi limité."
        }
        // Published values are only touched when they change (60 calls per second).
        if code != trackingCode {
            trackingCode = code
        }
        if advice != trackingAdvice {
            trackingAdvice = advice
        }
    }

    // MARK: Helpers

    /// `simd_float4x4` to 16 floats, column after column: the layout of
    /// `world_from_cam` in the frame header. No conversion is needed, ARKit's
    /// world (+Y up) and camera (-Z forward) are those of the server.
    static func columnMajor(_ m: simd_float4x4) -> [Float] {
        let c0 = m.columns.0
        let c1 = m.columns.1
        let c2 = m.columns.2
        let c3 = m.columns.3
        return [
            c0.x, c0.y, c0.z, c0.w,
            c1.x, c1.y, c1.z, c1.w,
            c2.x, c2.y, c2.z, c2.w,
            c3.x, c3.y, c3.z, c3.w,
        ]
    }

    /// The hardware model, for example "iPhone17,2".
    static func deviceIdentifier() -> String {
        var size = 0
        sysctlbyname("hw.machine", nil, &size, nil, 0)
        guard size > 0 else {
            return "unknown"
        }
        var machine = [CChar](repeating: 0, count: size + 1)
        sysctlbyname("hw.machine", &machine, &size, nil, 0)
        // The bytes up to the terminating zero.
        let bytes = machine.prefix(while: { $0 != 0 }).map { UInt8(bitPattern: $0) }
        return bytes.isEmpty ? "unknown" : String(decoding: bytes, as: UTF8.self)
    }

    static func appVersion() -> String {
        return (Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String) ?? "0.1.0"
    }

    static var documentsDirectory: URL {
        return FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
    }

    /// Documents/<name>-<date>.gscap, with a name that is safe in a file name.
    static func newRecordingURL(name: String) -> URL {
        let allowed = CharacterSet.alphanumerics.union(CharacterSet(charactersIn: " -_"))
        var safe = ""
        for scalar in name.unicodeScalars {
            if allowed.contains(scalar) {
                safe.unicodeScalars.append(scalar)
            } else {
                safe.append("_")
            }
        }
        safe = safe.trimmingCharacters(in: .whitespaces)
        if safe.isEmpty {
            safe = "Capture"
        }
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.dateFormat = "yyyyMMdd-HHmmss"
        let stamp = formatter.string(from: Date())
        return documentsDirectory.appendingPathComponent("\(safe.prefix(40))-\(stamp).\(GscapFile.fileExtension)")
    }
}
