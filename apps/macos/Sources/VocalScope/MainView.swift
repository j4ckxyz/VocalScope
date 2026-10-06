import AppKit
import SwiftUI
import UniformTypeIdentifiers
import VocalScopeCore

/// Content of the main window.
struct MainView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        Group {
            if let recording = model.activeRecording, let runtime = model.activeRuntime {
                DocumentView(recording: recording, runtime: runtime)
            } else {
                WelcomeView()
            }
        }
        .frame(minWidth: 760, minHeight: 440)
        .navigationTitle(model.documentName)
        .navigationSubtitle(model.documentSubtitle)
        .modifier(DocumentProxy(url: model.documentURL))
        .toolbar { MainToolbar() }
        .inspector(isPresented: inspectorBinding) {
            if let recording = model.activeRecording {
                InspectorView(recording: recording)
                    .inspectorColumnWidth(min: 250, ideal: 290, max: 400)
            }
        }
        .onDrop(of: [.fileURL], isTargeted: nil) { providers in
            guard let provider = providers.first else { return false }
            _ = provider.loadObject(ofClass: URL.self) { url, _ in
                guard let url else { return }
                Task { @MainActor in model.open(url: url) }
            }
            return true
        }
        .background(WindowReader { model.attach(window: $0) })
        .onAppear { Launch.mark("window", final: !Launch.waitsForDocument) }
    }

    /// The inspector only exists while something is open.
    private var inspectorBinding: Binding<Bool> {
        Binding(
            get: { model.inspectorShown && model.hasProject },
            set: { model.inspectorShown = $0 }
        )
    }
}

/// Shows the document's icon in the title bar (Command-click reveals its
/// location, and it can be dragged), as in any document-based Mac app.
private struct DocumentProxy: ViewModifier {
    let url: URL?

    func body(content: Content) -> some View {
        if let url {
            content.navigationDocument(url)
        } else {
            content
        }
    }
}

/// Gives access to the hosting `NSWindow`.
private struct WindowReader: NSViewRepresentable {
    let onWindow: (NSWindow) -> Void

    final class Probe: NSView {
        var onWindow: ((NSWindow) -> Void)?
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            if let window { onWindow?(window) }
        }
    }

    func makeNSView(context: Context) -> Probe {
        let probe = Probe()
        probe.onWindow = onWindow
        return probe
    }

    func updateNSView(_ probe: Probe, context: Context) {}
}

// MARK: - Toolbar

private struct MainToolbar: ToolbarContent {
    @EnvironmentObject private var model: AppModel

    var body: some ToolbarContent {
        // Transport and time together in the centre, like a player's display.
        ToolbarItem(placement: .principal) {
            if model.hasProject {
                HStack(spacing: 10) {
                    Button { model.seek(to: 0) } label: {
                        Label("Return to Start", systemImage: "backward.end.fill")
                    }
                    .help("Return to start (Return)")

                    Button { model.togglePlayback() } label: {
                        Label(
                            model.playback.state == .playing ? "Pause" : "Play",
                            systemImage: model.playback.state == .playing ? "pause.fill" : "play.fill")
                            .frame(width: 16)
                    }
                    .help(model.playback.state == .playing ? "Pause (Space)" : "Play (Space)")

                    Button { model.stop() } label: {
                        Label("Stop", systemImage: "stop.fill")
                    }
                    .help("Stop")

                    Divider().frame(height: 16)
                    TimeReadout()
                }
                .labelStyle(.iconOnly)
                .buttonStyle(.borderless)
                .disabled(!model.playback.hasTrack)
                .padding(.horizontal, 8)
            }
        }

        // Nothing to zoom, hear or inspect until something is open.
        if model.hasProject {
            documentControls
        }
    }

    @ToolbarContentBuilder
    private var documentControls: some ToolbarContent {
        ToolbarItemGroup(placement: .primaryAction) {
            if model.session.comparison != nil {
                RecordingSwitch()
            }

            Toggle(isOn: $model.pitchShown) {
                Label("Pitch", systemImage: "waveform.path.ecg")
            }
            .help(model.pitchShown ? "Hide the pitch curve (⌥⌘P)" : "Show the pitch curve (⌥⌘P)")

            ControlGroup {
                Button { model.timeline?.zoom(by: 1 / 1.6) } label: {
                    Label("Zoom Out", systemImage: "minus.magnifyingglass")
                }
                .help("Zoom out (⌘−)")
                Button { model.timeline?.zoom(by: 1.6) } label: {
                    Label("Zoom In", systemImage: "plus.magnifyingglass")
                }
                .help("Zoom in (⌘+)")
                Button { model.timeline?.zoomToFit() } label: {
                    Label("Zoom to Fit", systemImage: "arrow.left.and.right")
                }
                .help("Zoom to fit (⌘0)")
            }

            VolumeControl()

            Button { model.inspectorShown.toggle() } label: {
                Label("Inspector", systemImage: "sidebar.trailing")
            }
            .help(model.inspectorShown ? "Hide inspector (⌥⌘I)" : "Show inspector (⌥⌘I)")
        }
    }
}

