import SplatCaptureKit
import SwiftUI

/// The .gscap files recorded on the device.
struct RecordingsView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        NavigationStack {
            List {
                if model.recordings.isEmpty {
                    Text("Aucun enregistrement. Dans l'onglet Capture, activez « Enregistrer sans serveur » puis touchez Démarrer.")
                        .foregroundStyle(.secondary)
                } else {
                    ForEach(model.recordings) { recording in
                        NavigationLink {
                            RecordingDetailView(recording: recording)
                        } label: {
                            RecordingRow(recording: recording)
                        }
                    }
                    .onDelete { offsets in
                        let doomed = offsets.map { model.recordings[$0] }
                        for recording in doomed {
                            model.delete(recording)
                        }
                    }
                }
            }
            .navigationTitle("Enregistrements")
            .onAppear {
                model.refreshRecordings()
            }
        }
    }
}

struct RecordingRow: View {
    let recording: Recording

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(recording.name)
                .font(.headline)
            Text(RecordingFormat.summary(recording))
                .font(.footnote)
                .foregroundStyle(.secondary)
        }
    }
}

/// One recording: details, sharing (AirDrop to the Mac), upload to the
/// server, deletion.
struct RecordingDetailView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var confirmDelete = false
    private let recording: Recording

    init(recording: Recording) {
        self.recording = recording
    }

    var body: some View {
        Form {
            Section {
                LabeledContent("Nom", value: recording.name)
                LabeledContent("Mode", value: AppModel.modeLabel(rawValue: recording.mode))
                LabeledContent("Date", value: RecordingFormat.date(recording.created))
                LabeledContent("Taille", value: RecordingFormat.size(recording.size))
                LabeledContent("Images", value: "\(recording.frames)")
                LabeledContent("Durée", value: RecordingFormat.duration(recording.duration))
                LabeledContent("Fichier", value: recording.url.lastPathComponent)
                if !recording.readable {
                    Text("Ce fichier n'est pas un enregistrement lisible.")
                        .foregroundStyle(.red)
                } else if recording.truncated {
                    Text("L'enregistrement a été interrompu : la dernière image est incomplète, les autres sont utilisables.")
                        .font(.footnote)
                        .foregroundStyle(.orange)
                }
            }

            Section {
                ShareLink(item: recording.url) {
                    Label("Partager (AirDrop, Fichiers)", systemImage: "square.and.arrow.up")
                }
                if model.uploadingID == recording.id {
                    VStack(alignment: .leading, spacing: 6) {
                        ProgressView(value: model.uploadProgress)
                        Text("Envoi au serveur : \(Int(model.uploadProgress * 100)) %")
                            .font(.footnote)
                        Button("Annuler l'envoi", role: .cancel) {
                            model.cancelUpload()
                        }
                    }
                } else {
                    Button {
                        model.upload(recording)
                    } label: {
                        Label("Envoyer au serveur", systemImage: "arrow.up.circle")
                    }
                    .disabled(model.uploadingID != nil || !recording.readable)
                }
                if let message = model.uploadMessage {
                    Text(message)
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                }
            } footer: {
                Text(model.pairingStatus)
            }

            Section {
                Button("Supprimer l'enregistrement", role: .destructive) {
                    confirmDelete = true
                }
                .disabled(model.uploadingID == recording.id)
            }
        }
        .navigationTitle(recording.name)
        .navigationBarTitleDisplayMode(.inline)
        .onAppear {
            model.uploadMessage = nil
            model.checkServer()
        }
        .confirmationDialog("Supprimer cet enregistrement ?", isPresented: $confirmDelete, titleVisibility: .visible) {
            Button("Supprimer", role: .destructive) {
                model.delete(recording)
                dismiss()
            }
            Button("Annuler", role: .cancel) {
            }
        }
    }
}

/// French texts for the list and the detail screen.
enum RecordingFormat {
    static func date(_ date: Date?) -> String {
        guard let date = date else {
            return "inconnue"
        }
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "fr_FR")
        formatter.dateStyle = .medium
        formatter.timeStyle = .short
        return formatter.string(from: date)
    }

    static func size(_ bytes: Int64) -> String {
        return ByteCountFormatter.string(fromByteCount: bytes, countStyle: .file)
    }

    static func duration(_ seconds: Double) -> String {
        let whole = Int(seconds.rounded())
        if whole < 60 {
            return "\(whole) s"
        }
        return "\(whole / 60) min \(whole % 60) s"
    }

    static func summary(_ recording: Recording) -> String {
        if !recording.readable {
            return "\(date(recording.created)), \(size(recording.size)), fichier illisible"
        }
        let mode = AppModel.modeLabel(rawValue: recording.mode)
        return "\(mode), \(date(recording.created)), \(size(recording.size)), \(recording.frames) images"
    }
}
