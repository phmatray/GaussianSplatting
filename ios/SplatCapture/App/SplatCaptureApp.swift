import SwiftUI

@main
struct SplatCaptureApp: App {
    @StateObject private var model = AppModel()

    var body: some Scene {
        WindowGroup {
            TabView {
                CaptureView(engine: model.engine)
                    .tabItem {
                        Label("Capture", systemImage: "camera.viewfinder")
                    }
                RecordingsView()
                    .tabItem {
                        Label("Enregistrements", systemImage: "tray.full")
                    }
                SettingsView()
                    .tabItem {
                        Label("Réglages", systemImage: "gearshape")
                    }
            }
            .environmentObject(model)
        }
    }
}
