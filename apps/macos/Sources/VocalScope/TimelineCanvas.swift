import AppKit
import QuartzCore
import SwiftUI
import VocalScopeCore

/// The timeline: an overview of the whole recording above a zoomable
/// waveform with a time ruler.
///
/// Rendering strategy
/// * The waveform is drawn only when the visible range, the size or the data
///   changes — never per animation frame.
/// * The playhead is a separate Core Animation layer, moved by a display
///   link, so playback animates without redrawing the waveform.
/// * Peak data for exactly the visible range comes from the core at one
///   column per device pixel. That call is a few hundred microseconds, so it
///   is made synchronously whenever the view changes.
///
/// All geometry (zoom, pan, follow, ruler ticks) is computed by the shared
/// core, so every platform's timeline behaves identically.
final class TimelineCanvas: NSView {
    private enum Metrics {
        static let overviewHeight: CGFloat = 44
        static let rulerHeight: CGFloat = 22
        static let minLabelSpacing = 90.0
    }

    private let core: AppCore
    private weak var model: AppModel?

    private var recordingId: String?
    private var duration: Double = 0
    private var ready = false
    private(set) var timeView = TimeView(start: 0, span: 1)

    private var peaks: [Int16] = []
    private var overviewPeaks: [Int16] = []
    private var overviewColumns = 0

    private let playhead = CALayer()
    private let overviewPlayhead = CALayer()
    private let viewport = CALayer()
    private let hoverLine = CALayer()
    private let hoverLabel = CATextLayer()

    private var frameLink: CADisplayLink?
    private var following = true
    private var scrubbing = false
    private var draggingOverview = false
    // Smooths the playhead between the engine's position updates.
    private var lastReportedPosition = 0.0
    private var lastReportedAt = CACurrentMediaTime()