/// Current position and total length. Refreshes itself while playing, so
/// playback never invalidates the rest of the window.
private struct TimeReadout: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        let playing = model.playback.state == .playing
        SwiftUI.TimelineView(.animation(minimumInterval: 1.0 / 20, paused: !playing)) { _ in
            let status = model.core.playbackStatus()
            HStack(alignment: .firstTextBaseline, spacing: 5) {
                Text(Format.time(status.positionSeconds, decimals: 3))
                    .fontWeight(.medium)
                Text("/ \(Format.time(status.durationSeconds ?? model.duration, decimals: 3))")
                    .foregroundStyle(.secondary)
            }
            .monospacedDigit()
            .frame(minWidth: 170)
        }
        .accessibilityLabel("Playback position")
    }
}

/// Chooses which of two compared recordings is shown and heard.
private struct RecordingSwitch: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        Picker("Recording", selection: Binding(
            get: { model.session.activeRecordingId ?? "" },
            set: { model.setActiveRecording($0) })
        ) {
            ForEach(model.session.recordings, id: \.recordingId) { runtime in
                Text(model.letter(for: runtime.recordingId))
                    .tag(runtime.recordingId)
                    .help(runtime.displayTitle)
            }
        }
        .pickerStyle(.segmented)
        .help("Switch between the two recordings at the matching moment (X)")
    }
}

private struct VolumeControl: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        HStack(spacing: 4) {
            Button { model.toggleMute() } label: {
                Label(
                    model.playback.muted ? "Unmute" : "Mute",
                    systemImage: model.playback.muted || model.playback.volume == 0
                        ? "speaker.slash.fill" : "speaker.wave.2.fill")
            }
            .help(model.playback.muted ? "Unmute (M)" : "Mute (M)")

            Slider(
                value: Binding(
                    get: { Double(model.playback.volume) },
                    set: { model.setVolume(Float($0)) }),
                in: 0...1
            )
            .controlSize(.small)
            .frame(width: 84)
            .opacity(model.playback.muted ? 0.5 : 1)
            .help("Volume")
            .accessibilityLabel("Volume")
        }
    }
}

// MARK: - Document

private struct DocumentView: View {
    @EnvironmentObject private var model: AppModel
    let recording: Recording
    let runtime: RecordingRuntime

    var body: some View {
        if runtime.sourceExists {
            WaveformTimeline(
                recordingId: recording.id,
                duration: model.duration,
                ready: runtime.waveformStatus == .ready,
                pitch: model.pitchLayer,
                playback: model.playback
            )
            .overlay { statusOverlay }
            .overlay(alignment: .bottomTrailing) { analysisBadge }
            .onChange(of: runtime.waveformStatus, initial: true) { _, status in
                if status == .ready { Launch.mark("waveform") }
            }
            .onChange(of: runtime.analysisStatus, initial: true) { _, status in
                if status == .ready || status == .failed { Launch.mark("analysis", final: true) }
            }
        } else {
            ContentUnavailableView {
                Label("Audio File Not Found", systemImage: "questionmark.folder")
            } description: {
                Text("“\(recording.audio.fileName)” is not where it was when this project was saved. Your labels and notes are intact.")
                Text(recording.source.path)
                    .font(.caption)
                    .textSelection(.enabled)
            } actions: {
                Button("Locate File…") { model.locateActiveRecording() }
                    .buttonStyle(.borderedProminent)
            }
        }
    }

