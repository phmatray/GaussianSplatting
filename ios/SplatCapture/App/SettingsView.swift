import SplatCaptureKit
import SwiftUI

/// Pairing with the server and the capture settings.
struct SettingsView: View {
    @EnvironmentObject private var model: AppModel
    @State private var showScanner = false
    @State private var manualText = ""
    @State private var pairingError: String?
    @State private var confirmForget = false

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Text(model.pairingStatus)
                        .font(.headline)
                    if let problem = model.serverProblem {
                        Text(problem)
                            .font(.footnote)
                            .foregroundStyle(.red)
                    }
                    Button("Scanner le QR code du serveur") {
                        pairingError = nil
                        // ARKit and the scanner must never hold the camera together.
                        model.engine.stopPreview()
                        showScanner = true
                    }
                    if model.pairing != nil {
                        Button("Tester la connexion") {
                            model.checkServer()
                        }
                        Button("Oublier ce serveur", role: .destructive) {
                            confirmForget = true
                        }
                    }
                } header: {
                    Text("Serveur")
                } footer: {
                    Text("Sur l'ordinateur, activez l'accès au réseau local dans Splat Live puis affichez le QR code de jumelage. L'iPhone et l'ordinateur doivent être sur le même réseau Wi-Fi.")
                }

                Section {
                    TextField("splatlive://pair?host=...", text: $manualText, axis: .vertical)
                        .font(.footnote.monospaced())
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled(true)
                        .lineLimit(3...6)
                    Button("Jumeler avec cette adresse") {
                        pairingError = model.pair(with: manualText)
                        if pairingError == nil {
                            manualText = ""
                        }
                    }
                    .disabled(manualText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    if let pairingError = pairingError {
                        Text(pairingError)
                            .font(.footnote)
                            .foregroundStyle(.red)
                    }
                } header: {
                    Text("Saisie manuelle")
                } footer: {
                    Text("Collez ici l'adresse de jumelage affichée par le serveur si le QR code ne peut pas être scanné.")
                }

                Section {
                    Picker("Taille de l'image couleur", selection: $model.options.colorWidth) {
                        Text("640 x 480").tag(640)
                        Text("960 x 720").tag(960)
                        Text("1440 x 1080").tag(1440)
                        Text("Pleine résolution (1920 x 1440)").tag(0)
                    }
                    Toggle("Profondeur lissée", isOn: $model.options.smoothDepth)
                    Toggle("Afficher le maillage de la scène", isOn: $model.options.showMesh)
                    Toggle("Masque de personne (modes Personne et Direct)", isOn: $model.options.personMask)
                } header: {
                    Text("Capture")
                } footer: {
                    Text("Une image plus grande donne plus de détails mais demande un réseau plus rapide. La profondeur lissée réduit le bruit sur les scènes immobiles. Le maillage montre ce que le LiDAR a déjà couvert.")
                }

                Section {
                    LabeledContent("Version", value: CaptureEngine.appVersion())
                    LabeledContent("Appareil", value: CaptureEngine.deviceIdentifier())
                    LabeledContent("Capteur LiDAR", value: CaptureEngine.deviceHasLiDAR ? "présent" : "absent")
                } header: {
                    Text("À propos")
                }
            }
            .navigationTitle("Réglages")
            .onAppear {
                model.checkServer()
            }
            .sheet(isPresented: $showScanner) {
                ScannerSheet(onCode: { text in
                    let problem = model.pair(with: text)
                    if problem == nil {
                        showScanner = false
                    }
                    return problem
                })
            }
            .confirmationDialog("Oublier ce serveur ?", isPresented: $confirmForget, titleVisibility: .visible) {
                Button("Oublier", role: .destructive) {
                    model.forgetPairing()
                }
                Button("Annuler", role: .cancel) {
                }
            }
        }
    }
}

/// The QR scanner with a title, a way out and the last error.
struct ScannerSheet: View {
    /// Receives the text of a QR code; returns nil if it was a valid pairing
    /// code, otherwise the French explanation to show.
    private let onCode: (String) -> String?

    @Environment(\.dismiss) private var dismiss
    @State private var message: String?
    @State private var lastRejected = ""
    @State private var accepted = false

    init(onCode: @escaping (String) -> String?) {
        self.onCode = onCode
    }

    var body: some View {
        NavigationStack {
            ZStack(alignment: .bottom) {
                QRScannerView(
                    onCode: { text in
                        // The camera reports the same code many times a second.
                        if accepted || text == lastRejected {
                            return
                        }
                        if let problem = onCode(text) {
                            lastRejected = text
                            message = problem
                        } else {
                            accepted = true
                        }
                    },
                    onError: { problem in
                        message = problem
                    }
                )
                .ignoresSafeArea(edges: .bottom)

                Text(message ?? "Visez le QR code de jumelage affiché par Splat Live sur l'ordinateur.")
                    .font(.footnote)
                    .multilineTextAlignment(.center)
                    .padding(12)
                    .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
                    .padding(16)
            }
            .navigationTitle("Jumelage")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Annuler") {
                        dismiss()
                    }
                }
            }
        }
    }
}
