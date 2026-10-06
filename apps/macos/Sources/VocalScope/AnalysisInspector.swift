import SwiftUI
import VocalScopeCore

/// The pitch analysis of the active recording: what was sung, what the
/// correction indicators suggest, and the isolated vocals it can be made from.
struct AnalysisInspector: View {
    @EnvironmentObject private var model: AppModel
    let recording: Recording

    var body: some View {
        Form {
            PitchSection()
            if let analysis = model.analysis {
                IndicatorsSection(analysis: analysis)
            }
            VocalsSection(recording: recording)
        }
        .formStyle(.grouped)
        .monospacedDigit()
    }
}

private struct PitchSection: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        Section("Pitch") {
            switch model.activeRuntime?.analysisStatus {
            case .ready:
                if let analysis = model.analysis {
                    summary(analysis)
                }
            case .failed:
                Label(
                    model.activeRuntime?.analysisError?.message ?? "The pitch could not be analysed.",
                    systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.secondary)
                Button("Analyse Again") { model.reanalyse() }
            case .pending:
                HStack(spacing: 8) {
                    if let progress = model.analysisProgress {
                        ProgressView(value: Double(progress))
                    } else {
                        ProgressView().controlSize(.small)
                    }
                    Text("Analysing pitch…")
                        .foregroundStyle(.secondary)
                        .fixedSize()
                }
            default:
                Text("The pitch is analysed once the audio has been read.")
                    .foregroundStyle(.secondary)
            }
            Toggle("Show pitch on the timeline", isOn: $model.pitchShown)
        }
    }

    @ViewBuilder
    private func summary(_ analysis: AnalysisView) -> some View {
        let summary = analysis.summary
        LabeledContent("Analysed", value: analysis.isolatedVocals ? "Isolated vocals" : "The recording as it is")
        LabeledContent("Sung time", value: Format.time(summary.voicedSeconds, decimals: 1))
        LabeledContent("Notes", value: "\(summary.noteCount)")
        if let low = summary.lowestNote, let high = summary.highestNote {
            LabeledContent("Range", value: "\(low) – \(high)")
        }
        if let note = summary.medianNote {
            LabeledContent("Median pitch", value: "\(note) · \(Format.hertz(summary.medianFrequencyHz))")
        }
        if let reference = summary.referencePitchHz {
            LabeledContent("Tuning") {
                Text("A4 ≈ \(Format.hertz(reference))")
                    .help("Where this recording's notes cluster, compared with standard pitch (A4 = 440 Hz): \(Format.cents(summary.tuningOffsetCents)).")
            }
        }
        if let caution = analysis.caution {
            Label(caution, systemImage: "info.circle")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}

private struct IndicatorsSection: View {
    let analysis: AnalysisView

    var body: some View {
        let report = analysis.indicators
        Section("Pitch-Correction Indicators") {
            VStack(alignment: .leading, spacing: 4) {
                Text(report.headline)
                    .font(.headline)
                    .fixedSize(horizontal: false, vertical: true)
                Text(report.summary)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .accessibilityElement(children: .combine)

            ForEach(report.indicators, id: \.id) { indicator in
                IndicatorRow(indicator: indicator)
            }

            Text(report.caveat)
                .font(.footnote)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}

private struct IndicatorRow: View {
    let indicator: Indicator
    @State private var expanded = false

    var body: some View {
        DisclosureGroup(isExpanded: $expanded) {
            Text(indicator.explanation)
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        } label: {
            VStack(alignment: .leading, spacing: 3) {
                HStack(alignment: .firstTextBaseline) {
                    Text(indicator.title)
                    Spacer(minLength: 8)
                    Text(indicator.displayValue)
                        .foregroundStyle(.secondary)
                }
                AssessmentBadge(assessment: indicator.assessment)
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(indicator.title): \(indicator.displayValue). \(indicator.assessment.label).")
    }
}

/// The reading of one indicator. Colour is a hint only; the words carry it.
private struct AssessmentBadge: View {
    let assessment: Assessment

    private var colour: Color {
        switch assessment {
        case .typicalOfUnprocessed: return .green
        case .consistentWithCorrection: return .orange
        case .inconclusive, .notEnoughData: return .secondary
        }
    }

    private var symbol: String {
        switch assessment {
        case .typicalOfUnprocessed: return "waveform.path"
        case .consistentWithCorrection: return "ruler"
        case .inconclusive: return "questionmark"
        case .notEnoughData: return "minus"
        }
    }

    var body: some View {
        Label(assessment.label, systemImage: symbol)
            .font(.caption.weight(.medium))
            .foregroundStyle(colour)
    }
}

private struct VocalsSection: View {
    @EnvironmentObject private var model: AppModel
    let recording: Recording
    @State private var chosenModel: String?

    private var selected: SeparationModel? {
        model.models.first { $0.id == chosenModel }
            ?? model.models.first { $0.recommended }
            ?? model.models.first
    }

    var body: some View {
        Section {
            if model.isolationIsRunning {
                progress
            } else if let stem = model.activeRuntime?.vocalStem {
                isolated(stem)
            } else {
                offer
            }
        } header: {
            Text("Vocals")
        } footer: {
            if model.activeRuntime?.vocalStem == nil, !model.isolationIsRunning {
                Text("Isolating the vocals of a full song first makes the pitch analysis far more dependable. It runs entirely on this Mac.")
            }
        }
    }

    @ViewBuilder
    private var progress: some View {
        let status = model.isolation
        let ours = status.recordingId == recording.id
        VStack(alignment: .leading, spacing: 6) {
            Text(ours ? status.stage.label : "Isolating the vocals of another recording…")
            if let fraction = status.fraction {
                ProgressView(value: Double(fraction))
            } else {
                ProgressView().progressViewStyle(.linear)
            }
        }
        Button("Cancel") { model.cancelIsolation() }
    }

    @ViewBuilder
    private func isolated(_ stem: VocalStem) -> some View {
        LabeledContent("Isolated with", value: stem.modelName)
        Toggle("Analyse the isolated vocals", isOn: Binding(
            get: { recording.analysisSource == .isolatedVocalsWhenAvailable },
            set: { model.setAnalyseIsolatedVocals($0) }))
        Toggle("Listen to the isolated vocals", isOn: Binding(
            get: { model.session.listeningToVocals },
            set: { model.setListeningToVocals($0) }))
        HStack {
            Button("Export…") { model.exportVocals() }
            Button("Remove", role: .destructive) { model.removeVocals() }
        }
    }

    @ViewBuilder
    private var offer: some View {
        if model.isolation.stage == .failed, model.isolation.recordingId == recording.id,
           let error = model.isolation.error
        {
            Label {
                Text([error.message, error.suggestion].compactMap { $0 }.joined(separator: " "))
                    .fixedSize(horizontal: false, vertical: true)
            } icon: {
                Image(systemName: "exclamationmark.triangle")
            }
            .font(.callout)
            .foregroundStyle(.secondary)
        }
        Picker("Model", selection: Binding(
            get: { selected?.id ?? "" },
            set: { chosenModel = $0 })
        ) {
            ForEach(model.models) { item in
                Text(item.recommended ? "\(item.name) (suggested)" : item.name).tag(item.id)
            }
        }
        if let selected {
            VStack(alignment: .leading, spacing: 3) {
                Text(selected.description)
                    .fixedSize(horizontal: false, vertical: true)
                Text("\(selected.installed ? "Downloaded" : Format.fileSize(selected.sizeBytes) + " download") · Licence: \(selected.license)")
                Text(selected.source)
            }
            .font(.caption)
            .foregroundStyle(.secondary)

            HStack {
                Button(selected.installed ? "Isolate Vocals" : "Download and Isolate…") {
                    model.isolateVocals(with: selected)
                }
                .disabled(model.activeRuntime?.sourceExists != true)
                if selected.installed {
                    Button("Delete Model") { model.removeModel(selected) }
                        .help("Frees \(Format.fileSize(selected.sizeBytes)) of disk space. The model can be downloaded again.")
                }
            }
        }
    }
}
