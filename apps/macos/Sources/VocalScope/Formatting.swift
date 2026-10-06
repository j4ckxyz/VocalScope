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
