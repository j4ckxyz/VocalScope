import AppKit
import QuartzCore
import SwiftUI
import VocalScopeCore

/// What the timeline draws about pitch, gathered from the model so the view
/// can tell cheaply whether any of it changed.
struct PitchLayer: Equatable {
    let notes: [Note]
    /// Vertical range, as MIDI note numbers.
    let low: Double
    let high: Double
    /// Passages where the compared recording's pitch differs, on this
    /// recording's timeline.
    let differences: [ClosedRange<Double>]
    /// Another recording's curve is to be drawn alongside.
    let compared: Bool
    /// Changes whenever the analysis or the comparison behind this does.
    let key: String

    static func == (a: PitchLayer, b: PitchLayer) -> Bool { a.key == b.key }
}

/// The timeline: an overview of the whole recording above a time ruler, the
/// pitch (curve, notes and a note grid) and the waveform.
///
/// Rendering strategy
/// * The waveform and pitch are drawn only when the visible range, the size
///   or the data changes — never per animation frame.
/// * The playhead is a separate Core Animation layer, moved by a display
///   link, so playback animates without redrawing anything else.
/// * Peak and pitch data for exactly the visible range come from the core,
///   already reduced to about one value per pixel. Those calls take a few
///   hundred microseconds, so they are made synchronously whenever the view
///   changes.
///
/// All geometry (zoom, pan, follow, ruler ticks) is computed by the shared
/// core, so every platform's timeline behaves identically.
final class TimelineCanvas: NSView {
    private enum Metrics {
        static let overviewHeight: CGFloat = 44
        static let rulerHeight: CGFloat = 22
        static let minLabelSpacing = 90.0
        /// Share of the area under the ruler given to the waveform while the
        /// pitch is shown above it, and the least it may get.
        static let waveShare: CGFloat = 0.24
        static let waveMinHeight: CGFloat = 54
        static let noteLabelMinWidth: CGFloat = 34
    }

    private let core: AppCore
    private weak var model: AppModel?

    private var recordingId: String?
    private var duration: Double = 0
    private var ready = false
    private var pitch: PitchLayer?
    private(set) var timeView = TimeView(start: 0, span: 1)
    /// The range to show when the next recording appears, in place of
    /// fitting all of it. Set just before switching between two compared
    /// recordings so the same music stays on screen.
    var pendingView: TimeView?

    private var peaks: [Int16] = []
    private var overviewPeaks: [Int16] = []
    private var overviewColumns = 0
    private var curve: PitchCurve?
    private var comparedCurve: PitchCurve?

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

    private static let smallFont = NSFont.monospacedDigitSystemFont(ofSize: 10, weight: .regular)
    private static let noteFont = NSFont.systemFont(ofSize: 9, weight: .medium)

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
    /// Everything under the ruler.
    private var contentRect: CGRect {
        let top = Metrics.overviewHeight + Metrics.rulerHeight
        return CGRect(x: 0, y: top, width: bounds.width, height: max(0, bounds.height - top))
    }
    private var waveHeight: CGFloat {
        guard pitch != nil else { return contentRect.height }
        return min(contentRect.height, max(Metrics.waveMinHeight, contentRect.height * Metrics.waveShare))
    }
    /// The waveform: all of the content area, or its lower part while the
    /// pitch is shown.
    private var waveRect: CGRect {
        let content = contentRect
        return CGRect(x: 0, y: content.maxY - waveHeight, width: content.width, height: waveHeight)
    }
    private var pitchRect: CGRect {
        let content = contentRect
        guard pitch != nil else { return .zero }
        return CGRect(x: 0, y: content.minY, width: content.width, height: max(0, content.height - waveHeight))
    }

    // MARK: - Configuration from SwiftUI

