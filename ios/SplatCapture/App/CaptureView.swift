import ARKit
import Metal
import SceneKit
import SplatCaptureKit
import SwiftUI
import UIKit

/// The capture screen: camera preview, mode, controls and counters.
struct CaptureView: View {
    @EnvironmentObject private var model: AppModel
    @ObservedObject private var engine: CaptureEngine

    init(engine: CaptureEngine) {
        _engine = ObservedObject(wrappedValue: engine)
    }

    var body: some View {
        if CaptureEngine.deviceHasLiDAR {
            captureScreen
        } else {
            NoLiDARView()
        }
    }

    private var captureScreen: some View {
        ZStack {
            ARPreview(
                engine: engine,
                showMesh: model.options.showMesh,
                regionCenter: model.mode.usesRegion ? engine.regionCenter : nil,
                regionRadius: engine.regionRadius,
                onTap: { point in
                    if model.mode.usesRegion {
                        engine.setRegion(center: point)
                    }
                }
            )
            .ignoresSafeArea(edges: .top)

            VStack(spacing: 8) {
                statusPanel
                Spacer()
                if let notice = engine.notice {
                    noticePanel(notice)
                }
                controlPanel
            }
            .padding(10)
        }
        .onAppear {
            UIApplication.shared.isIdleTimerDisabled = true
            engine.startPreview(options: model.options, mode: model.mode)
            model.checkServer()
        }
        .onDisappear {
            UIApplication.shared.isIdleTimerDisabled = false
            engine.stopPreview()
        }
        .onChange(of: model.options) { _, newValue in
            engine.update(options: newValue, mode: model.mode)
        }
        .onChange(of: model.mode) { _, newValue in
            engine.regionRadius = newValue == .person ? 1.2 : 0.6
            engine.update(options: model.options, mode: newValue)
        }
    }

    // MARK: Status and counters

