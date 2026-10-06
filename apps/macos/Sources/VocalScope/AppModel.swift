import AppKit
import SwiftUI
import UniformTypeIdentifiers
import VocalScopeCore

/// Forwards core callbacks, which arrive on arbitrary threads, to the model
/// on the main thread.
private final class ObserverBridge: AppObserver, @unchecked Sendable {
    weak var model: AppModel?

    func sessionChanged(session: SessionView) {
        Task { @MainActor [weak self] in self?.model?.apply(session: session) }
    }

    func playbackChanged(status: PlaybackStatus) {
        Task { @MainActor [weak self] in self?.model?.apply(playback: status) }
    }

    func waveformProgress(recordingId: Uuid, fraction: Float?) {
        Task { @MainActor [weak self] in
            self?.model?.apply(waveformProgress: fraction, recordingId: recordingId)
        }
    }

    func analysisProgress(recordingId: Uuid, fraction: Float?) {
        Task { @MainActor [weak self] in
            self?.model?.apply(analysisProgress: fraction, recordingId: recordingId)
        }
    }

    func isolationChanged(status: IsolationStatus) {
        Task { @MainActor [weak self] in self?.model?.apply(isolation: status) }
    }

    func recentsChanged(recents: [RecentItem]) {
        Task { @MainActor [weak self] in self?.model?.recents = recents }
    }

    func settingsChanged(settings: CoreSettings) {
        Task { @MainActor [weak self] in self?.model?.apply(settings: settings) }
    }
}

/// The app's state and actions. Views observe it; menu commands, toolbar
/// buttons, keyboard shortcuts and drag-and-drop all call the same methods,
/// so each flow (panel, unsaved-changes prompt, error alert) exists once.
///
/// All real work happens in the shared core; this class only adapts it to
/// AppKit and SwiftUI.
@MainActor
final class AppModel: ObservableObject {
    static let shared = AppModel()

    let core: AppCore
    private let bridge = ObserverBridge()
    /// Transport commands run here so a slow audio device never stalls the UI.
    private let playbackQueue = DispatchQueue(label: "org.vocalscope.playback", qos: .userInitiated)

    @Published private(set) var session: SessionView
    @Published private(set) var playback: PlaybackStatus
    @Published fileprivate(set) var recents: [RecentItem]
    @Published private(set) var settings: CoreSettings
    @Published private(set) var waveformProgress: Float?
    /// The finished pitch analysis of the active recording.
    @Published private(set) var analysis: AnalysisView?
    @Published private(set) var analysisProgress: Float?
    @Published private(set) var isolation: IsolationStatus
    @Published private(set) var models: [SeparationModel]
    @Published var inspectorShown: Bool {
        didSet { UserDefaults.standard.set(inspectorShown, forKey: "inspectorShown") }
    }
    @Published var inspectorTab: InspectorTab {
        didSet { UserDefaults.standard.set(inspectorTab.rawValue, forKey: "inspectorTab") }
    }
    /// Draw the pitch curve and notes over the timeline.
    @Published var pitchShown: Bool {
        didSet { UserDefaults.standard.set(pitchShown, forKey: "pitchShown") }
    }
    /// What the analysis on screen was made from; when this changes the
    /// analysis is fetched again.
    private var analysisKey = ""

    /// The timeline view registers itself so zoom commands can reach it
    /// without routing every scroll event through SwiftUI state.
    weak var timeline: TimelineCanvas?
    private weak var window: NSWindow?
    private let closeGuard = WindowCloseGuard()
    private var keyMonitor: Any?
    private let seekSlot = SeekSlot()

    static let skipSeconds = 5.0

