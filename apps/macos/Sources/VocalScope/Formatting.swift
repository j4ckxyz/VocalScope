import Foundation
import VocalScopeCore

/// SwiftUI also has a `Settings` (the scene), so the core's record gets an
/// unambiguous name here.
typealias CoreSettings = VocalScopeCore.Settings

/// Display formatting. Anything genuinely unknown is shown as a dash; nothing
/// is ever guessed.
enum Format {
    static let unknown = "—"

    static func time(_ seconds: Double?, decimals: UInt32 = 0) -> String {
        guard let seconds else { return unknown }
        return formatTime(seconds: seconds, decimals: decimals)
    }

    static func fileSize(_ bytes: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(clamping: bytes), countStyle: .file)
    }

    static func memory(_ bytes: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(clamping: bytes), countStyle: .memory)
    }

    static func sampleRate(_ hertz: UInt32) -> String {
        let kilohertz = Double(hertz) / 1000
        let digits = kilohertz == kilohertz.rounded() ? 0 : 1
        return "\(kilohertz.formatted(.number.precision(.fractionLength(digits)))) kHz"
    }

    static func channels(_ count: UInt16) -> String {
        switch count {
        case 1: return "Mono"
        case 2: return "Stereo"
        default: return "\(count) channels"
        }
    }

    static func level(_ decibels: Float?) -> String {
        guard let decibels, decibels.isFinite else { return unknown }
        let rounded = (Double(decibels) * 10).rounded() / 10
        // Avoid "-0.0".
        let value = rounded == 0 ? 0 : rounded
        return "\(value.formatted(.number.precision(.fractionLength(1)))) dBFS"
    }
}

extension StereoContent {
    var label: String {
        switch self {
        case .mono: return "Mono"
        case .dualMono: return "Identical left and right"
        case .stereo: return "Distinct left and right"
        case .multichannel: return "More than two channels"
        }
    }
}

extension SourceKind {
    static let allCases: [SourceKind] = [.unspecified, .fullMix, .vocalStem]

    var label: String {
        switch self {
        case .unspecified: return "Not specified"
        case .fullMix: return "Full mix"
        case .vocalStem: return "Vocal stem"
        }
    }
}

extension MemoryPressure {
    var label: String {
        switch self {
        case .normal: return "Normal"
        case .warning: return "Warning"
        case .critical: return "Critical"
        }
    }
}

extension RecentItem: Identifiable {}
extension SeparationModel: Identifiable {}

/// The pages of the inspector.
enum InspectorTab: String, CaseIterable {
    case details, analysis, compare

    var label: String {
        switch self {
        case .details: return "Details"
        case .analysis: return "Analysis"
        case .compare: return "Compare"
        }
    }
}

extension Format {
    /// A distance in cents with its sign, e.g. "+12 cents".
    static func cents(_ value: Float?, digits: Int = 0) -> String {
        guard let value, value.isFinite else { return unknown }
        let rounded = Double(value)
        let text = rounded.formatted(.number.precision(.fractionLength(digits)).sign(strategy: .always(includingZero: false)))
        return "\(text) cents"
    }

    static func hertz(_ value: Float?) -> String {
        guard let value, value.isFinite else { return unknown }
        return "\(Double(value).formatted(.number.precision(.fractionLength(1)))) Hz"
    }

    static func percent(_ fraction: Float?) -> String {
        guard let fraction, fraction.isFinite else { return unknown }
        return Double(fraction).formatted(.percent.precision(.fractionLength(0)))
    }
}

extension Assessment {
    var label: String {
        switch self {
        case .notEnoughData: return "Not enough data"
        case .typicalOfUnprocessed: return "Typical of unprocessed singing"
        case .inconclusive: return "Inconclusive"
        case .consistentWithCorrection: return "Consistent with pitch correction"
        }
    }
}

extension AlignmentQuality {
    var label: String {
        switch self {
        case .good: return "Lined up"
        case .uncertain: return "Probably lined up — check by ear"
        case .poor: return "No convincing match"
        }
    }
}

extension IsolationStage {
    var label: String {
        switch self {
        case .idle: return ""
        case .downloading: return "Downloading the model…"
        case .preparing: return "Loading the model…"
        case .isolating: return "Isolating vocals…"
        case .failed: return "Isolation failed"
        }
    }
}