    init(core: AppCore, model: AppModel) {
        self.core = core
        self.model = model
        super.init(frame: .zero)
        wantsLayer = true
        layerContentsRedrawPolicy = .onSetNeedsDisplay
        for sublayer in [viewport, overviewPlayhead, playhead, hoverLine, hoverLabel] {
            sublayer.actions = ["position": NSNull(), "bounds": NSNull(), "hidden": NSNull(), "contents": NSNull()]
            layer?.addSublayer(sublayer)
        }
        viewport.borderWidth = 1
        hoverLine.isHidden = true
        hoverLabel.isHidden = true
        hoverLabel.fontSize = 10
        hoverLabel.alignmentMode = .center
        hoverLabel.cornerRadius = 3
        setAccessibilityElement(true)
        setAccessibilityRole(.slider)
        setAccessibilityLabel("Timeline")
        updateTrackingAreas()
        applyLayerColours()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not used") }

    override var isFlipped: Bool { true }
    override var isOpaque: Bool { true }
    override var acceptsFirstResponder: Bool { true }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    private var scale: CGFloat { window?.backingScaleFactor ?? 2 }
    private var overviewRect: CGRect { CGRect(x: 0, y: 0, width: bounds.width, height: Metrics.overviewHeight) }
    private var rulerRect: CGRect {
        CGRect(x: 0, y: Metrics.overviewHeight, width: bounds.width, height: Metrics.rulerHeight)
    }
    private var waveRect: CGRect {
        let top = Metrics.overviewHeight + Metrics.rulerHeight
        return CGRect(x: 0, y: top, width: bounds.width, height: max(0, bounds.height - top))
    }

    // MARK: - Configuration from SwiftUI

    /// Called whenever the session changes.
    func configure(recordingId: String, duration: Double, ready: Bool) {
        let changedRecording = recordingId != self.recordingId
        let becameReady = ready && !self.ready
        let hadDuration = self.duration > 0
        self.recordingId = recordingId
        self.duration = duration
        self.ready = ready

        if changedRecording || !hadDuration {
            following = true
            timeView = timelineFit(duration: duration)
        } else {
            timeView = timelineClamp(view: timeView, duration: duration)
        }
        if changedRecording || becameReady || !ready {
            overviewColumns = 0
        }
        reload()
        playbackChanged()
    }

    /// Called when the transport state changes.
    func playbackChanged() {
        let status = core.playbackStatus()
        lastReportedPosition = status.positionSeconds
        lastReportedAt = CACurrentMediaTime()
        placePlayhead(at: status.positionSeconds)
        let playing = status.state == .playing
        if playing { following = true }
        frameLink?.isPaused = !playing
    }

    // MARK: - Zoom commands (toolbar and menu)

    func zoom(by factor: Double) {
        guard duration > 0 else { return }
        let position = core.playbackStatus().positionSeconds
        let visible = position >= timeView.start && position <= timeView.start + timeView.span
        let anchor = visible ? position : timeView.start + timeView.span / 2
        setView(timelineZoom(view: timeView, factor: factor, anchor: anchor, duration: duration))
    }

    func zoomToFit() {
        setView(timelineFit(duration: duration))
    }

    private func setView(_ view: TimeView) {
        guard view != timeView else { return }
        timeView = view
        reload()
        placePlayhead(at: currentPosition())
    }

    // MARK: - Data

    private func reload() {
        guard ready, let recordingId, duration > 0, bounds.width > 0 else {
            peaks = []
            overviewPeaks = []
            needsDisplay = true
            layoutOverlays()
            return
        }
        let columns = Int((bounds.width * scale).rounded())
        peaks = core.waveformPeaks(
            recordingId: recordingId,
            startSeconds: timeView.start,
            endSeconds: timeView.start + timeView.span,
            columns: UInt32(columns)
        )
        if overviewColumns != columns {
            overviewColumns = columns
            overviewPeaks = core.waveformPeaks(
                recordingId: recordingId, startSeconds: 0, endSeconds: duration, columns: UInt32(columns))
        }
        needsDisplay = true
        layoutOverlays()
    }

    // MARK: - Drawing

    override func draw(_ dirtyRect: NSRect) {
        guard let context = NSGraphicsContext.current?.cgContext else { return }

        NSColor.textBackgroundColor.setFill()
        bounds.fill()
        NSColor.windowBackgroundColor.setFill()
        overviewRect.fill()
        rulerRect.fill()

        if !overviewPeaks.isEmpty {
            context.setFillColor(NSColor.tertiaryLabelColor.cgColor)
            fillColumns(overviewPeaks, in: overviewRect.insetBy(dx: 0, dy: 3), context: context)
        }
        drawRuler(context: context)

        let wave = waveRect
        NSColor.separatorColor.setFill()
        CGRect(x: 0, y: wave.midY, width: wave.width, height: 1 / scale).fill()
        if !peaks.isEmpty {
            // Neutral on purpose: the accent colour is reserved for the
            // playhead and, from v0.2, the pitch curve drawn over this.
            context.setFillColor(NSColor.secondaryLabelColor.cgColor)
            fillColumns(peaks, in: wave.insetBy(dx: 0, dy: 6), context: context)
        }

        NSColor.separatorColor.setFill()
        CGRect(x: 0, y: overviewRect.maxY - 1, width: bounds.width, height: 1).fill()
        CGRect(x: 0, y: rulerRect.maxY - 1, width: bounds.width, height: 1).fill()
    }

    /// Fills one min-to-max bar per column, all in a single path.
    private func fillColumns(_ values: [Int16], in rect: CGRect, context: CGContext) {
        let count = values.count / 2
        guard count > 0, rect.height > 0 else { return }
        let columnWidth = rect.width / CGFloat(count)
        let mid = rect.midY
        let amplitude = rect.height / 2
        let hairline = 1 / scale
        let path = CGMutablePath()
        values.withUnsafeBufferPointer { buffer in
            for column in 0..<count {
                let low = CGFloat(buffer[column * 2]) / 32767
                let high = CGFloat(buffer[column * 2 + 1]) / 32767
                let top = mid - high * amplitude
                let height = max(hairline, (high - low) * amplitude)
                path.addRect(CGRect(x: rect.minX + CGFloat(column) * columnWidth, y: top, width: columnWidth, height: height))
            }
        }
        context.addPath(path)
        context.fillPath()
    }

    private func drawRuler(context: CGContext) {
        guard duration > 0, bounds.width > 0 else { return }
        let ruler = timelineRuler(view: timeView, width: Double(bounds.width), minLabelSpacing: Metrics.minLabelSpacing)
        let attributes: [NSAttributedString.Key: Any] = [
            .font: NSFont.monospacedDigitSystemFont(ofSize: 10, weight: .regular),
            .foregroundColor: NSColor.secondaryLabelColor,
        ]
        let rect = rulerRect
        let wave = waveRect
        let hairline = 1 / scale
        for tick in ruler.ticks {
            let x = (xPosition(of: tick.time) * scale).rounded() / scale
            if let label = tick.label {
                NSColor.quaternaryLabelColor.withAlphaComponent(0.08).setFill()
                CGRect(x: x, y: wave.minY, width: hairline, height: wave.height).fill()
                NSColor.tertiaryLabelColor.setFill()
                CGRect(x: x, y: rect.maxY - 8, width: hairline, height: 8).fill()
                (label as NSString).draw(at: CGPoint(x: x + 4, y: rect.minY + 4), withAttributes: attributes)
            } else {
                NSColor.quaternaryLabelColor.setFill()
                CGRect(x: x, y: rect.maxY - 4, width: hairline, height: 4).fill()
            }
        }
    }

    // MARK: - Overlays (no redraw needed)

    private func xPosition(of time: Double) -> CGFloat {
        guard timeView.span > 0 else { return 0 }
        return CGFloat((time - timeView.start) / timeView.span) * bounds.width
    }

    private func time(atX x: CGFloat) -> Double {
        guard bounds.width > 0 else { return timeView.start }
        let time = timeView.start + Double(x / bounds.width) * timeView.span
        return min(max(time, 0), duration)
    }

    private func layoutOverlays() {
        let visible = ready && duration > 0
        // No viewport box when the whole recording is already in view.
        viewport.isHidden = !visible || timeView.span >= duration
        playhead.isHidden = !visible
        overviewPlayhead.isHidden = !visible
        guard visible else { return }
        let width = bounds.width
        let left = CGFloat(timeView.start / duration) * width
        let span = max(3, CGFloat(timeView.span / duration) * width)
        viewport.frame = CGRect(x: left, y: 0, width: span, height: Metrics.overviewHeight - 1)
    }

    private func placePlayhead(at position: Double) {
        guard ready, duration > 0 else { return }
        let x = xPosition(of: position)
        let top = Metrics.overviewHeight
        playhead.isHidden = x < -1 || x > bounds.width + 1
        playhead.frame = CGRect(x: x - 0.75, y: top, width: 1.5, height: bounds.height - top)
        let overviewX = CGFloat(position / duration) * bounds.width
        overviewPlayhead.frame = CGRect(x: overviewX - 0.5, y: 0, width: 1, height: Metrics.overviewHeight - 1)
    }

    private func applyLayerColours() {
        effectiveAppearance.performAsCurrentDrawingAppearance {
            playhead.backgroundColor = NSColor.controlAccentColor.cgColor
            overviewPlayhead.backgroundColor = NSColor.controlAccentColor.cgColor
            viewport.backgroundColor = NSColor.controlAccentColor.withAlphaComponent(0.16).cgColor
            viewport.borderColor = NSColor.controlAccentColor.withAlphaComponent(0.7).cgColor
            hoverLine.backgroundColor = NSColor.tertiaryLabelColor.cgColor
            hoverLabel.backgroundColor = NSColor.windowBackgroundColor.cgColor
            hoverLabel.foregroundColor = NSColor.labelColor.cgColor
        }
        hoverLabel.font = NSFont.monospacedDigitSystemFont(ofSize: 10, weight: .regular)
    }

    // MARK: - View lifecycle

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        frameLink?.invalidate()
        frameLink = nil
        guard window != nil else { return }
        hoverLabel.contentsScale = scale
        let link = displayLink(target: self, selector: #selector(step(_:)))
        link.add(to: .main, forMode: .common)
        link.isPaused = core.playbackStatus().state != .playing
        frameLink = link
        model?.timeline = self
        reload()
    }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        hoverLabel.contentsScale = scale
        overviewColumns = 0
        reload()
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        applyLayerColours()
        needsDisplay = true
    }