    private init() {
        let files = FileManager.default
        func directory(_ base: FileManager.SearchPathDirectory, _ name: String) -> String {
            let root = files.urls(for: base, in: .userDomainMask).first
                ?? URL(fileURLWithPath: NSTemporaryDirectory())
            return root.appendingPathComponent(name, isDirectory: true).path
        }
        let identifier = Bundle.main.bundleIdentifier ?? "org.vocalscope.VocalScope"
        let config: AppConfig
        if let root = ProcessInfo.processInfo.environment["VOCALSCOPE_DATA_ROOT"], !root.isEmpty {
            // Benchmarks and tests run against a throwaway location so they
            // never touch the user's settings or recent files.
            config = AppConfig(
                dataDirectory: root + "/data", cacheDirectory: root + "/cache", logDirectory: root + "/logs")
        } else {
            config = AppConfig(
                dataDirectory: directory(.applicationSupportDirectory, identifier),
                cacheDirectory: directory(.cachesDirectory, identifier),
                logDirectory: directory(.libraryDirectory, "Logs/VocalScope")
            )
        }
        core = AppCore(config: config, observer: bridge)
        session = core.session()
        playback = core.playbackStatus()
        recents = core.recents()
        settings = core.settings()
        isolation = core.isolationStatus()
        models = core.separationModels()
        inspectorShown = UserDefaults.standard.object(forKey: "inspectorShown") as? Bool ?? true
        inspectorTab = InspectorTab(rawValue: UserDefaults.standard.string(forKey: "inspectorTab") ?? "") ?? .details
        pitchShown = UserDefaults.standard.object(forKey: "pitchShown") as? Bool ?? true
        bridge.model = self
        applyAppearance()
        installKeyMonitor()

        // `VocalScope song.flac` in a terminal: open it before the first
        // frame, so the window appears with the recording already in it.
        if let argument = CommandLine.arguments.dropFirst().first(where: {
            !$0.hasPrefix("-") && files.fileExists(atPath: $0)
        }) {
            let url = URL(fileURLWithPath: argument).standardizedFileURL
            launchDocument = url
            open(url: url)
        }
    }

    /// The file named on the command line, if one was opened at launch.
    private(set) var launchDocument: URL?

    // MARK: - Derived state

    var hasProject: Bool { session.project != nil }

    var activeRecording: Recording? {
        session.project?.recordings.first { $0.id == session.activeRecordingId }
    }

    var activeRuntime: RecordingRuntime? {
        session.recordings.first { $0.recordingId == session.activeRecordingId }
    }

    /// Best known duration of the active recording, in seconds.
    var duration: Double {
        activeRecording?.waveform?.durationSeconds ?? activeRecording?.audio.durationSeconds ?? 0
    }

    var documentName: String {
        if let file = session.projectFileName {
            return (file as NSString).deletingPathExtension
        }
        return activeRuntime?.displayTitle ?? session.project?.name ?? "VocalScope"
    }

    /// Format facts for the title bar, e.g. "MP3 · 44.1 kHz · Stereo".
    var documentSubtitle: String {
        guard let audio = activeRecording?.audio else { return "" }
        var parts = [audio.codec.uppercased(), Format.sampleRate(audio.sampleRateHz), Format.channels(audio.channelCount)]
        if let depth = audio.bitDepth { parts.append("\(depth)-bit") }
        if let detail = activeRuntime?.displayDetail { parts.append(detail) }
        return parts.joined(separator: " · ")
    }

    /// The recording being compared with the active one, if there is one.
    var otherRecording: Recording? {
        guard let comparison = session.comparison, let active = session.activeRecordingId else { return nil }
        let other = comparison.referenceRecordingId == active
            ? comparison.otherRecordingId : comparison.referenceRecordingId
        return session.project?.recordings.first { $0.id == other }
    }

    /// "A" for the first recording of a compared pair, "B" for the second.
    func letter(for recordingId: Uuid) -> String {
        session.comparison?.otherRecordingId == recordingId ? "B" : "A"
    }

