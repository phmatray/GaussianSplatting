import ARKit
import Metal
import SceneKit
import SplatCaptureKit
import SwiftUI
import UIKit


/// The capture screen. The capture's health reads in one glance, as a frieze
/// of one tick per frame that turns green once the server has confirmed it;
/// the numbers sit under it and the shutter under the thumb, on one sheet of
/// glass. A small pill at the top carries the clock and the link.
struct CaptureView: View {
    @EnvironmentObject private var model: AppModel
    @ObservedObject private var engine: CaptureEngine
    /// Compact height: an iPhone held in landscape.
    @Environment(\.verticalSizeClass) private var verticalSizeClass
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var showOptions = false
    @State private var confirmReset = false
    /// Capture clock: time banked by earlier runs, and the current run's start.
    @State private var banked: TimeInterval = 0
    @State private var runStart: Date?
    @State private var frieze = Frieze()

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

    private var landscape: Bool { verticalSizeClass == .compact }
    private var recording: Bool { engine.phase == .running || engine.phase == .paused }
    private var capturing: Bool { engine.phase != .idle }

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
            .ignoresSafeArea()

            if landscape {
                landscapeLayout
            } else {
                portraitLayout
            }
        }
        // Filming owns the screen, as in the Camera app.
        .toolbar(capturing ? .hidden : .visible, for: .tabBar)
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
        .onChange(of: engine.phase) { old, new in
            advanceClock(from: old, to: new)
            if old == .idle {
                frieze = Frieze()
            }
        }
        .onChange(of: engine.stats) { old, new in
            // A reset zeroes the counters: the frieze starts over.
            if new.sent < old.sent || new.recorded < old.recorded || new.skipped < old.skipped {
                frieze = Frieze()
                return
            }
            frieze.advance(
                written: new.recorded - old.recorded,
                sent: new.sent - old.sent,
                dropped: new.skipped - old.skipped,
                acknowledged: new.acknowledged - old.acknowledged)
        }
        .sheet(isPresented: $showOptions) {
            CaptureOptionsSheet()
                .environmentObject(model)
                .presentationDetents([.medium])
        }
        .confirmationDialog("Recommencer la capture ?", isPresented: $confirmReset, titleVisibility: .visible) {
            Button("Recommencer", role: .destructive) {
                engine.reset()
                frieze = Frieze()
                banked = 0
                runStart = engine.phase == .running ? Date() : nil
            }
        } message: {
            Text("Ce qui a été filmé depuis le début est abandonné et la capture repart de zéro.")
        }
    }

    // MARK: Layouts

    private var portraitLayout: some View {
        VStack(spacing: 0) {
            pill
                .padding(.top, 4)
            Spacer(minLength: 12)
            panel
        }
        .padding(.horizontal, 12)
        .padding(.bottom, capturing ? 4 : 8)
    }

    /// Landscape: the pill over the image, the panel on the right third.
    private var landscapeLayout: some View {
        GeometryReader { geometry in
            HStack(alignment: .top, spacing: 12) {
                pill
                    .frame(maxWidth: .infinity, alignment: .topLeading)
                ViewThatFits(in: .vertical) {
                    VStack(spacing: 0) {
                        Spacer(minLength: 0)
                        panel
                    }
                    ScrollView {
                        panel
                    }
                    .scrollIndicators(.hidden)
                }
                .frame(width: geometry.size.width / 3)
            }
        }
        .padding(10)
    }

    // MARK: Pill

    /// Before filming: the pairing state. While filming: recording dot,
    /// clock, mode and the link's dot.
    private var pill: some View {
        HStack(spacing: 8) {
            if capturing {
                Circle()
                    .fill(engine.phase == .running ? Color.red : Color.secondary)
                    .frame(width: 9, height: 9)
                TimelineView(.periodic(from: .now, by: 1)) { context in
                    Text(clockText(at: context.date))
                        .font(.subheadline.weight(.semibold).monospacedDigit())
                }
                .accessibilityLabel("Durée de la capture")
                Text(captureTitle)
                    .font(.subheadline.weight(.semibold))
                    .lineLimit(1)
                Circle()
                    .fill(statusColor)
                    .frame(width: 7, height: 7)
                    .accessibilityLabel(statusLine)
            } else {
                Circle()
                    .fill(statusColor)
                    .frame(width: 8, height: 8)
                Text(statusLine)
                    .font(.subheadline.weight(.semibold))
                    .lineLimit(2)
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 9)
        .panelGlass(cornerRadius: 22)
    }

    private var captureTitle: String {
        let name = model.captureName.isEmpty ? "Capture" : model.captureName
        return "\(AppModel.modeLabel(model.mode)) · \(name)"
    }

    // MARK: Panel

    private var panel: some View {
        VStack(alignment: .leading, spacing: 14) {
            banners
            if capturing {
                friezeBlock
                metricsRow
            }
            if model.mode.usesRegion {
                regionControls
            }
            if !capturing {
                Picker("Mode", selection: $model.mode) {
                    ForEach(CaptureMode.allCases, id: \.self) { mode in
                        // The narrow landscape panel shows symbols; their
                        // names stay for VoiceOver.
                        if landscape {
                            Image(systemName: Self.modeSymbol(mode))
                                .accessibilityLabel(AppModel.modeLabel(mode))
                                .tag(mode)
                        } else {
                            Text(AppModel.modeLabel(mode)).tag(mode)
                        }
                    }
                }
                .pickerStyle(.segmented)
                Text(destinationLine)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .frame(maxWidth: .infinity)
            }
            controlRow
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .panelGlass(cornerRadius: 32)
    }

    @ViewBuilder
    private var banners: some View {
        if !engine.trackingAdvice.isEmpty {
            Label(engine.trackingAdvice, systemImage: "exclamationmark.triangle.fill")
                .font(.footnote.weight(.medium))
                .foregroundStyle(.yellow)
        }
        if let notice = engine.notice {
            HStack(alignment: .top, spacing: 8) {
                Label(notice, systemImage: "info.circle.fill")
                    .font(.footnote)
                    .frame(maxWidth: .infinity, alignment: .leading)
                Button {
                    engine.notice = nil
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .font(.body)
                        .symbolRenderingMode(.hierarchical)
                        .frame(width: 44, height: 44)
                }
                .buttonStyle(.plain)
                .foregroundStyle(.secondary)
                .padding(.vertical, -12)
                .accessibilityLabel("Fermer le message")
            }
        }
    }

    /// One tick per recent frame: green confirmed (or written, offline),
    /// white on its way, orange dropped.
    private var friezeBlock: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .bottom, spacing: 0) {
                ForEach(0..<Frieze.capacity, id: \.self) { slot in
                    let tick = frieze.tick(at: slot)
                    Capsule()
                        .fill(tick.map(Self.tickColor) ?? Color.white.opacity(0.18))
                        .frame(width: 3, height: slot % 10 == 0 ? 18 : 12)
                        .frame(maxWidth: .infinity)
                }
            }
            .frame(height: 18)
            .animation(reduceMotion ? nil : .easeOut(duration: 0.2), value: frieze)
            HStack {
                Text("Les \(Frieze.capacity) dernières images")
                Spacer()
                if engine.stats.skipped > 0 {
                    Text("\(engine.stats.skipped.formatted()) ignorées")
                        .foregroundStyle(.orange)
                }
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(friezeSummary)
    }

    private static func tickColor(_ tick: Frieze.Tick) -> Color {
        switch tick {
        case .confirmed: return .green
        case .inFlight: return .white.opacity(0.9)
        case .dropped: return .orange
        }
    }

    private var friezeSummary: String {
        "Dernières images : \(frieze.count(of: .confirmed)) confirmées, "
            + "\(frieze.count(of: .inFlight)) en route, \(frieze.count(of: .dropped)) ignorées"
    }

    private struct Metric {
        let label: String
        let value: String
        var warn = false
    }

    private var metrics: [Metric] {
        let stats = engine.stats
        if engine.target == .file {
            return [
                Metric(label: "Images", value: stats.recorded.formatted()),
                Metric(label: "Taille", value: megabytes(stats.recordedBytes)),
                Metric(label: "Ignorées", value: stats.skipped.formatted(), warn: stats.skipped > 0),
            ]
        }
        return [
            Metric(label: "Envoyées", value: stats.sent.formatted()),
            Metric(label: "Confirmées", value: stats.acknowledged.formatted()),
            Metric(label: "Ignorées", value: stats.skipped.formatted(), warn: stats.skipped > 0),
            Metric(label: "Splats", value: stats.serverSplats.formatted(.number.notation(.compactName))),
        ]
    }

    private var metricsRow: some View {
        HStack(alignment: .firstTextBaseline, spacing: 0) {
            ForEach(metrics, id: \.label) { metric in
                VStack(alignment: .leading, spacing: 1) {
                    Text(metric.value)
                        .font(.title3.weight(.semibold).monospacedDigit())
                        .foregroundStyle(metric.warn ? .orange : .primary)
                        .lineLimit(1)
                        .minimumScaleFactor(0.7)
                    Text(metric.label)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityElement(children: .combine)
            }
        }
    }

    private func megabytes(_ bytes: Int64) -> String {
        "\(String(format: "%.1f", locale: Locale.current, Double(bytes) / 1_000_000)) Mo"
    }

    private var statusLine: String {
        switch engine.target {
        case .file:
            return "Enregistrement sur l'iPhone"
        case .server:
            switch engine.linkState {
            case .connected:
                return "Connecté à \(model.pairing?.host ?? "serveur")"
            case .connecting:
                return "Connexion au serveur…"
            case .failed:
                return engine.link.isRetrying ? "Serveur injoignable, nouvel essai…" : "Serveur injoignable"
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

    // MARK: Clock

    private func advanceClock(from old: CapturePhase, to new: CapturePhase) {
        switch new {
        case .running:
            if old == .idle {
                banked = 0
            }
            runStart = Date()
        case .paused, .finishing:
            if let start = runStart {
                banked += Date().timeIntervalSince(start)
            }
            runStart = nil
        case .idle:
            banked = 0
            runStart = nil
        }
    }

    private func clockText(at date: Date) -> String {
        let total = Int(banked + (runStart.map { date.timeIntervalSince($0) } ?? 0))
        return String(format: "%d:%02d", total / 60, total % 60)
    }

    // MARK: Controls

    private static func modeSymbol(_ mode: CaptureMode) -> String {
        switch mode {
        case .room: return "house"
        case .object: return "cube"
        case .person: return "figure.stand"
        case .live: return "dot.radiowaves.left.and.right"
        }
    }

    private var destinationLine: String {
        let name = model.captureName.isEmpty ? "Capture" : model.captureName
        let place = model.recordOffline || model.pairing == nil ? "sur l'iPhone" : "vers le Mac"
        return "\(AppModel.modeLabel(model.mode)) · \(name) · \(place)"
    }

    private var controlRow: some View {
        HStack(spacing: 0) {
            leadingControl
                .frame(maxWidth: .infinity)
            shutter
            trailingControl
                .frame(maxWidth: .infinity)
        }
    }

    @ViewBuilder
    private var leadingControl: some View {
        switch engine.phase {
        case .idle:
            roundButton("Options de capture", systemImage: "slider.horizontal.3") {
                showOptions = true
            }
        case .running:
            roundButton("Pause", systemImage: "pause.fill") {
                engine.pause()
            }
        case .paused:
            roundButton("Reprendre", systemImage: "play.fill") {
                engine.resume()
            }
        case .finishing:
            Color.clear.frame(width: 56, height: 56)
        }
    }

    @ViewBuilder
    private var trailingControl: some View {
        if recording {
            roundButton("Recommencer", systemImage: "arrow.counterclockwise", tint: .red) {
                confirmReset = true
            }
        } else {
            Color.clear.frame(width: 56, height: 56)
        }
    }

    private func roundButton(
        _ title: String,
        systemImage: String,
        tint: Color? = nil,
        action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            Label(title, systemImage: systemImage)
                .labelStyle(.iconOnly)
                .font(.title3.weight(.semibold))
                .frame(width: 56, height: 56)
        }
        .roundGlassButton()
        .tint(tint)
    }

    /// The shutter starts and ends a capture. Its ring is the share of sent
    /// frames the server confirmed: green while it keeps up, orange when
    /// frames pile up, red when the link is down.
    private var shutter: some View {
        Button {
            switch engine.phase {
            case .idle:
                engine.start(
                    mode: model.mode,
                    name: model.captureName,
                    pairing: model.pairing,
                    offline: model.recordOffline)
            case .running, .paused:
                engine.finish()
            case .finishing:
                break
            }
        } label: {
            ZStack {
                Circle()
                    .stroke(.white.opacity(0.9), lineWidth: 4)
                if recording, engine.target == .server {
                    Circle()
                        .trim(from: 0, to: confirmedFraction)
                        .stroke(ringColor, style: StrokeStyle(lineWidth: 4, lineCap: .round))
                        .rotationEffect(.degrees(-90))
                }
                RoundedRectangle(cornerRadius: recording ? 8 : 32, style: .continuous)
                    .fill(.red)
                    .frame(width: recording ? 32 : 64, height: recording ? 32 : 64)
                if engine.phase == .finishing {
                    ProgressView()
                        .tint(.white)
                }
            }
            .frame(width: 80, height: 80)
            .contentShape(Circle())
            .animation(reduceMotion ? nil : .snappy(duration: 0.25), value: engine.phase)
        }
        .buttonStyle(.plain)
        .disabled(engine.phase == .finishing)
        .accessibilityLabel(shutterLabel)
    }

    private var shutterLabel: String {
        switch engine.phase {
        case .idle: return "Démarrer la capture"
        case .running, .paused: return "Terminer la capture"
        case .finishing: return "Fin de la capture en cours"
        }
    }

    private var confirmedFraction: CGFloat {
        let stats = engine.stats
        guard stats.sent > 0 else {
            return 0
        }
        return CGFloat(min(1, Double(stats.acknowledged) / Double(stats.sent)))
    }

    private var ringColor: Color {
        guard engine.linkState == .connected else {
            return .red
        }
        return engine.stats.sent - engine.stats.acknowledged > 8 ? .orange : .green
    }

    private var regionControls: some View {
        VStack(alignment: .leading, spacing: 4) {
            if engine.regionCenter == nil {
                Label("Touchez le sujet à l'écran pour placer la zone.", systemImage: "hand.tap")
                    .font(.footnote)
            } else {
                Text("Rayon de la zone : \(String(format: "%.2f", locale: Locale.current, Double(engine.regionRadius))) m")
                    .font(.footnote.monospacedDigit())
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
            .accessibilityLabel("Rayon de la zone")
        }
    }
}


private extension View {
    /// Liquid Glass from iOS 26; the system material before it.
    @ViewBuilder
    func panelGlass(cornerRadius: CGFloat) -> some View {
        let shape = RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
        if #available(iOS 26.0, *) {
            // Glass behind the content, not applied to it: the shutter's red
            // and the state colours stay what they are.
            self.background {
                Color.clear.glassEffect(.regular, in: shape)
            }
        } else {
            self.background(.regularMaterial, in: shape)
        }
    }

    /// A round control of the panel, in glass from iOS 26.
    @ViewBuilder
    func roundGlassButton() -> some View {
        if #available(iOS 26.0, *) {
            self.buttonStyle(.glass).buttonBorderShape(.circle)
        } else {
            self.buttonStyle(.bordered).buttonBorderShape(.circle)
        }
    }
}

/// What is decided once, before filming: the capture's name, and whether it
/// stays on the phone.
struct CaptureOptionsSheet: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            Form {
                Section("Nom") {
                    TextField("Nom de la capture", text: $model.captureName)
                        .submitLabel(.done)
                }
                Section {
                    Toggle("Enregistrer sans serveur", isOn: $model.recordOffline)
                } footer: {
                    Text("La capture est gardée sur l'iPhone, dans l'onglet Enregistrements.")
                }
            }
            .navigationTitle("Options de capture")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("OK") {
                        dismiss()
                    }
                }
            }
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
