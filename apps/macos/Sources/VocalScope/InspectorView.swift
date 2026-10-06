import SwiftUI
import VocalScopeCore

/// Details of the active recording: how the user labels it (editable, saved
/// in the project) and what the file is (read-only, exactly as found).
struct InspectorView: View {
    @EnvironmentObject private var model: AppModel
    let recording: Recording

    private enum Field: Hashable { case name, version, year, notes }

    @State private var name = ""
    @State private var version = ""
    @State private var year = ""
    @State private var notes = ""
    @State private var kind: SourceKind = .unspecified
    @FocusState private var focus: Field?

    private var parsedYear: Int32? {
        let trimmed = year.trimmingCharacters(in: .whitespaces)
        guard trimmed.count == 4, let value = Int32(trimmed), (1000...2999).contains(value) else { return nil }
        return value
    }

    private var yearIsInvalid: Bool {
        !year.trimmingCharacters(in: .whitespaces).isEmpty && parsedYear == nil
    }

    var body: some View {
        Form {
            Section("Label") {
                TextField("Name", text: $name, prompt: Text(model.activeRuntime?.displayTitle ?? ""))
                    .focused($focus, equals: .name)
                TextField("Version", text: $version, prompt: Text("e.g. Original release"))
                    .focused($focus, equals: .version)
                TextField("Release year", text: $year)
                    .focused($focus, equals: .year)
                if yearIsInvalid {
                    Text("Enter a four-digit year, or leave it blank.")
                        .font(.caption)
                        .foregroundStyle(.red)
                }
                Picker("This audio is", selection: $kind) {
                    ForEach(SourceKind.allCases, id: \.self) { Text($0.label).tag($0) }
                }
                TextField("Notes", text: $notes, axis: .vertical)
                    .lineLimit(3...8)
                    .focused($focus, equals: .notes)
            }

            Section("File") {
                let audio = recording.audio
                LabeledContent("Name", value: audio.fileName)
                LabeledContent("Format", value: audio.codecDescription)
                LabeledContent("Duration", value: Format.time(recording.waveform?.durationSeconds ?? audio.durationSeconds, decimals: 3))
                LabeledContent("Sample rate", value: Format.sampleRate(audio.sampleRateHz))
                LabeledContent("Channels", value: Format.channels(audio.channelCount))
                if let waveform = recording.waveform, audio.channelCount == 2 {
                    LabeledContent("Stereo content", value: waveform.stereoContent.label)
                }
                LabeledContent("Bit depth", value: audio.bitDepth.map { "\($0)-bit" } ?? "Not applicable")
                LabeledContent("Bitrate", value: audio.averageBitrateKbps.map { "\($0) kbps average" } ?? Format.unknown)
                LabeledContent("Peak level", value: Format.level(recording.waveform?.peakDbfs))
                LabeledContent("Size", value: Format.fileSize(audio.fileSizeBytes))
            }

            Section("Embedded Tags") {
                let tags = recording.audio.tags
                if tags.title == nil, tags.artist == nil, tags.album == nil, tags.year == nil, tags.genre == nil {
                    Text("This file has no embedded tags.")
                        .foregroundStyle(.secondary)
                } else {
                    LabeledContent("Title", value: tags.title ?? Format.unknown)
                    LabeledContent("Artist", value: tags.artist ?? Format.unknown)
                    LabeledContent("Album", value: tags.album ?? Format.unknown)
                    LabeledContent("Year", value: tags.year.map { String($0) } ?? Format.unknown)
                    if let genre = tags.genre { LabeledContent("Genre", value: genre) }
                }
            }
        }
        .formStyle(.grouped)
        .monospacedDigit()
        .onAppear(perform: load)
        .onChange(of: recording.id) { load() }
        .onChange(of: recording.label) { if focus == nil { load() } }
        .onChange(of: focus) { commit() }
        .onChange(of: kind) { commit() }
        .onSubmit(commit)
    }

    /// Copies the saved label into the fields.
    private func load() {
        name = recording.label.recordingName ?? ""
        version = recording.label.version ?? ""
        year = recording.label.releaseYear.map { String($0) } ?? ""
        notes = recording.label.notes ?? ""
        kind = recording.source.kind
    }

    private func commit() {
        guard !yearIsInvalid else { return }
        func clean(_ text: String) -> String? {
            let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
            return trimmed.isEmpty ? nil : trimmed
        }
        let label = RecordingLabel(
            recordingName: clean(name), version: clean(version), releaseYear: parsedYear, notes: clean(notes))
        model.updateLabel(for: recording, label: label, kind: kind)
    }
}