    /// What the timeline should draw about pitch, or `nil` when the pitch
    /// is hidden or not ready.
    var pitchLayer: PitchLayer? {
        guard pitchShown, let analysis, let active = session.activeRecordingId,
              analysis.recordingId == active
        else { return nil }
        var differences: [ClosedRange<Double>] = []
        var compared = false
        var comparisonKey = ""
        if let comparison = session.comparison, let alignment = comparison.alignment {
            let isReference = comparison.referenceRecordingId == active
            let otherId = isReference ? comparison.otherRecordingId : comparison.referenceRecordingId
            let other = session.recordings.first { $0.recordingId == otherId }
            compared = other?.analysisStatus == .ready
            if let pitch = comparison.pitch {
                // Regions are on the reference's timeline; move them onto
                // the other's when that is the one on screen.
                let place: (Double) -> Double = isReference
                    ? { $0 } : { alignment.offsetSeconds + alignment.speedRatio * $0 }
                differences = pitch.regions.map { place($0.startSeconds)...place($0.endSeconds) }
            }
            comparisonKey = "\(otherId)|\(alignment.offsetSeconds)|\(alignment.speedRatio)|\(compared)|\(other?.analysedIsolatedVocals ?? false)|\(comparison.pitch?.regions.count ?? -1)|\(comparison.pitch?.comparedSeconds ?? -1)"
        }
        return PitchLayer(
            notes: analysis.notes,
            low: Double(analysis.displayLowMidi),
            high: Double(analysis.displayHighMidi),
            differences: differences,
            compared: compared,
            key: "\(analysisKey)#\(analysis.notes.count)#\(comparisonKey)")
    }

    var isolationIsRunning: Bool {
        isolation.stage != .idle && isolation.stage != .failed
    }

    /// The file the title bar's proxy icon stands for.
    var documentURL: URL? {
        if let path = session.projectPath { return URL(fileURLWithPath: path) }
        if let path = activeRecording?.source.path, activeRuntime?.sourceExists == true {
            return URL(fileURLWithPath: path)
        }
        return nil
    }

    // MARK: - Applying core state

    fileprivate func apply(session: SessionView) {
        self.session = session
        if activeRuntime?.waveformStatus != .pending { waveformProgress = nil }
        if activeRuntime?.analysisStatus != .pending { analysisProgress = nil }
        window?.isDocumentEdited = session.dirty
        refreshAnalysis()
    }

    /// Fetches the active recording's analysis when a different one has
    /// become available (or none is any more).
    private func refreshAnalysis() {
        guard let runtime = activeRuntime, runtime.analysisStatus == .ready,
              let recording = activeRecording
        else {
            analysisKey = ""
            if analysis != nil { analysis = nil }
            return
        }
        // The source kind is part of the key because it decides the caution.
        let key = "\(runtime.recordingId)|\(runtime.analysedIsolatedVocals)|\(recording.source.kind)|\(recording.source.path)|\(runtime.vocalStem?.modelId ?? "")"
        guard key != analysisKey || analysis == nil else { return }
        analysisKey = key
        analysis = core.analysis(recordingId: runtime.recordingId)
    }

    fileprivate func apply(analysisProgress fraction: Float?, recordingId: Uuid) {
        if recordingId == session.activeRecordingId, activeRuntime?.analysisStatus == .pending {
            analysisProgress = fraction
        }
    }

    fileprivate func apply(isolation status: IsolationStatus) {
        let finished = isolationIsRunning && (status.stage == .idle || status.stage == .failed)
        isolation = status
        // A finished job may have installed a model.
        if finished { models = core.separationModels() }
    }

    fileprivate func apply(playback: PlaybackStatus) {
        self.playback = playback
    }

    fileprivate func apply(waveformProgress fraction: Float?, recordingId: Uuid) {
        if recordingId == session.activeRecordingId { waveformProgress = fraction }
    }

    fileprivate func apply(settings: CoreSettings) {
        self.settings = settings
        applyAppearance()
    }

    private func applyAppearance() {
        let appearance: NSAppearance?
        switch settings.general.theme {
        case .system: appearance = nil
        case .light: appearance = NSAppearance(named: .aqua)
        case .dark: appearance = NSAppearance(named: .darkAqua)
        }
        NSApplication.shared.appearance = appearance
    }

    // MARK: - Window integration

    func attach(window: NSWindow) {
        guard self.window !== window else { return }
        self.window = window
        window.isDocumentEdited = session.dirty
        // Ask about unsaved changes before the window closes, while keeping
        // SwiftUI's own window delegate in charge of everything else.
        closeGuard.original = window.delegate
        closeGuard.shouldClose = { [weak self] in self?.confirmDiscardChanges() ?? true }
        window.delegate = closeGuard
    }

