import AppKit
import SwiftUI
import VocalScopeCore

/// The Settings window (⌘,). Only settings that take effect today are shown;
/// panes for analysis, vocal separation and updates arrive with those
/// features.
struct SettingsView: View {
    var body: some View {
        TabView {
            GeneralSettings()
                .tabItem { Label("General", systemImage: "gearshape") }
            PlaybackSettings()
                .tabItem { Label("Playback", systemImage: "speaker.wave.2") }
            AdvancedSettings()
                .tabItem { Label("Advanced", systemImage: "wrench.and.screwdriver") }
        }
        .frame(width: 520)
    }
}

private struct GeneralSettings: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        Form {
            Picker("Appearance", selection: model.setting(\.general.theme)) {
                Text("Match System").tag(Theme.system)
                Text("Light").tag(Theme.light)
                Text("Dark").tag(Theme.dark)
            }

            LabeledContent("Default project folder") {
                VStack(alignment: .trailing, spacing: 6) {
                    Text(model.settings.general.defaultProjectDirectory.map { ($0 as NSString).abbreviatingWithTildeInPath } ?? "System default")
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    HStack {
                        Button("Choose…") {
                            if let url = Dialogs.chooseDirectory(title: "Default Project Folder") {
                                model.updateSettings { $0.general.defaultProjectDirectory = url.path }
                            }
                        }
                        Button("Reset") {
                            model.updateSettings { $0.general.defaultProjectDirectory = nil }
                        }
                        .disabled(model.settings.general.defaultProjectDirectory == nil)
                    }
                }
            }

            Stepper(
                "Recent files shown: \(model.settings.general.recentFileCount)",
                value: model.setting(\.general.recentFileCount), in: 1...30)

            LabeledContent("Recent files") {
                Button("Clear Recent Files…") {
                    if Dialogs.confirm(
                        title: "Clear the recent files list?",
                        message: "The files themselves are not affected.", action: "Clear") {
                        model.clearRecents()
                    }
                }
                .disabled(model.recents.isEmpty)
            }
        }
        .formStyle(.grouped)
    }
}

private struct PlaybackSettings: View {
    @EnvironmentObject private var model: AppModel
    @State private var devices: [OutputDevice] = []

    private var savedDevice: String? { model.settings.playback.outputDevice }
    private var savedDeviceMissing: Bool {
        guard let savedDevice else { return false }
        return !devices.contains { $0.name == savedDevice }
    }

    var body: some View {
        Form {
            Picker("Output device", selection: model.setting(\.playback.outputDevice)) {
                Text("System Default").tag(String?.none)
                Divider()
                ForEach(devices, id: \.name) { device in
                    Text(device.isDefault ? "\(device.name) (current default)" : device.name)
                        .tag(String?.some(device.name))
                }
                // A saved device that is unplugged stays visible rather than
                // being silently replaced.
                if savedDeviceMissing, let savedDevice {
                    Text("\(savedDevice) (not connected)").tag(String?.some(savedDevice))
                }
            }
            if savedDeviceMissing {
                Text("This device is not connected. The system default is used until it returns.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            LabeledContent("Volume at launch") {
                HStack {
                    Slider(value: model.setting(\.playback.defaultVolume), in: 0...1)
                        .frame(width: 180)
                    Text(model.settings.playback.defaultVolume, format: .percent.precision(.fractionLength(0)))
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                        .frame(width: 40, alignment: .trailing)
                }
            }
        }
        .formStyle(.grouped)
        .task { devices = model.core.outputDevices() }
    }
}

private struct AdvancedSettings: View {
    @EnvironmentObject private var model: AppModel
    @State private var diagnostics: Diagnostics?

    var body: some View {
        Form {
            Section("Diagnostics") {
                if let diagnostics {
                    let hardware = diagnostics.hardware
                    LabeledContent("Version", value: diagnostics.applicationVersion)
                    LabeledContent("System", value: "macOS \(ProcessInfo.processInfo.operatingSystemVersionString)")
                    LabeledContent("Processor", value: "\(hardware.cpuModel) · \(hardware.logicalCpuCount) cores")
                    LabeledContent("Memory", value: "\(Format.memory(hardware.totalMemoryBytes)) · \(Format.memory(hardware.availableMemoryBytes)) available")
                    LabeledContent("Memory pressure", value: hardware.memoryPressure?.label ?? Format.unknown)
                    LabeledContent("CPU load", value: hardware.cpuUsagePercent.map { "\(Int($0.rounded()))%" } ?? Format.unknown)
                    Button("Copy Diagnostics") { copy(diagnostics) }
                } else {
                    ProgressView().controlSize(.small)
                }
            }

            Section {
                LabeledContent("Logs") {
                    Button("Show in Finder") {
                        guard let directory = diagnostics?.logDirectory else { return }
                        NSWorkspace.shared.open(URL(fileURLWithPath: directory))
                    }
                    .disabled(diagnostics == nil)
                }
                LabeledContent("Settings") {
                    Button("Reset All Settings…") {
                        if Dialogs.confirm(
                            title: "Reset all settings?",
                            message: "Settings return to their defaults. Projects, recent files and audio are not affected.",
                            action: "Reset") {
                            model.resetSettings()
                        }
                    }
                }
            } footer: {
                Text("Logs stay on this Mac. They record versions, hardware and errors — never audio.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .formStyle(.grouped)
        .task {
            // Sampling CPU load takes a moment; keep it off the main thread.
            let core = model.core
            diagnostics = await Task.detached { core.diagnostics() }.value
        }
    }

    private func copy(_ diagnostics: Diagnostics) {
        let hardware = diagnostics.hardware
        let text = [
            "VocalScope \(diagnostics.applicationVersion)",
            "macOS \(ProcessInfo.processInfo.operatingSystemVersionString) (\(hardware.cpuArchitecture))",
            "\(hardware.cpuModel), \(hardware.logicalCpuCount) logical CPUs",
            "Memory: \(Format.memory(hardware.totalMemoryBytes)) total, \(Format.memory(hardware.availableMemoryBytes)) available",
            "Memory pressure: \(hardware.memoryPressure?.label ?? "unknown")",
        ].joined(separator: "\n")
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }
}
