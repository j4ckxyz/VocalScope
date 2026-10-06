import SwiftUI
import VocalScopeCore

/// The menu bar. Standard items (About, Settings, Edit, Window, Quit) come
/// from the system; these add the document, analysis, playback and view
/// commands.
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
            Menu("Export") {
                Group {
                    Button("Report…") { model.export(.report) }
                        .keyboardShortcut("e")
                    Button("Pitch Curve as CSV…") { model.export(.pitchCsv) }
                    Button("Notes as CSV…") { model.export(.notesCsv) }
                    Button("Notes as MIDI…") { model.export(.midi) }
                    Button("Everything as JSON…") { model.export(.json) }
                }
                .disabled(model.analysis == nil)
                Divider()
                Button("Isolated Vocals as WAV…") { model.exportVocals() }
                    .disabled(model.activeRuntime?.vocalStem == nil)
            }
            .disabled(!model.hasProject)
            Divider()
            Button("Close Project") { model.closeProject() }
                .keyboardShortcut("w", modifiers: [.command, .shift])
                .disabled(!model.hasProject)
        }

        CommandMenu("Analysis") {
            Group {
                Toggle("Show Pitch on the Timeline", isOn: $model.pitchShown)
                    .keyboardShortcut("p", modifiers: [.command, .option])
                Button("Analyse Again") { model.reanalyse() }
                    .disabled(model.activeRuntime?.waveformStatus != .ready)
                Divider()
                Button("Isolate Vocals…") {
                    model.inspectorTab = .analysis
                    model.inspectorShown = true
                }
                .disabled(model.isolationIsRunning)
                Toggle("Listen to the Isolated Vocals", isOn: Binding(
                    get: { model.session.listeningToVocals },
                    set: { model.setListeningToVocals($0) }))
                    .keyboardShortcut("l", modifiers: [.command, .option])
                    .disabled(!model.session.recordings.contains { $0.vocalStem != nil })
                Divider()
                Button("Add Recording to Compare…") { model.addRecordingPanel() }
                    .disabled((model.session.project?.recordings.count ?? 0) != 1)
                Button("Switch to the Other Recording") { model.switchRecording() }
                    .keyboardShortcut("x", modifiers: [.command, .option])
                    .disabled(model.session.comparison == nil)
            }
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