    override func setFrameSize(_ newSize: NSSize) {
        let changed = newSize != frame.size
        super.setFrameSize(newSize)
        if changed {
            reload()
            placePlayhead(at: currentPosition())
        }
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        trackingAreas.forEach(removeTrackingArea)
        addTrackingArea(NSTrackingArea(
            rect: .zero,
            options: [.mouseMoved, .mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
            owner: self))
    }

    // MARK: - Playback animation

    /// The engine refreshes its position every few milliseconds; between
    /// refreshes the playhead is extrapolated so it moves at display rate.
    private func currentPosition() -> Double {
        let status = core.playbackStatus()
        guard status.state == .playing else { return status.positionSeconds }
        let now = CACurrentMediaTime()
        if status.positionSeconds != lastReportedPosition {
            lastReportedPosition = status.positionSeconds
            lastReportedAt = now
        }
        let position = lastReportedPosition + min(now - lastReportedAt, 0.1)
        return status.durationSeconds.map { min(position, $0) } ?? position
    }

    @objc private func step(_ link: CADisplayLink) {
        let position = currentPosition()
        let visible = position >= timeView.start && position < timeView.start + timeView.span
        if visible {
            following = true
        } else if following, !scrubbing, !draggingOverview {
            setView(timelineFollow(view: timeView, position: position, duration: duration))
        }
        placePlayhead(at: position)
        if core.playbackStatus().state != .playing { link.isPaused = true }
    }

    // MARK: - Pointer input

    override func mouseDown(with event: NSEvent) {
        guard ready, duration > 0 else { return }
        window?.makeFirstResponder(self)
        let point = convert(event.locationInWindow, from: nil)
        if overviewRect.contains(point) {
            draggingOverview = true
            centreView(atOverviewX: point.x)
        } else {
            scrubbing = true
            following = true
            model?.seek(to: time(atX: point.x))
        }
    }

    override func mouseDragged(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        if draggingOverview {
            centreView(atOverviewX: point.x)
        } else if scrubbing {
            model?.seek(to: time(atX: point.x))
            updateHover(at: point)
        }
    }

    override func mouseUp(with event: NSEvent) {
        scrubbing = false
        draggingOverview = false
    }

    private func centreView(atOverviewX x: CGFloat) {
        guard bounds.width > 0 else { return }
        following = false
        let time = Double(x / bounds.width) * duration
        setView(timelineCentre(view: timeView, time: time, duration: duration))
    }

    override func scrollWheel(with event: NSEvent) {
        guard ready, duration > 0, bounds.width > 0 else { return }
        let point = convert(event.locationInWindow, from: nil)
        let unit: CGFloat = event.hasPreciseScrollingDeltas ? 1 : 12
        if event.modifierFlags.contains(.command) || event.modifierFlags.contains(.option) {
            let factor = exp(Double(event.scrollingDeltaY * unit) * 0.01)
            setView(timelineZoom(view: timeView, factor: factor, anchor: time(atX: point.x), duration: duration))
        } else {
            let dominant = abs(event.scrollingDeltaX) > abs(event.scrollingDeltaY)
                ? event.scrollingDeltaX : event.scrollingDeltaY
            let seconds = -Double(dominant * unit / bounds.width) * timeView.span
            following = false
            setView(timelinePan(view: timeView, deltaSeconds: seconds, duration: duration))
        }
        updateHover(at: point)
    }

    /// Trackpad pinch.
    override func magnify(with event: NSEvent) {
        guard ready, duration > 0 else { return }
        let point = convert(event.locationInWindow, from: nil)
        let factor = 1 + Double(event.magnification)
        setView(timelineZoom(view: timeView, factor: factor, anchor: time(atX: point.x), duration: duration))
    }

    /// Double-tap with two fingers: fit the whole recording.
    override func smartMagnify(with event: NSEvent) {
        zoomToFit()
    }

    override func mouseMoved(with event: NSEvent) {
        updateHover(at: convert(event.locationInWindow, from: nil))
    }

    override func mouseExited(with event: NSEvent) {
        hoverLine.isHidden = true
        hoverLabel.isHidden = true
    }

    private func updateHover(at point: CGPoint) {
        let inside = ready && duration > 0 && point.y >= Metrics.overviewHeight && bounds.contains(point)
        hoverLine.isHidden = !inside
        hoverLabel.isHidden = !inside
        guard inside else { return }
        let top = Metrics.overviewHeight
        hoverLine.frame = CGRect(x: point.x, y: top, width: 1 / scale, height: bounds.height - top)
        // One more digit than the ruler shows at this zoom level.
        let secondsPerPoint = timeView.span / Double(bounds.width)
        let decimals: UInt32 = secondsPerPoint < 0.002 ? 3 : (secondsPerPoint < 0.2 ? 2 : 1)
        hoverLabel.string = formatTime(seconds: time(atX: point.x), decimals: decimals)
        let labelWidth: CGFloat = 62
        let labelX = point.x + labelWidth + 10 > bounds.width ? point.x - labelWidth - 6 : point.x + 6
        hoverLabel.frame = CGRect(x: labelX, y: waveRect.minY + 6, width: labelWidth, height: 15)
    }
}

/// Hosts the AppKit timeline in SwiftUI.
struct WaveformTimeline: NSViewRepresentable {
    @EnvironmentObject private var model: AppModel
    let recordingId: String
    let duration: Double
    let ready: Bool
    /// Changes whenever the transport does, prompting `updateNSView`.
    let playback: PlaybackStatus

    func makeNSView(context: Context) -> TimelineCanvas {
        TimelineCanvas(core: model.core, model: model)
    }

    func updateNSView(_ canvas: TimelineCanvas, context: Context) {
        canvas.configure(recordingId: recordingId, duration: duration, ready: ready)
    }
}
