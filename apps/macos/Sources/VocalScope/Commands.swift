import SwiftUI
import VocalScopeCore

/// The menu bar. Standard items (About, Settings, Edit, Window, Quit) come
/// from the system; these add the document, playback and view commands.
struct AppCommands: Commands {
    @ObservedObject var model: AppModel

    var body: some Commands {
        CommandGroup(replacing: .newItem) {
            Button("Open Audio…") { model.openAudioPanel() }
                .keyboardShortcut("o")
            Button("Open Project…") { model.openProjectPanel() }
                .keyboardShortcut("o", modifiers: [.command, .shift])
            Menu("Open Recent") {
                ForEach(model.recents) { item in
                    Button(recentTitle(item)) { model.open(recent: item) }
                }
                if !model.recents.isEmpty { Divider() }
                Button("Clear Menu") { model.clearRecents() }
                    .disabled(model.recents.isEmpty)
            }
        }

        CommandGroup(replacing: .saveItem) {
            Button("Save Project") { model.saveProject() }
                .keyboardShortcut("s")
                .disabled(!model.hasProject)
            Button("Save Project As…") { model.saveProjectAs() }
                .keyboardShortcut("s", modifiers: [.command, .shift])
                .disabled(!model.hasProject)
            Divider()
            Button("Close Project") { model.closeProject() }
                .keyboardShortcut("w", modifiers: [.command, .shift])
                .disabled(!model.hasProject)
        }

        CommandMenu("Playback") {
            Group {
                Button(model.playback.state == .playing ? "Pause" : "Play") { model.togglePlayback() }
                Button("Stop") { model.stop() }
                Divider()
                Button("Return to Start") { model.seek(to: 0) }
                Button("Skip Back 5 Seconds") { model.skip(by: -AppModel.skipSeconds) }
                Button("Skip Forward 5 Seconds") { model.skip(by: AppModel.skipSeconds) }
                Divider()
                Button(model.playback.muted ? "Unmute" : "Mute") { model.toggleMute() }
            }
            .disabled(!model.hasProject)
        }

        CommandGroup(after: .toolbar) {
            Button("Zoom In") { model.timeline?.zoom(by: 1.6) }
                .keyboardShortcut("+")
                .disabled(!model.hasProject)
            Button("Zoom Out") { model.timeline?.zoom(by: 1 / 1.6) }
                .keyboardShortcut("-")
                .disabled(!model.hasProject)
            Button("Zoom to Fit") { model.timeline?.zoomToFit() }
                .keyboardShortcut("0")
                .disabled(!model.hasProject)
            Divider()
            Button(model.inspectorShown ? "Hide Inspector" : "Show Inspector") {
                model.inspectorShown.toggle()
            }
            .keyboardShortcut("i", modifiers: [.command, .option])
            .disabled(!model.hasProject)
            Divider()
        }
    }

    private func recentTitle(_ item: RecentItem) -> String {
        var title = item.title
        if let detail = item.detail { title += " — \(detail)" }
        if !FileManager.default.fileExists(atPath: item.path) { title += " (missing)" }
        return title
    }
}
