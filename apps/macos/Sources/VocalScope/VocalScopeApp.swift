import AppKit
import SwiftUI
import VocalScopeCore

@main
struct VocalScopeApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var model = AppModel.shared

    var body: some Scene {
        // One window. A `WindowGroup` (rather than `Window`) is what lets the
        // app be launched by opening a file; the two `handlesExternalEvents`
        // calls route every later open to this same window instead of
        // spawning another, and the New Window command is removed below.
        WindowGroup("VocalScope", id: "main") {
            MainView()
                .environmentObject(model)
                .handlesExternalEvents(preferring: ["*"], allowing: ["*"])
        }
        .handlesExternalEvents(matching: ["*"])
        .defaultSize(width: 1180, height: 700)
        .commands { AppCommands(model: model) }

        SwiftUI.Settings {
            SettingsView()
                .environmentObject(model)
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        Launch.mark("launched")
    }

    /// Files opened from Finder: double-click, Open With, or a drop on the
    /// Dock icon. (A path given on the command line is opened even earlier,
    /// by `AppModel`; AppKit sometimes reports it here as well.)
    func application(_ application: NSApplication, open urls: [URL]) {
        guard let url = urls.first?.standardizedFileURL else { return }
        if url == AppModel.shared.launchDocument, AppModel.shared.hasProject { return }
        AppModel.shared.open(url: url)
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        AppModel.shared.confirmDiscardChanges() ? .terminateNow : .terminateCancel
    }

    func applicationSupportsSecureRestorableState(_ app: NSApplication) -> Bool {
        true
    }
}

/// Launch-time measurement for `scripts/bench-macos.sh`. With
/// `VOCALSCOPE_BENCH=1` the app prints how long each launch milestone took
/// and, with `VOCALSCOPE_BENCH_EXIT=1`, quits once the last one is reached,
/// so start-up can be timed repeatably. It does nothing otherwise.
enum Launch {
    private static let enabled = ProcessInfo.processInfo.environment["VOCALSCOPE_BENCH"] == "1"
    private static let exits = ProcessInfo.processInfo.environment["VOCALSCOPE_BENCH_EXIT"] == "1"
    private static var seen = Set<String>()

    /// Milliseconds since the process was created, from the kernel's record.
    private static func millisecondsSinceProcessStart() -> Double {
        var info = kinfo_proc()
        var size = MemoryLayout<kinfo_proc>.stride
        var query = [CTL_KERN, KERN_PROC, KERN_PROC_PID, getpid()]
        guard sysctl(&query, UInt32(query.count), &info, &size, nil, 0) == 0 else { return -1 }
        let started = info.kp_proc.p_starttime
        var now = timeval()
        gettimeofday(&now, nil)
        return Double(now.tv_sec - started.tv_sec) * 1000 + Double(now.tv_usec - started.tv_usec) / 1000
    }

    static func mark(_ milestone: String, final: Bool = false) {
        guard enabled, seen.insert(milestone).inserted else { return }
        let elapsed = millisecondsSinceProcessStart()
        print("bench \(milestone) \(String(format: "%.1f", elapsed))")
        fflush(stdout)
        if final, exits {
            // Let the frame that satisfied the milestone reach the screen.
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) { exit(0) }
        }
    }

    static var waitsForDocument: Bool {
        CommandLine.arguments.dropFirst().contains { !$0.hasPrefix("-") }
    }
}