    /// Single-key shortcuts. These are handled here rather than as menu key
    /// equivalents so they can never swallow a keystroke meant for a text
    /// field.
    private func installKeyMonitor() {
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self, self.handleKey(event) else { return event }
            return nil
        }
    }

    private func handleKey(_ event: NSEvent) -> Bool {
        guard hasProject, let window, event.window === window, window.attachedSheet == nil else { return false }
        if window.firstResponder is NSText { return false }
        let modifiers = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        guard modifiers.isDisjoint(with: [.command, .control, .option]) else { return false }
        let step = modifiers.contains(.shift) ? 1.0 : Self.skipSeconds

        switch event.keyCode {
        case 7 where session.comparison != nil: switchRecording()   // X
        case 49: togglePlayback()                       // Space
        case 36, 115: seek(to: 0)                       // Return, Home
        case 119: seek(to: duration)                    // End
        case 123: skip(by: -step)                       // Left arrow
        case 124: skip(by: step)                        // Right arrow
        case 126: setVolume(playback.volume + 0.05)     // Up arrow
        case 125: setVolume(playback.volume - 0.05)     // Down arrow
        case 46: toggleMute()                           // M
        default: return false
        }
        return true
    }

    // MARK: - Opening and saving

    /// Runs a core call that touches the disk off the main thread.
    private func inBackground<T: Sendable>(
        _ work: @escaping @Sendable (AppCore) throws -> T,
        then completion: @escaping @MainActor (T) -> Void = { _ in }
    ) {
        let core = self.core
        Task.detached(priority: .userInitiated) {
            do {
                let result = try work(core)
                await MainActor.run { completion(result) }
            } catch {
                await MainActor.run { Dialogs.present(error: error) }
            }
        }
    }

    /// Opens a file. Local files open in about a millisecond, so this waits
    /// briefly for the result and shows it in the same frame; anything
    /// slower (a network drive, a sleeping disk) completes in the background
    /// without blocking the window.
    func open(url: URL) {
        guard confirmDiscardChanges() else { return }
        let path = url.path
        let core = self.core
        let attempt = OpenAttempt()
        DispatchQueue.global(qos: .userInitiated).async {
            let result = Result { try core.openPath(path: path) }
            if attempt.complete(result) {
                Task { @MainActor in AppModel.shared.finishOpen(result, url: url) }
            }
        }
        if let result = attempt.wait(milliseconds: 60) { finishOpen(result, url: url) }
    }

    private func finishOpen(_ result: Result<SessionView, Error>, url: URL) {
        switch result {
        case .success(let session):
            apply(session: session)
            // Registering with the system's recent-documents list (the Dock
            // menu) is slow; do it once the window is up to date.
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
                NSDocumentController.shared.noteNewRecentDocumentURL(url)
            }
        case .failure(let error):
            // An alert cannot run before the application's event loop does
            // (a bad file named on the command line), so queue it.
            if NSApplication.shared.isRunning {
                Dialogs.present(error: error)
            } else {
                DispatchQueue.main.async { Dialogs.present(error: error) }
            }
        }
    }

    func openAudioPanel() {
        let types = supportedAudioExtensions().compactMap { UTType(filenameExtension: $0) }
        if let url = Dialogs.chooseFile(title: "Open Audio", types: types + [.audio]) { open(url: url) }
    }

    func openProjectPanel() {
        let directory = settings.general.defaultProjectDirectory.map { URL(fileURLWithPath: $0) }
        if let url = Dialogs.chooseFile(title: "Open Project", types: [Self.projectType], directory: directory) {
            open(url: url)
        }
    }

    static var projectType: UTType {
        UTType(filenameExtension: projectFileExtension()) ?? .json
    }

    /// Opens a recent item. If its file has gone missing the user can locate
    /// it again or drop the entry.
    func open(recent item: RecentItem) {
        if FileManager.default.fileExists(atPath: item.path) {
            open(url: URL(fileURLWithPath: item.path))
            return
        }
        switch Dialogs.missingRecent(fileName: item.fileName) {
        case .locate:
            let types: [UTType] = item.kind == .project ? [Self.projectType] : [.audio]
            guard let url = Dialogs.chooseFile(title: "Locate “\(item.fileName)”", types: types) else { return }
            try? core.removeRecent(id: item.id)
            open(url: url)
        case .remove:
            try? core.removeRecent(id: item.id)
        case .cancel:
            break
        }
    }

    func removeRecent(_ item: RecentItem) {
        attempt { try self.core.removeRecent(id: item.id) }
    }

    func clearRecents() {
        attempt { try self.core.clearRecents() }
        NSDocumentController.shared.clearRecentDocuments(nil)
    }

    /// Returns `true` once the project is safely on disk.
    @discardableResult
    func saveProject() -> Bool {
        guard hasProject else { return false }
        guard session.projectPath != nil else { return saveProjectAs() }
        return attempt { self.apply(session: try self.core.saveProject(path: nil)) }
    }

    @discardableResult
    func saveProjectAs() -> Bool {
        guard hasProject else { return false }
        let directory = session.projectPath.map { URL(fileURLWithPath: $0).deletingLastPathComponent() }
            ?? settings.general.defaultProjectDirectory.map { URL(fileURLWithPath: $0) }
        guard let url = Dialogs.chooseSaveLocation(
            title: "Save Project", name: documentName, type: Self.projectType, directory: directory)
        else { return false }
        return attempt { self.apply(session: try self.core.saveProject(path: url.path)) }
    }

    func closeProject() {
        guard hasProject, confirmDiscardChanges() else { return }
        attempt { self.apply(session: try self.core.closeProject()) }
    }

    /// Asks what to do with unsaved changes before they would be lost.
    /// Returns `true` when it is safe to proceed.
    func confirmDiscardChanges() -> Bool {
        guard session.dirty else { return true }
        switch Dialogs.unsavedChanges(documentName: documentName) {
        case .save: return saveProject()
        case .discard: return true
        case .cancel: return false
        }
    }

    func locateActiveRecording() {
        guard let recording = activeRecording else { return }
        let name = (recording.source.path as NSString).lastPathComponent
        guard let url = Dialogs.chooseFile(title: "Locate “\(name)”", types: [.audio]) else { return }
        let path = url.path
        inBackground(
            { try $0.relocateRecording(recordingId: recording.id, path: path) },
            then: { [weak self] in self?.apply(session: $0) }
        )
    }

    func updateLabel(for recording: Recording, label: RecordingLabel, kind: SourceKind) {
        guard label != recording.label || kind != recording.source.kind else { return }
        attempt {
            self.apply(session: try self.core.updateRecordingLabel(recordingId: recording.id, label: label, sourceKind: kind))
        }
    }

    /// Runs a quick core call on the main thread, showing any failure.
    @discardableResult
    private func attempt(_ work: () throws -> Void) -> Bool {
        do {
            try work()
            return true
        } catch {
            Dialogs.present(error: error)
            return false
        }
    }

    // MARK: - Analysis and export

    /// Forgets the analysis on screen so the next session update fetches it
    /// afresh.
    private func invalidateAnalysis() {
        analysisKey = ""
    }

    func reanalyse() {
        guard let id = session.activeRecordingId else { return }
        invalidateAnalysis()
        attempt { self.apply(session: try self.core.reanalyse(recordingId: id)) }
    }

    func setAnalyseIsolatedVocals(_ useVocals: Bool) {
        guard let id = session.activeRecordingId else { return }
        invalidateAnalysis()
        attempt {
            self.apply(session: try self.core.setAnalysisSource(
                recordingId: id, source: useVocals ? .isolatedVocalsWhenAvailable : .original))
        }
    }

    func export(_ format: ExportFormat) {
        guard let id = session.activeRecordingId else { return }
        let name = core.suggestedExportName(recordingId: id, format: format)
        let type = UTType(filenameExtension: exportFileExtension(format: format)) ?? .data
        guard let url = Dialogs.chooseSaveLocation(
            title: "Export", name: name, type: type, directory: exportDirectory)
        else { return }
        let path = url.path
        inBackground({ try $0.exportAnalysis(recordingId: id, format: format, path: path) })
    }

    func exportVocals() {
        guard let id = session.activeRecordingId, let recording = activeRecording else { return }
        let name = ((recording.audio.fileName as NSString).deletingPathExtension) + " vocals.wav"
        guard let url = Dialogs.chooseSaveLocation(
            title: "Export Vocals", name: name, type: .wav, directory: exportDirectory)
        else { return }
        let path = url.path
        inBackground({ try $0.exportVocalStem(recordingId: id, path: path) })
    }

    /// Exports start next to the project, or next to the audio.
    private var exportDirectory: URL? {
        if let path = session.projectPath { return URL(fileURLWithPath: path).deletingLastPathComponent() }
        if let path = activeRecording?.source.path { return URL(fileURLWithPath: path).deletingLastPathComponent() }
        return nil
    }

    // MARK: - Vocal isolation

    /// Isolates the active recording's vocals with `model`, asking first if
    /// that means downloading it.
    func isolateVocals(with model: SeparationModel) {
        guard let id = session.activeRecordingId, !isolationIsRunning else { return }
        if !model.installed {
            let agreed = Dialogs.confirm(
                title: "Download “\(model.name)”?",
                message: "VocalScope needs to download this model (\(Format.fileSize(model.sizeBytes))) once before it can isolate vocals. It comes from \(model.source). Licence: \(model.license).\n\nThis is the only time VocalScope uses the internet; your audio never leaves this Mac.",
                action: "Download")
            guard agreed else { return }
        }
        attempt { self.apply(isolation: try self.core.isolateVocals(recordingId: id, modelId: model.id)) }
    }

    func cancelIsolation() {
        core.cancelIsolation()
    }

    func setListeningToVocals(_ listening: Bool) {
        let core = self.core
        playbackQueue.async {
            do {
                let session = try core.setListeningToVocals(listening: listening)
                Task { @MainActor in AppModel.shared.apply(session: session) }
            } catch {
                Task { @MainActor in Dialogs.present(error: error) }
            }
        }
    }

    func removeVocals() {
        guard let id = session.activeRecordingId else { return }
        guard Dialogs.confirm(
            title: "Remove the isolated vocals?",
            message: "They can be made again at any time, which takes as long as it did the first time.",
            action: "Remove")
        else { return }
        invalidateAnalysis()
        attempt { self.apply(session: try self.core.removeVocalStem(recordingId: id)) }
    }

    func removeModel(_ model: SeparationModel) {
        attempt {
            try self.core.removeSeparationModel(modelId: model.id)
            self.models = self.core.separationModels()
        }
    }

    // MARK: - Comparison

    func addRecordingPanel() {
        guard hasProject else { return }
        let types = supportedAudioExtensions().compactMap { UTType(filenameExtension: $0) }
        guard let url = Dialogs.chooseFile(title: "Add a Recording to Compare", types: types + [.audio]) else { return }
        let path = url.path
        inBackground({ try $0.addRecording(path: path) }, then: { [weak self] in
            self?.apply(session: $0)
            self?.inspectorTab = .compare
            self?.inspectorShown = true
        })
    }

    func removeRecording(_ recording: Recording) {
        invalidateAnalysis()
        attempt { self.apply(session: try self.core.removeRecording(recordingId: recording.id)) }
    }

    /// Shows and plays the other recording of a compared pair, from the
    /// matching moment.
    func switchRecording() {
        guard let other = otherRecording else { return }
        setActiveRecording(other.id)
    }

    func setActiveRecording(_ id: Uuid) {
        guard id != session.activeRecordingId else { return }
        // Keep looking at the same music: carry the visible range across.
        if let active = session.activeRecordingId, let timeline {
            timeline.pendingView = mappedView(timeline.timeView, from: active, to: id)
        }
        invalidateAnalysis()
        let core = self.core
        playbackQueue.async {
            do {
                let session = try core.setActiveRecording(recordingId: id)
                Task { @MainActor in AppModel.shared.apply(session: session) }
            } catch {
                Task { @MainActor in Dialogs.present(error: error) }
            }
        }
    }

    private func mappedView(_ view: TimeView, from: Uuid, to: Uuid) -> TimeView? {
        guard let start = core.mapTime(fromRecordingId: from, toRecordingId: to, seconds: view.start),
              let end = core.mapTime(fromRecordingId: from, toRecordingId: to, seconds: view.start + view.span)
        else { return nil }
        return TimeView(start: start, span: end - start)
    }

    // MARK: - Playback

    private func transport(_ command: @escaping @Sendable (AppCore) throws -> PlaybackStatus) {
        let core = self.core
        playbackQueue.async {
            do {
                let status = try command(core)
                Task { @MainActor in AppModel.shared.apply(playback: status) }
            } catch {
                Task { @MainActor in Dialogs.present(error: error) }
            }
        }
    }

    func togglePlayback() {
        guard playback.hasTrack else { return }
        transport { try $0.togglePlayback() }
    }

    func stop() { transport { try $0.stop() } }

    /// Seeks are coalesced: while one is in flight only the newest target is
    /// kept, so dragging across the timeline never builds up a backlog.
    func seek(to seconds: Double) {
        guard seekSlot.put(seconds) else { return }
        let core = self.core
        let slot = seekSlot
        playbackQueue.async {
            while let target = slot.take() {
                if let status = try? core.seek(seconds: target) {
                    Task { @MainActor in AppModel.shared.apply(playback: status) }
                }
            }
        }
    }

    func skip(by delta: Double) {
        seek(to: max(0, core.playbackStatus().positionSeconds + delta))
    }

    func setVolume(_ volume: Float) {
        let clamped = min(1, max(0, volume))
        transport { try $0.setVolume(volume: clamped) }
    }

    func toggleMute() {
        let muted = !playback.muted
        transport { try $0.setMuted(muted: muted) }
    }

    // MARK: - Settings

    func updateSettings(_ change: (inout CoreSettings) -> Void) {
        var draft = settings
        change(&draft)
        guard draft != settings else { return }
        attempt { self.apply(settings: try self.core.updateSettings(settings: draft)) }
    }

    func resetSettings() {
        attempt { self.apply(settings: try self.core.resetSettings()) }
    }

    /// A two-way binding to one setting; writing it saves immediately.
    func setting<Value>(_ keyPath: WritableKeyPath<CoreSettings, Value>) -> Binding<Value> {
        Binding(
            get: { self.settings[keyPath: keyPath] },
            set: { value in self.updateSettings { $0[keyPath: keyPath] = value } }
        )
    }
}

