//! Timeline geometry shared by every UI: the visible time window, zooming,
//! panning, following the playhead, ruler ticks and time formatting.
//!
//! Keeping this in the core means the macOS and Windows timelines behave
//! identically without either re-implementing (and re-testing) the maths.

/// The visible portion of a recording.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct TimeView {
    /// Time at the left edge, in seconds.
    pub start: f64,
    /// Visible length, in seconds.
    pub span: f64,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct RulerTick {
    pub time: f64,
    /// Formatted time for labelled (major) ticks; `None` for minor ticks.
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct Ruler {
    /// Spacing between labelled ticks, in seconds.
    pub major_step: f64,
    /// Fractional-second digits used in the labels.
    pub decimals: u32,
    pub ticks: Vec<RulerTick>,
}

/// Closest zoom: 20 ms across the whole view.
pub const MIN_SPAN_SECONDS: f64 = 0.02;
/// When paging to follow the playhead, it lands this far in from the left.
const FOLLOW_MARGIN: f64 = 0.05;

/// "Nice" label spacings in seconds, from 1 ms up to an hour.
const STEPS: [f64; 22] = [
    0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0,
    120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0,
];

/// Keeps a view inside `[0, duration]` and within the zoom limits.
#[uniffi::export]
pub fn timeline_clamp(view: TimeView, duration: f64) -> TimeView {
    if !(duration > 0.0) {
        return TimeView {
            start: 0.0,
            span: 1.0,
        };
    }
    let min_span = MIN_SPAN_SECONDS.min(duration);
    let span = if view.span.is_finite() {
        view.span.clamp(min_span, duration)
    } else {
        duration
    };
    let start = if view.start.is_finite() {
        view.start.clamp(0.0, duration - span)
    } else {
        0.0
    };
    TimeView { start, span }
}

/// The view that shows the whole recording.
#[uniffi::export]
pub fn timeline_fit(duration: f64) -> TimeView {
    timeline_clamp(
        TimeView {
            start: 0.0,
            span: duration,
        },
        duration,
    )
}

/// Zooms by `factor` (>1 zooms in) keeping `anchor` — a time, typically under
/// the pointer — at the same horizontal position.
#[uniffi::export]
pub fn timeline_zoom(view: TimeView, factor: f64, anchor: f64, duration: f64) -> TimeView {
    if !(factor > 0.0) || !factor.is_finite() {
        return view;
    }
    let zoomed = timeline_clamp(
        TimeView {
            start: view.start,
            span: view.span / factor,
        },
        duration,
    );
    let ratio = if view.span > 0.0 {
        (anchor - view.start) / view.span
    } else {
        0.5
    };
    timeline_clamp(
        TimeView {
            start: anchor - ratio * zoomed.span,
            span: zoomed.span,
        },
        duration,
    )
}

#[uniffi::export]
pub fn timeline_pan(view: TimeView, delta_seconds: f64, duration: f64) -> TimeView {
    timeline_clamp(
        TimeView {
            start: view.start + delta_seconds,
            span: view.span,
        },
        duration,
    )
}

/// Centres the view on `time`, keeping the zoom level.
#[uniffi::export]
pub fn timeline_centre(view: TimeView, time: f64, duration: f64) -> TimeView {
    timeline_clamp(
        TimeView {
            start: time - view.span / 2.0,
            span: view.span,
        },
        duration,
    )
}

/// While playing, pages the view forward when the playhead runs off the right
/// edge, and brings it back into view if it is somewhere else entirely.
#[uniffi::export]
pub fn timeline_follow(view: TimeView, position: f64, duration: f64) -> TimeView {
    let visible = position >= view.start && position < view.start + view.span;
    // Nothing to follow when the playhead is visible or everything is shown.
    if visible || view.span >= duration {
        return view;
    }
    timeline_clamp(
        TimeView {
            start: position - view.span * FOLLOW_MARGIN,
            span: view.span,
        },
        duration,
    )
}

fn minor_divisions(step: f64) -> u32 {
    // Split a major interval into tidy parts: 15 s → 3 × 5 s, 30 s → 3 × 10 s,
    // anything starting with 2 → halves, otherwise fifths.
    if [15.0, 30.0, 900.0, 1800.0].contains(&step) {
        return 3;
    }
    let leading = step / 10f64.powf(step.log10().floor());
    if (leading - 2.0).abs() < 1e-6 {
        2
    } else {
        5
    }
}

/// Chooses tick positions so labels are at least `min_label_spacing` points
/// apart in a view `width` points wide.
#[uniffi::export]
pub fn timeline_ruler(view: TimeView, width: f64, min_label_spacing: f64) -> Ruler {
    let seconds_per_point = if width > 0.0 {
        view.span / width
    } else {
        view.span
    };
    let wanted = seconds_per_point * min_label_spacing;
    let major_step = STEPS
        .iter()
        .copied()
        .find(|step| *step >= wanted)
        .unwrap_or_else(|| (wanted / 3600.0).ceil() * 3600.0);
    let divisions = minor_divisions(major_step);
    let minor_step = major_step / divisions as f64;
    let decimals = if major_step >= 1.0 {
        0
    } else {
        ((-major_step.log10() - 1e-9).ceil() as u32).min(3)
    };

    let first = (view.start / minor_step - 1e-9).ceil() as i64;
    let last = ((view.start + view.span) / minor_step + 1e-9).floor() as i64;
    // Index arithmetic avoids accumulating floating-point error.
    let ticks = (first..=last)
        .map(|index| {
            let time = index as f64 * minor_step;
            let major = index.rem_euclid(divisions as i64) == 0;
            RulerTick {
                time,
                label: major.then(|| format_time(time, decimals)),
            }
        })
        .collect();
    Ruler {
        major_step,
        decimals,
        ticks,
    }
}

/// Formats a position or duration as `m:ss`, `m:ss.mmm` or `h:mm:ss(.mmm)`.
/// `decimals` is the number of fractional-second digits (0–3).
#[uniffi::export]
pub fn format_time(seconds: f64, decimals: u32) -> String {
    if !seconds.is_finite() {
        return "—".to_string();
    }
    let decimals = decimals.min(3);
    let scale = 10u64.pow(decimals);
    // Round once, up front, so 59.9996 becomes 1:00.000 rather than 0:60.000.
    let total = (seconds.abs() * scale as f64).round() as u64;
    let sign = if seconds < 0.0 && total > 0 { "-" } else { "" };
    let fraction = total % scale;
    let whole = total / scale;
    let (h, m, s) = (whole / 3600, (whole / 60) % 60, whole % 60);
    let tail = if decimals > 0 {
        format!(".{fraction:0width$}", width = decimals as usize)
    } else {
        String::new()
    };
    if h > 0 {
        format!("{sign}{h}:{m:02}:{s:02}{tail}")
    } else {
        format!("{sign}{m}:{s:02}{tail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(start: f64, span: f64) -> TimeView {
        TimeView { start, span }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn clamp_keeps_the_view_inside_the_recording() {
        assert_eq!(timeline_clamp(view(-5.0, 10.0), 60.0), view(0.0, 10.0));
        assert_eq!(timeline_clamp(view(55.0, 10.0), 60.0), view(50.0, 10.0));
        assert_eq!(timeline_clamp(view(10.0, 500.0), 60.0), view(0.0, 60.0));
        assert_eq!(timeline_clamp(view(1.0, 1e-9), 60.0).span, MIN_SPAN_SECONDS);
    }

    #[test]
    fn clamp_copes_with_tiny_missing_and_nonsense_values() {
        assert_eq!(timeline_clamp(view(0.0, 1.0), 0.005), view(0.0, 0.005));
        assert_eq!(timeline_clamp(view(3.0, 2.0), 0.0), view(0.0, 1.0));
        assert_eq!(
            timeline_clamp(view(f64::NAN, f64::NAN), 60.0),
            view(0.0, 60.0)
        );
        assert_eq!(timeline_fit(42.0), view(0.0, 42.0));
    }

    #[test]
    fn zoom_keeps_the_anchor_under_the_pointer() {
        let zoomed = timeline_zoom(view(10.0, 20.0), 2.0, 15.0, 100.0);
        assert_eq!(zoomed.span, 10.0);
        assert!(close((15.0 - zoomed.start) / zoomed.span, 0.25));
    }

    #[test]
    fn zoom_is_bounded_reversible_and_ignores_bad_factors() {
        assert_eq!(
            timeline_zoom(view(10.0, 20.0), 0.01, 15.0, 100.0),
            view(0.0, 100.0)
        );

        let original = view(30.0, 12.0);
        let back = timeline_zoom(timeline_zoom(original, 4.0, 33.0, 100.0), 0.25, 33.0, 100.0);
        assert!(close(back.start, original.start) && close(back.span, original.span));

        assert_eq!(timeline_zoom(original, 0.0, 1.0, 100.0), original);
        assert_eq!(timeline_zoom(original, -3.0, 1.0, 100.0), original);
        assert_eq!(timeline_zoom(original, f64::INFINITY, 1.0, 100.0), original);
    }

    #[test]
    fn pan_and_centre_stay_in_bounds() {
        assert_eq!(timeline_pan(view(10.0, 10.0), 5.0, 60.0), view(15.0, 10.0));
        assert_eq!(timeline_pan(view(10.0, 10.0), -50.0, 60.0), view(0.0, 10.0));
        assert_eq!(
            timeline_pan(view(10.0, 10.0), 500.0, 60.0),
            view(50.0, 10.0)
        );
        assert_eq!(
            timeline_centre(view(0.0, 10.0), 30.0, 60.0),
            view(25.0, 10.0)
        );
        assert_eq!(
            timeline_centre(view(0.0, 10.0), 59.0, 60.0),
            view(50.0, 10.0)
        );
    }

    #[test]
    fn follow_pages_only_when_the_playhead_leaves() {
        let current = view(10.0, 10.0);
        assert_eq!(timeline_follow(current, 12.0, 100.0), current);
        assert_eq!(timeline_follow(current, 19.99, 100.0), current);
        assert!(close(timeline_follow(current, 20.0, 100.0).start, 19.5));
        assert!(close(timeline_follow(current, 2.0, 100.0).start, 1.5));
        assert_eq!(timeline_follow(current, 0.0, 100.0).start, 0.0);
        let everything = view(0.0, 100.0);
        assert_eq!(timeline_follow(everything, 100.0, 100.0), everything);
    }

    #[test]
    fn ruler_spaces_labels_sensibly() {
        // 60 s across 900 points: 90 points is 6 s, so the next nice step is 10 s.
        let ruler = timeline_ruler(view(0.0, 60.0), 900.0, 90.0);
        assert_eq!(ruler.major_step, 10.0);
        assert_eq!(ruler.decimals, 0);
        let labels: Vec<_> = ruler.ticks.iter().filter_map(|t| t.label.clone()).collect();
        assert_eq!(
            labels,
            ["0:00", "0:10", "0:20", "0:30", "0:40", "0:50", "1:00"]
        );
        assert_eq!(ruler.ticks.iter().filter(|t| t.label.is_none()).count(), 24);
        assert!(ruler.ticks[0].time == 0.0 && ruler.ticks[0].time.is_sign_positive());
    }

    #[test]
    fn ruler_uses_matching_precision_when_zoomed_in() {
        let ruler = timeline_ruler(view(12.3, 0.5), 1000.0, 90.0);
        assert_eq!(ruler.major_step, 0.05);
        assert_eq!(ruler.decimals, 2);
        assert!(ruler
            .ticks
            .iter()
            .all(|t| t.time >= 12.3 - 1e-6 && t.time <= 12.8 + 1e-6));
        assert!(ruler
            .ticks
            .iter()
            .any(|t| t.label.as_deref() == Some("0:12.50")));
    }

    #[test]
    fn ruler_divides_steps_tidily_and_survives_long_recordings() {
        let ruler = timeline_ruler(view(0.0, 180.0), 1000.0, 90.0);
        assert_eq!(ruler.major_step, 30.0);
        let first: Vec<_> = ruler.ticks[..4]
            .iter()
            .map(|t| (t.time, t.label.is_some()))
            .collect();
        assert_eq!(
            first,
            [(0.0, true), (10.0, false), (20.0, false), (30.0, true)]
        );

        assert_eq!(
            timeline_ruler(view(0.0, 20.0), 1000.0, 90.0).major_step,
            2.0
        );
        assert_eq!(minor_divisions(2.0), 2);
        assert_eq!(minor_divisions(0.02), 2);
        assert_eq!(minor_divisions(0.5), 5);

        let long = timeline_ruler(view(0.0, 40.0 * 3600.0), 800.0, 90.0);
        assert_eq!(long.major_step % 3600.0, 0.0);
        assert!(long.ticks.len() < 200);
    }

    #[test]
    fn formats_times() {
        assert_eq!(format_time(0.0, 0), "0:00");
        assert_eq!(format_time(65.0, 0), "1:05");
        assert_eq!(format_time(599.4, 0), "9:59");
        assert_eq!(format_time(3600.0, 0), "1:00:00");
        assert_eq!(format_time(11229.0, 0), "3:07:09");
        assert_eq!(format_time(12.43, 3), "0:12.430");
        assert_eq!(format_time(12.43, 1), "0:12.4");
        assert_eq!(format_time(3725.5, 2), "1:02:05.50");
    }

    #[test]
    fn time_formatting_carries_rounding_and_handles_odd_input() {
        assert_eq!(format_time(59.9996, 3), "1:00.000");
        assert_eq!(format_time(59.6, 0), "1:00");
        assert_eq!(format_time(3599.9999, 2), "1:00:00.00");
        assert_eq!(format_time(-1.5, 1), "-0:01.5");
        assert_eq!(format_time(-0.0001, 1), "0:00.0");
        assert_eq!(format_time(f64::NAN, 2), "—");
        assert_eq!(format_time(1.0, 9), "0:01.000");
    }
}