    private var statusPanel: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Circle()
                    .fill(statusColor)
                    .frame(width: 10, height: 10)
                Text(statusLine)
                    .font(.footnote.weight(.semibold))
                    .lineLimit(2)
            }
            if !engine.trackingAdvice.isEmpty {
                Text(engine.trackingAdvice)
                    .font(.footnote)
                    .foregroundStyle(.yellow)
            }
            if engine.phase != .idle {
                Text(counterLine)
                    .font(.caption.monospacedDigit())
                if engine.target == .server {
                    Text("Serveur : \(engine.stats.serverFrames) images, \(engine.stats.serverSplats) splats")
                        .font(.caption.monospacedDigit())
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(10)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
    }

    private var statusLine: String {
        switch engine.target {
        case .file:
            return "Enregistrement sur l'appareil"
        case .server:
            switch engine.linkState {
            case .connected:
                return "Connecté au serveur \(model.pairing?.host ?? "")"
            case .connecting:
                return "Connexion au serveur en cours"
            case .failed:
                return engine.link.isRetrying ? "Serveur injoignable, nouvelle tentative" : "Serveur injoignable"
            case .disconnected:
                return "Déconnecté du serveur"
            }
        case .none:
            return model.pairingStatus
        }
    }

    private var statusColor: Color {
        switch engine.target {
        case .file:
            return .orange
        case .server:
            return engine.linkState == .connected ? .green : .red
        case .none:
            if model.pairing == nil {
                return .gray
            }
            return model.serverReachable == true ? .green : (model.serverReachable == false ? .red : .gray)
        }
    }

    private var counterLine: String {
        let stats = engine.stats
        if engine.target == .file {
            let megabytes = String(format: "%.1f", locale: Locale.current, Double(stats.recordedBytes) / 1_000_000)
            return "Images enregistrées : \(stats.recorded) (\(megabytes) Mo), ignorées : \(stats.skipped)"
        }
        return "Images envoyées : \(stats.sent), confirmées : \(stats.acknowledged), ignorées : \(stats.skipped)"
    }

    private func noticePanel(_ text: String) -> some View {
        HStack(alignment: .top) {
            Text(text)
                .font(.footnote)
            Spacer()
            Button {
                engine.notice = nil
            } label: {
                Image(systemName: "xmark.circle.fill")
            }
            .accessibilityLabel("Fermer le message")
        }
        .padding(10)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
    }

    // MARK: Controls

    private var controlPanel: some View {
        VStack(spacing: 10) {
            if engine.phase == .idle {
                Picker("Mode", selection: $model.mode) {
                    ForEach(CaptureMode.allCases, id: \.self) { mode in
                        Text(AppModel.modeLabel(mode)).tag(mode)
                    }
                }
                .pickerStyle(.segmented)

                TextField("Nom de la capture", text: $model.captureName)
                    .textFieldStyle(.roundedBorder)
                    .submitLabel(.done)

                Toggle("Enregistrer sans serveur", isOn: $model.recordOffline)
                    .font(.footnote)
            }

            if model.mode.usesRegion {
                regionControls
            }

            HStack(spacing: 12) {
                switch engine.phase {
                case .idle:
                    Button("Démarrer") {
                        engine.start(
                            mode: model.mode,
                            name: model.captureName,
                            pairing: model.pairing,
                            offline: model.recordOffline)
                    }
                    .buttonStyle(.borderedProminent)
                case .running:
                    Button("Pause") {
                        engine.pause()
                    }
                    .buttonStyle(.bordered)
                    finishAndResetButtons
                case .paused:
                    Button("Reprendre") {
                        engine.resume()
                    }
                    .buttonStyle(.borderedProminent)
                    finishAndResetButtons
                case .finishing:
                    ProgressView()
                    Text("Fin de la capture")
                        .font(.footnote)
                }
            }
            .frame(maxWidth: .infinity)
        }
        .padding(10)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
    }

    @ViewBuilder
    private var finishAndResetButtons: some View {
        Button("Terminer") {
            engine.finish()
        }
        .buttonStyle(.bordered)
        Button("Réinitialiser", role: .destructive) {
            engine.reset()
        }
        .buttonStyle(.bordered)
    }

    private var regionControls: some View {
        VStack(alignment: .leading, spacing: 2) {
            if engine.regionCenter == nil {
                Text("Touchez le sujet à l'écran pour placer la zone à capturer.")
                    .font(.footnote)
            } else {
                Text("Rayon de la zone : \(String(format: "%.2f", locale: Locale.current, Double(engine.regionRadius))) m")
                    .font(.footnote)
            }
            Slider(
                value: $engine.regionRadius,
                in: 0.15...3.0,
                onEditingChanged: { editing in
                    if !editing {
                        engine.sendRegion()
                    }
                }
            )
        }
    }
}

/// Shown instead of the camera on a device without LiDAR scanner.
struct NoLiDARView: View {
    var body: some View {
        VStack(spacing: 16) {
            Image(systemName: "exclamationmark.triangle")
                .font(.system(size: 48))
                .foregroundStyle(.secondary)
            Text("Capteur LiDAR introuvable")
                .font(.title3.weight(.semibold))
            Text("La capture a besoin du scanner LiDAR des iPhone Pro (à partir de l'iPhone 12 Pro) et des iPad Pro (à partir de 2020). Cet appareil n'en a pas. Les onglets Enregistrements et Réglages restent utilisables.")
                .font(.body)
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
        }
        .padding(32)
    }
}

/// ARKit's camera view, with the optional mesh overlay and the marker of the
/// region of interest.
struct ARPreview: UIViewRepresentable {
    let engine: CaptureEngine
    var showMesh: Bool
    var regionCenter: SIMD3<Float>?
    var regionRadius: Float
    var onTap: (SIMD3<Float>) -> Void

    func makeCoordinator() -> Coordinator {
        return Coordinator()
    }

