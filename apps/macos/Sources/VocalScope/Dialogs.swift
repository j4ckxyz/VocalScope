import AppKit
import UniformTypeIdentifiers
import VocalScopeCore

/// Standard system panels and alerts.
@MainActor
enum Dialogs {
    enum UnsavedChoice { case save, discard, cancel }
    enum MissingRecentChoice { case locate, remove, cancel }

    static func chooseFile(title: String, types: [UTType], directory: URL? = nil) -> URL? {
        let panel = NSOpenPanel()
        panel.title = title
        panel.message = title
        panel.allowedContentTypes = types
        panel.allowsMultipleSelection = false
        panel.canChooseDirectories = false
        if let directory { panel.directoryURL = directory }
        return panel.runModal() == .OK ? panel.url : nil
    }

    static func chooseDirectory(title: String, directory: URL? = nil) -> URL? {
        let panel = NSOpenPanel()
        panel.title = title
        panel.message = title
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.canCreateDirectories = true
        panel.prompt = "Choose"
        if let directory { panel.directoryURL = directory }
        return panel.runModal() == .OK ? panel.url : nil
    }

    static func chooseSaveLocation(name: String, type: UTType, directory: URL?) -> URL? {
        let panel = NSSavePanel()
        panel.title = "Save Project"
        panel.nameFieldStringValue = name
        panel.allowedContentTypes = [type]
        panel.canCreateDirectories = true
        if let directory { panel.directoryURL = directory }
        return panel.runModal() == .OK ? panel.url : nil
    }

    static func unsavedChanges(documentName: String) -> UnsavedChoice {
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = "Do you want to save the changes made to “\(documentName)”?"
        alert.informativeText = "Your changes will be lost if you don’t save them."
        alert.addButton(withTitle: "Save")
        alert.addButton(withTitle: "Cancel")
        let discard = alert.addButton(withTitle: "Don’t Save")
        discard.hasDestructiveAction = true
        switch alert.runModal() {
        case .alertFirstButtonReturn: return .save
        case .alertThirdButtonReturn: return .discard
        default: return .cancel
        }
    }

    static func missingRecent(fileName: String) -> MissingRecentChoice {
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = "“\(fileName)” could not be found."
        alert.informativeText = "It may have been moved, renamed or deleted, or it may be on a drive that isn’t connected."
        alert.addButton(withTitle: "Locate…")
        alert.addButton(withTitle: "Cancel")
        alert.addButton(withTitle: "Remove from Recents")
        switch alert.runModal() {
        case .alertFirstButtonReturn: return .locate
        case .alertThirdButtonReturn: return .remove
        default: return .cancel
        }
    }

    static func confirm(title: String, message: String, action: String) -> Bool {
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = title
        alert.informativeText = message
        alert.addButton(withTitle: action)
        alert.addButton(withTitle: "Cancel")
        return alert.runModal() == .alertFirstButtonReturn
    }

    /// Shows an error: a plain-language summary first, with the technical
    /// detail available on request.
    static func present(error: Error) {
        let info: UserError
        if case let CoreError.Failure(error: userError) = error {
            // Cancelling is something the user asked for, not a failure.
            if userError.code == "cancelled" { return }
            info = userError
        } else {
            info = UserError(
                code: "unexpected",
                title: "Something went wrong",
                message: "VocalScope ran into an unexpected problem.",
                suggestion: "If this keeps happening, please report it and include the technical details.",
                details: String(describing: error)
            )
        }

        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = info.title
        alert.informativeText = [info.message, info.suggestion].compactMap { $0 }.joined(separator: "\n\n")
        alert.addButton(withTitle: "OK")
        if info.details != nil { alert.addButton(withTitle: "Show Technical Details") }

        let show: (NSAlert) -> NSApplication.ModalResponse = { alert in alert.runModal() }
        guard show(alert) == .alertSecondButtonReturn, let details = info.details else { return }

        let detail = NSAlert()
        detail.alertStyle = .informational
        detail.messageText = info.title
        detail.informativeText = "Technical details (\(info.code))"
        detail.accessoryView = detailsView(text: details)
        detail.addButton(withTitle: "OK")
        detail.addButton(withTitle: "Copy")
        if show(detail) == .alertSecondButtonReturn {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString("\(info.title)\n\(info.message)\n\(info.code): \(details)", forType: .string)
        }
    }

    private static func detailsView(text: String) -> NSView {
        let scroll = NSTextView.scrollableTextView()
        scroll.frame = NSRect(x: 0, y: 0, width: 420, height: 120)
        scroll.borderType = .bezelBorder
        if let textView = scroll.documentView as? NSTextView {
            textView.string = text
            textView.isEditable = false
            textView.font = .monospacedSystemFont(ofSize: NSFont.smallSystemFontSize, weight: .regular)
            textView.textContainerInset = NSSize(width: 4, height: 6)
        }
        return scroll
    }
}
