import SwiftUI
import VocalScopeCore

/// Comparison of two versions of a recording: which is which, how they line
/// up, and where their pitch differs.
struct CompareInspector: View {
    @EnvironmentObject private var model: AppModel
    let recording: Recording

    var body: some View {
        Form {
            if let comparison = model.session.comparison {
                recordings(comparison)
                alignment(comparison)
                differences(comparison)
            } else {
                Section {
                    Text("Add a second version of this recording — a remaster, a live take released as studio, a different pressing — to line the two up and see where their pitch differs.")
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    Button("Add Recording to Compare…") { model.addRecordingPanel() }
                }
            }
        }
        .formStyle(.grouped)
        .monospacedDigit()
    }

    @ViewBuilder
    private func recordings(_ comparison: ComparisonView) -> some View {
        Section("Recordings") {
            ForEach(model.session.recordings, id: \.recordingId) { runtime in
                let active = runtime.recordingId == model.session.activeRecordingId
                HStack(spacing: 8) {
                    Text(model.letter(for: runtime.recordingId))
                        .font(.caption.weight(.bold))
                        .frame(width: 18, height: 18)
                        .background(
                            active ? Color.accentColor : Color.orange,
                            in: RoundedRectangle(cornerRadius: 4))
                        .foregroundStyle(.white)
                        .accessibilityLabel("Recording \(model.letter(for: runtime.recordingId))")
                    VStack(alignment: .leading, spacing: 1) {
                        Text(runtime.displayTitle)
                            .lineLimit(1)
                        Text(active ? "Shown and playing" : (runtime.displayDetail ?? "Overlaid in orange"))
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                    Spacer(minLength: 4)
                    if !active {
                        Button("Switch") { model.setActiveRecording(runtime.recordingId) }
                            .help("Show and play this recording from the matching moment (X)")
                    }
                }
                .contextMenu {
                    Button("Remove from Project") {
                        if let target = model.session.project?.recordings.first(where: { $0.id == runtime.recordingId }) {
                            model.removeRecording(target)
                        }
                    }
                }
            }
            if let other = model.otherRecording {
                Button("Remove “\(model.session.recordings.first { $0.recordingId == other.id }?.displayTitle ?? "the other recording")”", role: .destructive) {
                    model.removeRecording(other)
                }
            }
        }
    }

    @ViewBuilder
    private func alignment(_ comparison: ComparisonView) -> some View {
        Section("Alignment") {
            if let alignment = comparison.alignment {
                LabeledContent("Match") {
                    Label(alignment.quality.label, systemImage: alignment.quality == .good ? "checkmark.circle" : "exclamationmark.triangle")
                        .foregroundStyle(alignment.quality == .good ? Color.primary : Color.orange)
                }
                LabeledContent("Similarity", value: Format.percent(alignment.confidence))
                LabeledContent("B starts", value: offsetText(alignment.offsetSeconds))
                LabeledContent("Speed", value: speedText(alignment.speedRatio))
                if alignment.quality == .poor {
                    Text("These may not be the same performance. The comparison below assumes they are, so treat it with care.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            } else if model.session.recordings.allSatisfy({ $0.waveformStatus == .ready }) {
                Text("The recordings are too short to line up.")
                    .foregroundStyle(.secondary)
            } else {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Reading both recordings…").foregroundStyle(.secondary)
                }
            }
        }
    }

    @ViewBuilder
    private func differences(_ comparison: ComparisonView) -> some View {
        Section {
            if let pitch = comparison.pitch {
                LabeledContent("Compared", value: Format.time(pitch.comparedSeconds, decimals: 1))
                LabeledContent("Overall shift") {
                    Text(Format.cents(pitch.medianDifferenceCents, digits: 1))
                        .help("How much higher (+) or lower (−) recording B is throughout. A constant shift is a transposition or speed difference, not a tuning change.")
                }
                LabeledContent("Typical difference", value: pitch.typicalDifferenceCents.map { Format.cents(abs($0), digits: 1).replacingOccurrences(of: "+", with: "") } ?? Format.unknown)
                LabeledContent("Within 10 cents", value: Format.percent(pitch.shareWithin10Cents))
                if pitch.regions.isEmpty {
                    Text("No passage differs by more than 25 cents.")
                        .foregroundStyle(.secondary)
                } else {
                    ForEach(Array(pitch.regions.prefix(200).enumerated()), id: \.offset) { _, region in
                        Button {
                            reveal(region, comparison: comparison)
                        } label: {
                            HStack {
                                Text("\(Format.time(region.startSeconds, decimals: 2)) – \(Format.time(region.endSeconds, decimals: 2))")
                                Spacer()
                                Text(Format.cents(region.meanDifferenceCents))
                                    .foregroundStyle(.secondary)
                            }
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .help("Go to this passage")
                    }
                    if pitch.regions.count > 200 {
                        Text("…and \(pitch.regions.count - 200) more. The JSON export lists them all.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            } else if comparison.alignment != nil {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Analysing both recordings…").foregroundStyle(.secondary)
                }
            } else {
                Text("Available once the recordings are lined up.")
                    .foregroundStyle(.secondary)
            }
        } header: {
            Text("Pitch Differences")
        } footer: {
            if let pitch = comparison.pitch, !pitch.regions.isEmpty {
                Text("Passages where B is more than 25 cents from A, beyond the overall shift. Times are on recording A. Positive means B is higher.")
            }
        }
    }

    /// Moves the playhead to a differing passage and brings it into view,
    /// whichever recording is on screen.
    private func reveal(_ region: DifferenceRegion, comparison: ComparisonView) {
        var start = region.startSeconds
        var end = region.endSeconds
        if model.session.activeRecordingId == comparison.otherRecordingId, let alignment = comparison.alignment {
            start = alignment.offsetSeconds + alignment.speedRatio * start
            end = alignment.offsetSeconds + alignment.speedRatio * end
        }
        model.seek(to: start)
        model.timeline?.reveal(from: start, to: end)
    }

    private func offsetText(_ seconds: Double) -> String {
        let size = abs(seconds).formatted(.number.precision(.fractionLength(3)))
        if abs(seconds) < 0.0005 { return "At the same time" }
        return "\(size) s \(seconds > 0 ? "later" : "earlier")"
    }

    private func speedText(_ ratio: Double) -> String {
        if ratio == 1 { return "The same" }
        let percent = (abs(ratio - 1) * 100).formatted(.number.precision(.fractionLength(3)))
        return "B is \(percent)% \(ratio > 1 ? "slower" : "faster")"
    }
}