    func makeUIView(context: Context) -> ARSCNView {
        let view = ARSCNView(frame: .zero)
        view.session = engine.session
        // The engine stays the session's delegate; the view only renders.
        engine.session.delegate = engine
        view.delegate = context.coordinator
        view.automaticallyUpdatesLighting = false
        let tap = UITapGestureRecognizer(target: context.coordinator, action: #selector(Coordinator.handleTap(_:)))
        view.addGestureRecognizer(tap)
        return view
    }

    func updateUIView(_ view: ARSCNView, context: Context) {
        let coordinator = context.coordinator
        coordinator.onTap = onTap
        coordinator.showMesh = showMesh
        let hidden = !showMesh
        view.scene.rootNode.enumerateChildNodes { node, _ in
            if node.name == Coordinator.meshNodeName {
                node.isHidden = hidden
            }
        }
        coordinator.updateRegion(in: view, center: regionCenter, radius: regionRadius)
    }

    final class Coordinator: NSObject, ARSCNViewDelegate {
        static let meshNodeName = "scene-mesh"

        var onTap: ((SIMD3<Float>) -> Void)?
        var showMesh = false
        private var regionNode: SCNNode?

        /// Tap: where the ray through that pixel meets the scene.
        @MainActor @objc func handleTap(_ gesture: UITapGestureRecognizer) {
            guard let view = gesture.view as? ARSCNView else {
                return
            }
            let point = gesture.location(in: view)
            guard let query = view.raycastQuery(from: point, allowing: .estimatedPlane, alignment: .any),
                  let hit = view.session.raycast(query).first else {
                return
            }
            let position = hit.worldTransform.columns.3
            onTap?(SIMD3<Float>(position.x, position.y, position.z))
        }

        /// A translucent sphere where the region of interest is.
        @MainActor func updateRegion(in view: ARSCNView, center: SIMD3<Float>?, radius: Float) {
            guard let center = center else {
                regionNode?.removeFromParentNode()
                regionNode = nil
                return
            }
            let node: SCNNode
            if let existing = regionNode {
                node = existing
            } else {
                let sphere = SCNSphere(radius: CGFloat(radius))
                sphere.segmentCount = 32
                let material = SCNMaterial()
                material.diffuse.contents = UIColor.systemYellow
                material.transparency = 0.22
                material.lightingModel = .constant
                material.isDoubleSided = true
                material.writesToDepthBuffer = false
                sphere.materials = [material]
                node = SCNNode(geometry: sphere)
                view.scene.rootNode.addChildNode(node)
                regionNode = node
            }
            (node.geometry as? SCNSphere)?.radius = CGFloat(radius)
            node.simdPosition = center
        }

        // MARK: Mesh overlay (ARSCNViewDelegate, SceneKit's render thread)

        func renderer(_ renderer: SCNSceneRenderer, nodeFor anchor: ARAnchor) -> SCNNode? {
            guard let meshAnchor = anchor as? ARMeshAnchor else {
                return nil
            }
            let node = SCNNode(geometry: Coordinator.geometry(of: meshAnchor.geometry))
            node.name = Coordinator.meshNodeName
            node.isHidden = !showMesh
            return node
        }

        func renderer(_ renderer: SCNSceneRenderer, didUpdate node: SCNNode, for anchor: ARAnchor) {
            guard let meshAnchor = anchor as? ARMeshAnchor else {
                return
            }
            node.geometry = Coordinator.geometry(of: meshAnchor.geometry)
        }

        /// ARKit's reconstructed mesh as a wireframe.
        private static func geometry(of mesh: ARMeshGeometry) -> SCNGeometry {
            let vertices = mesh.vertices
            let source = SCNGeometrySource(
                buffer: vertices.buffer,
                vertexFormat: vertices.format,
                semantic: .vertex,
                vertexCount: vertices.count,
                dataOffset: vertices.offset,
                dataStride: vertices.stride)
            let faces = mesh.faces
            let indexData = Data(
                bytes: faces.buffer.contents(),
                count: faces.count * faces.indexCountPerPrimitive * faces.bytesPerIndex)
            let element = SCNGeometryElement(
                data: indexData,
                primitiveType: .triangles,
                primitiveCount: faces.count,
                bytesPerIndex: faces.bytesPerIndex)
            let geometry = SCNGeometry(sources: [source], elements: [element])
            let material = SCNMaterial()
            material.diffuse.contents = UIColor.systemTeal
            material.lightingModel = .constant
            material.fillMode = .lines
            material.isDoubleSided = true
            geometry.materials = [material]
            return geometry
        }
    }
}