    /// Called whenever the session changes.
    func configure(recordingId: String, duration: Double, ready: Bool, pitch: PitchLayer?) {
        let changedRecording = recordingId != self.recordingId
        let becameReady = ready && !self.ready
        let hadDuration = self.duration > 0
        let changedPitch = pitch != self.pitch
        self.recordingId = recordingId
        self.duration = duration
        self.ready = ready
        self.pitch = pitch

        if changedRecording, let pending = pendingView {
            following = true
            timeView = timelineClamp(view: pending, duration: duration)
        } else if changedRecording || !hadDuration {
            following = true
            timeView = timelineFit(duration: duration)
        } else {
            timeView = timelineClamp(view: timeView, duration: duration)
        }
        if changedRecording { pendingView = nil }
        if changedRecording || becameReady || !ready {
            overviewColumns = 0
        }
        if changedRecording || changedPitch {
            hoverLine.isHidden = true
            hoverLabel.isHidden = true
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

    /// Brings a stretch of the recording into view with some room around it.
    func reveal(from start: Double, to end: Double) {
        guard duration > 0 else { return }
        let span = max((end - start) * 3, 4)
        following = false
        setView(timelineClamp(view: TimeView(start: (start + end) / 2 - span / 2, span: span), duration: duration))
    }

    /// Shows exactly the given stretch of the recording.
    func show(from start: Double, to end: Double) {
        guard duration > 0, end > start else { return }
        following = false
        setView(timelineClamp(view: TimeView(start: start, span: end - start), duration: duration))
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
            curve = nil
            comparedCurve = nil
            needsDisplay = true
            layoutOverlays()
            return
        }
        let end = timeView.start + timeView.span
        let columns = Int((bounds.width * scale).rounded())
        peaks = core.waveformPeaks(
            recordingId: recordingId, startSeconds: timeView.start, endSeconds: end, columns: UInt32(columns))
        if overviewColumns != columns {
            overviewColumns = columns
            overviewPeaks = core.waveformPeaks(
                recordingId: recordingId, startSeconds: 0, endSeconds: duration, columns: UInt32(columns))
        }
        if let pitch {
            curve = core.pitchCurve(
                recordingId: recordingId, startSeconds: timeView.start, endSeconds: end, columns: UInt32(columns))
            comparedCurve = pitch.compared
                ? core.comparisonCurve(
                    recordingId: recordingId, startSeconds: timeView.start, endSeconds: end, columns: UInt32(columns))
                : nil
        } else {
            curve = nil
            comparedCurve = nil
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
        drawPitch(context: context)

        let wave = waveRect
        if pitch != nil {
            NSColor.windowBackgroundColor.withAlphaComponent(0.5).setFill()
            wave.fill()
        }
        NSColor.separatorColor.setFill()
        CGRect(x: 0, y: wave.midY, width: wave.width, height: 1 / scale).fill()
        if !peaks.isEmpty {
            // Neutral on purpose: the accent colour is reserved for the
            // playhead and the pitch curve.
            context.setFillColor(NSColor.secondaryLabelColor.cgColor)
            fillColumns(peaks, in: wave.insetBy(dx: 0, dy: pitch == nil ? 6 : 4), context: context)
        }

        NSColor.separatorColor.setFill()
        CGRect(x: 0, y: overviewRect.maxY - 1, width: bounds.width, height: 1).fill()
        CGRect(x: 0, y: rulerRect.maxY - 1, width: bounds.width, height: 1).fill()
        if pitch != nil {
            CGRect(x: 0, y: wave.minY, width: bounds.width, height: 1).fill()
        }
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
            .font: Self.smallFont,
            .foregroundColor: NSColor.secondaryLabelColor,
        ]
        let rect = rulerRect
        let content = contentRect
        let hairline = 1 / scale
        for tick in ruler.ticks {
            let x = (xPosition(of: tick.time) * scale).rounded() / scale
            if let label = tick.label {
                NSColor.quaternaryLabelColor.withAlphaComponent(0.08).setFill()
                CGRect(x: x, y: content.minY, width: hairline, height: content.height).fill()
                NSColor.tertiaryLabelColor.setFill()
                CGRect(x: x, y: rect.maxY - 8, width: hairline, height: 8).fill()
                (label as NSString).draw(at: CGPoint(x: x + 4, y: rect.minY + 4), withAttributes: attributes)
            } else {
                NSColor.quaternaryLabelColor.setFill()
                CGRect(x: x, y: rect.maxY - 4, width: hairline, height: 4).fill()
            }
        }
    }

    // MARK: - Pitch

    private func yPosition(ofMidi midi: Double, in rect: CGRect, layer: PitchLayer) -> CGFloat {
        rect.maxY - CGFloat((midi - layer.low) / (layer.high - layer.low)) * rect.height
    }

    private func drawPitch(context: CGContext) {
        guard let pitch, pitch.high > pitch.low else { return }
        let rect = pitchRect
        guard rect.height > 20, timeView.span > 0 else { return }
        let hairline = 1 / scale
        let semitone = rect.height / CGFloat(pitch.high - pitch.low)

        context.saveGState()
        context.clip(to: rect)

        // The note grid: every semitone, the Cs a little stronger, with as
        // many names as there is room for.
        let labelAttributes: [NSAttributedString.Key: Any] = [
            .font: Self.noteFont,
            .foregroundColor: NSColor.secondaryLabelColor,
        ]
        let naturals: Set<Int> = [0, 2, 4, 5, 7, 9, 11]
        for note in Int(pitch.low.rounded(.up))...Int(pitch.high.rounded(.down)) {
            let y = (yPosition(ofMidi: Double(note), in: rect, layer: pitch) * scale).rounded() / scale
            let pitchClass = ((note % 12) + 12) % 12
            let isC = pitchClass == 0
            NSColor.separatorColor.withAlphaComponent(isC ? 0.9 : (naturals.contains(pitchClass) ? 0.45 : 0.2)).setFill()
            CGRect(x: 0, y: y, width: rect.width, height: hairline).fill()
            let labelled = semitone >= 12 ? true : (semitone >= 7 ? naturals.contains(pitchClass) : isC)
            if labelled, y - 6 > rect.minY, y + 6 < rect.maxY, note >= 0, note <= 127 {
                (noteName(UInt8(note)) as NSString).draw(at: CGPoint(x: 4, y: y - 6), withAttributes: labelAttributes)
            }
        }

        // Where the compared recording differs.
        if !pitch.differences.isEmpty {
            NSColor.systemOrange.withAlphaComponent(0.12).setFill()
            for range in pitch.differences {
                let from = xPosition(of: range.lowerBound)
                let to = xPosition(of: range.upperBound)
                if to >= 0, from <= rect.width {
                    CGRect(x: from, y: rect.minY, width: max(1, to - from), height: rect.height).fill()
                }
            }
        }

        // Notes, as bars at their centre pitch.
        let viewEnd = timeView.start + timeView.span
        let barHeight = min(max(semitone * 0.7, 3), 12)
        let noteAttributes: [NSAttributedString.Key: Any] = [
            .font: Self.noteFont,
            .foregroundColor: NSColor.secondaryLabelColor,
        ]
        let bars = CGMutablePath()
        var labels: [(String, CGPoint)] = []
        for note in pitch.notes where note.endSeconds >= timeView.start && note.startSeconds <= viewEnd {
            let from = xPosition(of: note.startSeconds)
            let to = xPosition(of: note.endSeconds)
            let y = yPosition(ofMidi: Double(note.midiPitch), in: rect, layer: pitch)
            let bar = CGRect(x: from, y: y - barHeight / 2, width: max(1, to - from), height: barHeight)
            bars.addRoundedRect(in: bar, cornerWidth: min(3, bar.width / 2), cornerHeight: min(3, barHeight / 2))
            if bar.width >= Metrics.noteLabelMinWidth {
                let cents = Int(note.deviationCents.rounded())
                let text = cents == 0 ? note.name : "\(note.name) \(cents > 0 ? "+" : "−")\(abs(cents))"
                labels.append((text, CGPoint(x: from + 2, y: bar.minY - 12)))
            }
        }
        context.addPath(bars)
        context.setFillColor(NSColor.controlAccentColor.withAlphaComponent(0.2).cgColor)
        context.fillPath()

        // The curves: the compared recording underneath, this one on top.
        if let comparedCurve {
            strokeCurve(comparedCurve, in: rect, layer: pitch, colour: .systemOrange, width: 1.25, context: context)
        }
        if let curve {
            strokeCurve(curve, in: rect, layer: pitch, colour: .controlAccentColor, width: 1.5, context: context)
        }
        for (text, point) in labels where point.y > rect.minY {
            (text as NSString).draw(at: point, withAttributes: noteAttributes)
        }
        context.restoreGState()
    }

    private func strokeCurve(
        _ curve: PitchCurve, in rect: CGRect, layer: PitchLayer, colour: NSColor, width: CGFloat, context: CGContext
    ) {
        let path = CGMutablePath()
        var drawing = false
        var runLength = 0
        var last = CGPoint.zero
        curve.midi.withUnsafeBufferPointer { values in
            for index in 0..<values.count {
                let midi = values[index]
                guard midi.isFinite else {
                    // A voiced stretch one point long still deserves a mark.
                    if drawing, runLength == 1 { path.addLine(to: CGPoint(x: last.x + 1, y: last.y)) }
                    drawing = false
                    continue
                }
                let time = curve.startSeconds + Double(index) * curve.stepSeconds
                let point = CGPoint(x: xPosition(of: time), y: yPosition(ofMidi: Double(midi), in: rect, layer: layer))
                if drawing {
                    path.addLine(to: point)
                    runLength += 1
                } else {
                    path.move(to: point)
                    drawing = true
                    runLength = 1
                }
                last = point
            }
        }
        context.addPath(path)
        context.setStrokeColor(colour.cgColor)
        context.setLineWidth(width)
        context.setLineJoin(.round)
        context.setLineCap(.round)
        context.strokePath()
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
        hoverLabel.font = Self.smallFont
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

    /// The time under the pointer and, where there is one, the pitch there.
    private func updateHover(at point: CGPoint) {
        let inside = ready && duration > 0 && point.y >= Metrics.overviewHeight && bounds.contains(point)
        hoverLine.isHidden = !inside
        hoverLabel.isHidden = !inside
        guard inside, let recordingId else { return }
        let top = Metrics.overviewHeight
        hoverLine.frame = CGRect(x: point.x, y: top, width: 1 / scale, height: bounds.height - top)
        // One more digit than the ruler shows at this zoom level.
        let secondsPerPoint = timeView.span / Double(bounds.width)
        let decimals: UInt32 = secondsPerPoint < 0.002 ? 3 : (secondsPerPoint < 0.2 ? 2 : 1)
        let seconds = time(atX: point.x)
        var text = formatTime(seconds: seconds, decimals: decimals)
        if pitch != nil, let reading = core.pitchAt(recordingId: recordingId, seconds: seconds) {
            let cents = Int(reading.deviationCents.rounded())
            let offset = cents == 0 ? "" : " \(cents > 0 ? "+" : "−")\(abs(cents))¢"
            let hertz = Double(reading.frequencyHz).formatted(.number.precision(.fractionLength(1)))
            text += "   \(reading.noteName)\(offset)   \(hertz) Hz"
        }
        hoverLabel.string = text
        let labelWidth = ceil((text as NSString).size(withAttributes: [.font: Self.smallFont]).width) + 12
        let labelX = point.x + labelWidth + 10 > bounds.width ? point.x - labelWidth - 6 : point.x + 6
        hoverLabel.frame = CGRect(x: labelX, y: contentRect.minY + 6, width: labelWidth, height: 15)
    }
}

/// The name of a MIDI note, e.g. 69 is A4.
private func noteName(_ note: UInt8) -> String {
    let names = ["C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B"]
    return "\(names[Int(note) % 12])\(Int(note) / 12 - 1)"
}

/// Hosts the AppKit timeline in SwiftUI.
struct WaveformTimeline: NSViewRepresentable {
    @EnvironmentObject private var model: AppModel
    let recordingId: String
    let duration: Double
    let ready: Bool
    let pitch: PitchLayer?
    /// Changes whenever the transport does, prompting `updateNSView`.
    let playback: PlaybackStatus

    func makeNSView(context: Context) -> TimelineCanvas {
        TimelineCanvas(core: model.core, model: model)
    }

    func updateNSView(_ canvas: TimelineCanvas, context: Context) {
        canvas.configure(recordingId: recordingId, duration: duration, ready: ready, pitch: pitch)
    }
}