/// One attempt to open a file: lets the main thread wait a moment for the
/// result and, if it gives up waiting, tells the worker to deliver it later.
private final class OpenAttempt: @unchecked Sendable {
    private let lock = NSLock()
    private let finished = DispatchSemaphore(value: 0)
    private var result: Result<SessionView, Error>?
    private var abandoned = false

    /// Worker side. Returns `true` when nobody is waiting any more and the
    /// worker must deliver the result itself.
    func complete(_ result: Result<SessionView, Error>) -> Bool {
        lock.lock()
        self.result = result
        let late = abandoned
        lock.unlock()
        finished.signal()
        return late
    }

    /// Main-thread side. Returns the result if it arrived in time.
    func wait(milliseconds: Int) -> Result<SessionView, Error>? {
        _ = finished.wait(timeout: .now() + .milliseconds(milliseconds))
        lock.lock()
        defer { lock.unlock() }
        if result == nil { abandoned = true }
        return result
    }
}

/// Holds at most one pending seek target, shared between the main thread
/// (which writes) and the playback queue (which drains).
private final class SeekSlot: @unchecked Sendable {
    private let lock = NSLock()
    private var target: Double?
    private var draining = false

    /// Stores `seconds` as the newest target. Returns `true` when the caller
    /// must start a drain loop, `false` when one is already running.
    func put(_ seconds: Double) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        target = seconds
        if draining { return false }
        draining = true
        return true
    }

    /// Takes the newest target, or ends the drain when there is none.
    func take() -> Double? {
        lock.lock()
        defer { lock.unlock() }
        let next = target
        target = nil
        if next == nil { draining = false }
        return next
    }
}

/// Sits in front of SwiftUI's window delegate to add one behaviour — asking
/// about unsaved changes on close — and forwards everything else untouched.
final class WindowCloseGuard: NSObject, NSWindowDelegate {
    weak var original: NSWindowDelegate?
    var shouldClose: () -> Bool = { true }

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        guard shouldClose() else { return false }
        return original?.windowShouldClose?(sender) ?? true
    }

    override func responds(to selector: Selector!) -> Bool {
        super.responds(to: selector) || (original?.responds(to: selector) ?? false)
    }

    override func forwardingTarget(for selector: Selector!) -> Any? {
        original
    }
}