    /// A quiet note while background work that changes the timeline runs.
    @ViewBuilder
    private var analysisBadge: some View {
        let analysing = runtime.waveformStatus == .ready && runtime.analysisStatus == .pending && model.pitchShown
        let isolating = model.isolationIsRunning && model.isolation.recordingId == recording.id
        if analysing || isolating {
            HStack(spacing: 6) {
                ProgressView().controlSize(.small)
                Text(isolating ? model.isolation.stage.label : "Analysing pitch…")
                    .font(.callout)
                if isolating, let fraction = model.isolation.fraction {
                    Text(Format.percent(fraction))
                        .font(.callout)
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
            .background(.regularMaterial, in: Capsule())
            .padding(10)
            .accessibilityElement(children: .combine)
        }
    }

    @ViewBuilder
    private var statusOverlay: some View {
        switch runtime.waveformStatus {
        case .ready:
            EmptyView()
        case .failed:
            ContentUnavailableView {
                Label(runtime.waveformError?.title ?? "The audio could not be read", systemImage: "exclamationmark.triangle")
            } description: {
                Text(runtime.waveformError?.message ?? "")
                if let suggestion = runtime.waveformError?.suggestion { Text(suggestion) }
            }
        case .pending, .unavailable:
            VStack(spacing: 8) {
                if let progress = model.waveformProgress {
                    ProgressView(value: Double(progress))
                        .frame(width: 200)
                } else {
                    ProgressView()
                        .controlSize(.small)
                }
                Text("Reading audio…")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            .padding(.top, 60)
        }
    }
}

// MARK: - Welcome

private struct WelcomeView: View {
    @EnvironmentObject private var model: AppModel
    @State private var selection: RecentItem.ID?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .center, spacing: 16) {
                Image(nsImage: NSApplication.shared.applicationIconImage)
                    .resizable()
                    .frame(width: 64, height: 64)
                VStack(alignment: .leading, spacing: 4) {
                    Text("VocalScope")
                        .font(.title.weight(.semibold))
                    Text("Open a vocal recording or a full song to inspect it. Everything is processed on this Mac; nothing is uploaded.")
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }

            HStack(spacing: 10) {
                Button("Open Audio…") { model.openAudioPanel() }
                    .buttonStyle(.borderedProminent)
                Button("Open Project…") { model.openProjectPanel() }
                Text("or drop a file anywhere in this window")
                    .font(.callout)
                    .foregroundStyle(.tertiary)
            }
            .controlSize(.large)
            .padding(.top, 20)

            if !model.recents.isEmpty {
                Text("Recent")
                    .font(.headline)
                    .padding(.top, 28)
                    .padding(.bottom, 6)
                List(model.recents, selection: $selection) { item in
                    RecentRow(item: item)
                }
                .listStyle(.inset)
                // Sized to its rows, so a short list is not padded with empty ones.
                .frame(height: min(CGFloat(model.recents.count) * 52 + 14, 340))
                .clipShape(RoundedRectangle(cornerRadius: 8))
                .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(.separator))
                .contextMenu(forSelectionType: RecentItem.ID.self) { ids in
                    if let item = model.recents.first(where: { ids.contains($0.id) }) {
                        Button("Open") { model.open(recent: item) }
                        Button("Show in Finder") {
                            NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: item.path)])
                        }
                        .disabled(!FileManager.default.fileExists(atPath: item.path))
                        Divider()
                        Button("Remove from Recents") { model.removeRecent(item) }
                    }
                } primaryAction: { ids in
                    if let item = model.recents.first(where: { ids.contains($0.id) }) {
                        model.open(recent: item)
                    }
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 40)
        .padding(.top, 36)
        .padding(.bottom, 24)
        .frame(maxWidth: 680, maxHeight: .infinity, alignment: .topLeading)
        .frame(maxWidth: .infinity)
    }
}

private struct RecentRow: View {
    let item: RecentItem

    var body: some View {
        let exists = FileManager.default.fileExists(atPath: item.path)
        HStack(spacing: 10) {
            Image(systemName: item.kind == .project ? "doc.text" : "waveform")
                .foregroundStyle(.secondary)
                .frame(width: 20)
            VStack(alignment: .leading, spacing: 1) {
                Text(item.title)
                    .lineLimit(1)
                    .foregroundStyle(exists ? .primary : .secondary)
                Text(([item.detail, item.fileName].compactMap { $0 }).joined(separator: " · "))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer()
            if !exists {
                Text("Missing")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
            if let duration = item.durationSeconds {
                Text(Format.time(duration))
                    .font(.caption)
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
            }
            Text(item.lastOpenedAt, format: .dateTime.year().month(.abbreviated).day())
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .padding(.vertical, 2)
        .help(item.path)
    }
}
