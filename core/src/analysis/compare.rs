//! Comparing two versions of the same recording.
//!
//! Two steps. First the versions are lined up in time from their loudness
//! envelopes: a single offset (one version has a longer lead-in) and, when it
//! is measurable, a constant speed difference (a tape or vinyl transfer).
//! That is the right model for two releases of one performance; it is not a
//! way to match two different performances, and a poor match is reported as
//! such rather than forced. Then the two pitch curves are compared frame by
//! frame along that alignment.

use realfft::RealFftPlanner;
use serde::Serialize;

use super::Analysis;

/// Spacing of the loudness envelopes handed to [`align`].
pub const ENVELOPE_STEP_SECONDS: f64 = 0.01;

/// The envelopes must overlap by at least this much, and by this share of
/// the shorter one, for an offset to be considered.
const MIN_OVERLAP_SECONDS: f64 = 4.0;
const MIN_OVERLAP_SHARE: f64 = 0.5;
/// Slow level changes (fades, mastering) are removed over this long.
const DETREND_SECONDS: f64 = 0.5;
/// Envelope floor, as a linear amplitude: 60 dB below full scale.
const ENVELOPE_FLOOR: f32 = 1e-3;

/// Windows of the reference located independently in the other recording:
/// an eighth of the recording each, within these limits.
const LOCAL_WINDOW_SECONDS: f64 = 15.0;
const LOCAL_WINDOW_MIN_SECONDS: f64 = 4.0;
const MAX_LOCAL_WINDOWS: usize = 64;
/// A window counts as located when its best fit scores at least this.
const LOCAL_MIN_SCORE: f64 = 0.4;
/// At least this many windows must agree on one line...
const LOCAL_MIN_WINDOWS: usize = 3;
/// ...to within this many frames (50 ms).
const LOCAL_AGREEMENT_FRAMES: f64 = 5.0;
/// How far either side of the first estimate the refining pass looks.
const REFINE_SEARCH_FRAMES: i64 = 12;
/// Speed is judged from pairs of windows at least this far apart.
const LOCAL_SLOPE_BASELINE_SECONDS: f64 = 20.0;
/// Speed differences smaller than this (0.005%) are not distinguishable
/// from none.
const MIN_SPEED_DIFFERENCE: f64 = 5e-5;
/// ...as are ones that shift the recordings by less than this (20 ms) from
/// the first window to the last.
const MIN_DRIFT_FRAMES: f64 = 2.0;
/// The largest speed difference that is believed (3%, more than a semitone
/// would be 5.9%).
const MAX_SPEED_DIFFERENCE: f64 = 0.03;

/// Alignments closer together than this (half a second) count as the same
/// one when asking how unique the best is.
const UNIQUENESS_GAP_FRAMES: i64 = 50;
const GOOD_CONFIDENCE: f32 = 0.6;
const UNCERTAIN_CONFIDENCE: f32 = 0.35;

/// A pitch difference this large, held this long, is listed as a region.
const REGION_THRESHOLD_CENTS: f32 = 25.0;
const REGION_MIN_SECONDS: f64 = 0.12;
const MAX_REGIONS: usize = 500;
/// Differences beyond this are one tracker hearing a different octave or a
/// different sound, not a tuning difference; they are left out of the
/// statistics.
const OUTLIER_CENTS: f32 = 300.0;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum AlignmentQuality {
    /// The two recordings clearly line up.
    Good,
    /// They probably line up; check by ear.
    Uncertain,
    /// No convincing match: they may not be the same performance.
    Poor,
}

/// How a time in the reference recording maps to the other one:
/// `other = offset_seconds + speed_ratio × reference`.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, uniffi::Record)]
pub struct Alignment {
    pub offset_seconds: f64,
    /// 1.0 when both run at the same speed; 1.01 when the other recording
    /// takes 1% longer over the same music.
    pub speed_ratio: f64,
    /// Similarity of the two loudness envelopes at this alignment, 0–1.
    pub confidence: f32,
    pub quality: AlignmentQuality,
}

impl Alignment {
    pub fn to_other(&self, reference_seconds: f64) -> f64 {
        self.offset_seconds + self.speed_ratio * reference_seconds
    }

    pub fn to_reference(&self, other_seconds: f64) -> f64 {
        (other_seconds - self.offset_seconds) / self.speed_ratio
    }
}

/// A stretch where the two versions' pitch differs.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, uniffi::Record)]
pub struct DifferenceRegion {
    /// Times on the reference recording's timeline.
    pub start_seconds: f64,
    pub end_seconds: f64,
    /// Average of other minus reference, in cents (positive: the other
    /// version is sharper).
    pub mean_difference_cents: f32,
}

#[derive(Debug, Clone, Serialize, PartialEq, uniffi::Record)]
pub struct PitchComparison {
    /// Time during which both versions have a pitch.
    pub compared_seconds: f64,
    /// Median of other minus reference, in cents. A constant value here is a
    /// transposition or speed difference rather than a tuning change.
    pub median_difference_cents: Option<f32>,
    /// Typical size of the difference once that constant shift is removed.
    pub typical_difference_cents: Option<f32>,
    /// Share of compared time in which the versions agree within 10 cents,
    /// after removing the constant shift.
    pub share_within_10_cents: Option<f32>,
    pub regions: Vec<DifferenceRegion>,
}

/// Loudness in decibels with slow changes removed and unit variance, so that
/// two masterings of one performance look alike.
fn feature(envelope: &[f32]) -> Vec<f32> {
    let log: Vec<f32> = envelope
        .iter()
        .map(|v| 20.0 * v.max(ENVELOPE_FLOOR).log10())
        .collect();
    let radius = (DETREND_SECONDS / ENVELOPE_STEP_SECONDS / 2.0) as usize;
    let mut prefix = Vec::with_capacity(log.len() + 1);
    prefix.push(0f64);
    for value in &log {
        prefix.push(prefix[prefix.len() - 1] + *value as f64);
    }
    let mut out: Vec<f32> = (0..log.len())
        .map(|i| {
            let from = i.saturating_sub(radius);
            let to = (i + radius + 1).min(log.len());
            log[i] - ((prefix[to] - prefix[from]) / (to - from) as f64) as f32
        })
        .collect();
    let power = out.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / out.len().max(1) as f64;
    if power > 0.0 {
        let scale = (1.0 / power.sqrt()) as f32;
        out.iter_mut().for_each(|v| *v *= scale);
    }
    out
}

fn prefix_squares(values: &[f32]) -> Vec<f64> {
    let mut prefix = Vec::with_capacity(values.len() + 1);
    prefix.push(0f64);
    for value in values {
        prefix.push(prefix[prefix.len() - 1] + (*value as f64).powi(2));
    }
    prefix
}

/// Position of a peak between samples, from the parabola through it and its
/// neighbours.
fn peak_offset(before: f64, here: f64, after: f64) -> f64 {
    let curvature = before - 2.0 * here + after;
    if curvature >= 0.0 {
        return 0.0;
    }
    (0.5 * (before - after) / curvature).clamp(-0.5, 0.5)
}

/// Normalised correlation of `a` against `b` shifted by `lag` frames
/// (`a[i]` against `b[i + lag]`), over the part where they overlap.
struct Correlator<'a> {
    a: &'a [f32],
    b: &'a [f32],
    a_squares: Vec<f64>,
    b_squares: Vec<f64>,
}

impl Correlator<'_> {
    /// Overlap of the two as index ranges into `a`.
    fn overlap(&self, lag: i64) -> Option<(usize, usize)> {
        let from = (-lag).max(0);
        let to = (self.a.len() as i64).min(self.b.len() as i64 - lag);
        (to > from).then_some((from as usize, to as usize))
    }

    fn normalise(&self, sum: f64, lag: i64, from: usize, to: usize) -> f64 {
        let a_energy = self.a_squares[to] - self.a_squares[from];
        let b_from = (from as i64 + lag) as usize;
        let b_to = (to as i64 + lag) as usize;
        let b_energy = self.b_squares[b_to] - self.b_squares[b_from];
        if a_energy <= 0.0 || b_energy <= 0.0 {
            return 0.0;
        }
        sum / (a_energy * b_energy).sqrt()
    }

    /// Score of one window of `a` at one lag, computed directly.
    fn window_score(&self, from: usize, to: usize, lag: i64) -> Option<f64> {
        let b_from = from as i64 + lag;
        let b_to = to as i64 + lag;
        if b_from < 0 || b_to > self.b.len() as i64 {
            return None;
        }
        let sum: f64 = self.a[from..to]
            .iter()
            .zip(&self.b[b_from as usize..b_to as usize])
            .map(|(x, y)| *x as f64 * *y as f64)
            .sum();
        Some(self.normalise(sum, lag, from, to))
    }
}

/// Where one window of the reference best fits in the other recording.
struct WindowMatch {
    /// Centre of the window, in frames of the reference.
    centre: f64,
    /// Lag (fractional frames) at which it fits best.
    lag: f64,
}

/// Lines two recordings up from their loudness envelopes, each sampled every
/// [`ENVELOPE_STEP_SECONDS`]. `None` when either is too short to say anything.
///
/// Short windows of the reference are each located in the other recording
/// independently. Windows of a true match agree on one straight line (its
/// position gives the offset, its slope the speed difference); windows that
/// landed somewhere by chance do not, and are outvoted.
pub fn align(reference: &[f32], other: &[f32]) -> Option<Alignment> {
    let step = ENVELOPE_STEP_SECONDS;
    let shorter = reference.len().min(other.len());
    let min_overlap =
        ((MIN_OVERLAP_SECONDS / step) as usize).max((shorter as f64 * MIN_OVERLAP_SHARE) as usize);
    if shorter < min_overlap || min_overlap == 0 {
        return None;
    }
    let a = feature(reference);
    let b = feature(other);
    let correlator = Correlator {
        a_squares: prefix_squares(&a),
        b_squares: prefix_squares(&b),
        a: &a,
        b: &b,
    };

    let size = (a.len() + b.len()).next_power_of_two();
    let mut planner = RealFftPlanner::<f32>::new();
    let forward = planner.plan_fft_forward(size);
    let inverse = planner.plan_fft_inverse(size);
    let mut time = vec![0f32; size];
    let mut b_spectrum = forward.make_output_vec();
    time[..b.len()].copy_from_slice(&b);
    forward.process(&mut time, &mut b_spectrum).ok()?;
    let mut spectrum = forward.make_output_vec();
    let mut raw = vec![0f32; size];
    // Raw correlation of `a[from..to]` with all of `b`, at every lag at once.
    let mut correlate = |from: usize, to: usize, raw: &mut [f32]| -> Option<()> {
        time.fill(0.0);
        time[from..to].copy_from_slice(&a[from..to]);
        forward.process(&mut time, &mut spectrum).ok()?;
        for (x, y) in spectrum.iter_mut().zip(&b_spectrum) {
            *x = x.conj() * y;
        }
        inverse.process(&mut spectrum, raw).ok()
    };

    let window = (a.len() / 8).clamp(
        (LOCAL_WINDOW_MIN_SECONDS / step) as usize,
        (LOCAL_WINDOW_SECONDS / step) as usize,
    );
    let hop = (window / 2).max(1);
    let window_starts: Vec<usize> = (0..)
        .map(|i| i * hop)
        .take_while(|from| from + window <= a.len())
        .collect();
    // Long recordings do not need every window; an even spread is enough.
    let stride = window_starts.len().div_ceil(MAX_LOCAL_WINDOWS).max(1);

    let mut matches: Vec<WindowMatch> = Vec::new();
    for from in window_starts.iter().copied().step_by(stride) {
        let to = from + window;
        correlate(from, to, &mut raw)?;
        let score_at = |lag: i64| -> Option<f64> {
            if from as i64 + lag < 0 || to as i64 + lag > b.len() as i64 {
                return None;
            }
            let index = lag.rem_euclid(size as i64) as usize;
            Some(correlator.normalise(raw[index] as f64 / size as f64, lag, from, to))
        };
        let lags = -(from as i64)..=(b.len() as i64 - to as i64);
        let Some((lag, score)) = lags
            .filter_map(|lag| score_at(lag).map(|score| (lag, score)))
            .max_by(|x, y| x.1.total_cmp(&y.1))
        else {
            continue;
        };
        if score < LOCAL_MIN_SCORE {
            continue;
        }
        let fine = match (score_at(lag - 1), score_at(lag + 1)) {
            (Some(before), Some(after)) => peak_offset(before, score, after),
            _ => 0.0,
        };
        matches.push(WindowMatch {
            centre: (from + to) as f64 / 2.0,
            lag: lag as f64 + fine,
        });
    }

    let mut speed_ratio = 1.0;
    let mut whole_score = None;
    let offset_frames = match fit_line(&matches) {
        Some(mut line) => {
            // With the line roughly known, shorter windows searched close to
            // it give sharper positions: a speed difference smears a long
            // window's own match.
            let short = (window / 3).max((LOCAL_WINDOW_MIN_SECONDS / step) as usize);
            let short_starts: Vec<usize> = (0..)
                .map(|i| i * (short / 2).max(1))
                .take_while(|from| from + short <= a.len())
                .collect();
            let stride = short_starts.len().div_ceil(4 * MAX_LOCAL_WINDOWS).max(1);
            let refined: Vec<WindowMatch> = short_starts
                .iter()
                .copied()
                .step_by(stride)
                .filter_map(|from| {
                    let centre = (from + short / 2) as f64;
                    let expected = (line.1 + line.0 * centre).round() as i64;
                    let scores: Vec<f64> = (expected - REFINE_SEARCH_FRAMES
                        ..=expected + REFINE_SEARCH_FRAMES)
                        .map(|lag| correlator.window_score(from, from + short, lag))
                        .collect::<Option<_>>()?;
                    let peak =
                        (0..scores.len()).max_by(|x, y| scores[*x].total_cmp(&scores[*y]))?;
                    // A peak at the edge of the search is not a peak.
                    if scores[peak] < LOCAL_MIN_SCORE || peak == 0 || peak + 1 == scores.len() {
                        return None;
                    }
                    let fine = peak_offset(scores[peak - 1], scores[peak], scores[peak + 1]);
                    Some(WindowMatch {
                        centre,
                        lag: (expected - REFINE_SEARCH_FRAMES + peak as i64) as f64 + fine,
                    })
                })
                .collect();
            if let Some(better) = fit_line(&refined) {
                line = better;
            }
            if line.0.abs() >= MIN_SPEED_DIFFERENCE {
                speed_ratio = 1.0 + line.0;
            }
            line.1
        }
        None => {
            // Too short for windows to vote, or no agreement between them:
            // fall back on the single best overlap of the two as wholes.
            correlate(0, a.len(), &mut raw)?;
            let score_at = |lag: i64| -> Option<f64> {
                let (from, to) = correlator.overlap(lag)?;
                if to - from < min_overlap {
                    return None;
                }
                let index = lag.rem_euclid(size as i64) as usize;
                Some(correlator.normalise(raw[index] as f64 / size as f64, lag, from, to))
            };
            let all_lags = || -(a.len() as i64 - 1)..=(b.len() as i64 - 1);
            let (lag, score) = all_lags()
                .filter_map(|lag| score_at(lag).map(|score| (lag, score)))
                .max_by(|x, y| x.1.total_cmp(&y.1))?;
            // Discounted by how close the next-best alignment comes.
            let runner_up = all_lags()
                .filter(|other| (other - lag).abs() > UNIQUENESS_GAP_FRAMES)
                .filter_map(score_at)
                .fold(0f64, f64::max);
            let uniqueness = ((score - runner_up) / (0.5 * score)).clamp(0.0, 1.0);
            whole_score = Some(score * uniqueness);
            lag as f64
                + match (score_at(lag - 1), score_at(lag + 1)) {
                    (Some(before), Some(after)) => peak_offset(before, score, after),
                    _ => 0.0,
                }
        }
    };

    // How alike the envelopes are along the final mapping, window by window.
    // The median is high only when the recordings match throughout, which a
    // lucky peak somewhere cannot fake.
    // Short windows again, so a speed difference is not itself counted as
    // dissimilarity.
    let short = (window / 3).max((LOCAL_WINDOW_MIN_SECONDS / step) as usize);
    let mut local: Vec<f64> = (0..)
        .map(|i| i * (short / 2).max(1))
        .take_while(|from| from + short <= a.len())
        .filter_map(|from| {
            let centre = (from + short / 2) as f64;
            let lag = (offset_frames + (speed_ratio - 1.0) * centre).round() as i64;
            correlator.window_score(from, from + short, lag)
        })
        .collect();
    let confidence = if local.len() >= 3 {
        local.sort_by(f64::total_cmp);
        local[local.len() / 2]
    } else {
        whole_score.unwrap_or(0.0)
    }
    .clamp(0.0, 1.0) as f32;
    Some(Alignment {
        offset_seconds: offset_frames * step,
        speed_ratio,
        confidence,
        quality: if confidence >= GOOD_CONFIDENCE {
            AlignmentQuality::Good
        } else if confidence >= UNCERTAIN_CONFIDENCE {
            AlignmentQuality::Uncertain
        } else {
            AlignmentQuality::Poor
        },
    })
}

/// The straight line most of the window matches agree on, as (slope,
/// intercept) in frames, or `None` when there is no such agreement.
fn fit_line(matches: &[WindowMatch]) -> Option<(f64, f64)> {
    if matches.len() < LOCAL_MIN_WINDOWS {
        return None;
    }
    let median = |mut values: Vec<f64>| -> f64 {
        values.sort_by(f64::total_cmp);
        values[values.len() / 2]
    };
    // Theil–Sen: the median slope over pairs of windows is unmoved by a
    // minority of windows that matched in the wrong place.
    let min_separation = (LOCAL_SLOPE_BASELINE_SECONDS / ENVELOPE_STEP_SECONDS)
        .min((matches[matches.len() - 1].centre - matches[0].centre) / 2.0);
    let slopes: Vec<f64> = matches
        .iter()
        .enumerate()
        .flat_map(|(i, first)| {
            matches[i + 1..]
                .iter()
                .filter(move |second| second.centre - first.centre >= min_separation)
                .map(move |second| (second.lag - first.lag) / (second.centre - first.centre))
        })
        .collect();
    let mut slope = if slopes.is_empty() {
        0.0
    } else {
        median(slopes)
    };
    if slope.abs() > MAX_SPEED_DIFFERENCE {
        slope = 0.0;
    }
    let intercept = median(matches.iter().map(|m| m.lag - slope * m.centre).collect());

    let agreeing: Vec<&WindowMatch> = matches
        .iter()
        .filter(|m| (m.lag - (intercept + slope * m.centre)).abs() <= LOCAL_AGREEMENT_FRAMES)
        .collect();
    if agreeing.len() < LOCAL_MIN_WINDOWS || agreeing.len() * 2 < matches.len() {
        return None;
    }
    // Least squares over the windows that agree sharpens the estimate.
    let n = agreeing.len() as f64;
    let mean_x = agreeing.iter().map(|m| m.centre).sum::<f64>() / n;
    let mean_y = agreeing.iter().map(|m| m.lag).sum::<f64>() / n;
    let variance: f64 = agreeing.iter().map(|m| (m.centre - mean_x).powi(2)).sum();
    if variance <= 0.0 {
        return Some((0.0, mean_y));
    }
    let refined = agreeing
        .iter()
        .map(|m| (m.centre - mean_x) * (m.lag - mean_y))
        .sum::<f64>()
        / variance;
    // A slope only counts as a speed difference when it moves the two
    // recordings apart by more than the windows' own scatter.
    let span = agreeing[agreeing.len() - 1].centre - agreeing[0].centre;
    if refined.abs() < MIN_SPEED_DIFFERENCE || refined.abs() * span < MIN_DRIFT_FRAMES {
        Some((0.0, mean_y))
    } else {
        Some((refined, mean_y - refined * mean_x))
    }
}

fn median(values: &mut [f32]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    let mid = values.len() / 2;
    Some(*values.select_nth_unstable_by(mid, f32::total_cmp).1)
}

/// Compares the pitch of two analysed recordings along an alignment.
pub fn compare_pitch(
    reference: &Analysis,
    other: &Analysis,
    alignment: &Alignment,
) -> PitchComparison {
    let hop = reference.track.hop_seconds;
    let other_hop = other.track.hop_seconds;
    // Other minus reference per reference frame, where both are voiced.
    let differences: Vec<Option<f32>> = (0..reference.track.len())
        .map(|i| {
            let here = reference.track.midi(i)?;
            let position = alignment.to_other(i as f64 * hop) / other_hop;
            if position < 0.0 {
                return None;
            }
            // Interpolate between the two nearest frames when both are voiced.
            let below = position.floor() as usize;
            let blend = (position - below as f64) as f32;
            let there = match (other.track.midi(below), other.track.midi(below + 1)) {
                (Some(a), Some(b)) => a + (b - a) * blend,
                (Some(a), None) if blend < 0.5 => a,
                (None, Some(b)) if blend >= 0.5 => b,
                _ => return None,
            };
            Some((there - here) * 100.0)
        })
        .collect();

    let mut usable: Vec<f32> = differences
        .iter()
        .flatten()
        .copied()
        .filter(|d| d.abs() <= OUTLIER_CENTS)
        .collect();
    let compared = differences.iter().flatten().count();
    let median_difference = median(&mut usable);
    let shift = median_difference.unwrap_or(0.0);
    let mut residual: Vec<f32> = usable.iter().map(|d| (d - shift).abs()).collect();
    let within = residual.iter().filter(|d| **d <= 10.0).count();
    let share_within_10_cents =
        (!residual.is_empty()).then(|| within as f32 / residual.len() as f32);
    let typical_difference = median(&mut residual);

    // Regions are judged with the constant shift removed too: a transposed
    // or sped-up copy differs everywhere, which is one fact, not hundreds.
    let min_frames = (REGION_MIN_SECONDS / hop).ceil() as usize;
    let mut regions: Vec<DifferenceRegion> = Vec::new();
    let mut start: Option<usize> = None;
    let mut sum = 0f64;
    for i in 0..=differences.len() {
        let value = differences
            .get(i)
            .copied()
            .flatten()
            .map(|d| d - shift)
            .filter(|d| d.abs() > REGION_THRESHOLD_CENTS && d.abs() <= OUTLIER_CENTS);
        // A region continues only while the difference keeps its direction.
        let continues = match (value, start) {
            (Some(d), Some(_)) => (d > 0.0) == (sum > 0.0),
            _ => false,
        };
        if let (Some(first), false) = (start, continues) {
            if i - first >= min_frames {
                regions.push(DifferenceRegion {
                    start_seconds: first as f64 * hop,
                    end_seconds: i as f64 * hop,
                    mean_difference_cents: (sum / (i - first) as f64) as f32,
                });
            }
            start = None;
            sum = 0.0;
        }
        if let Some(d) = value {
            if start.is_none() {
                start = Some(i);
            }
            sum += d as f64;
        }
    }
    if regions.len() > MAX_REGIONS {
        // Keep the longest, in time order.
        regions.sort_by(|a, b| {
            (b.end_seconds - b.start_seconds).total_cmp(&(a.end_seconds - a.start_seconds))
        });
        regions.truncate(MAX_REGIONS);
        regions.sort_by(|a, b| a.start_seconds.total_cmp(&b.start_seconds));
    }

    PitchComparison {
        compared_seconds: compared as f64 * hop,
        median_difference_cents: median_difference,
        typical_difference_cents: typical_difference,
        share_within_10_cents,
        regions,
    }
}

#[cfg(test)]
mod tests {
    use super::super::pitch::test_support::{noise, synth, track};
    use super::super::test_support::VOICE;
    use super::*;

    /// A loudness envelope that looks like music: notes of irregular length
    /// and loudness, with rests.
    fn envelope(seconds: f64, seed: u64) -> Vec<f32> {
        let random = noise((seconds * 30.0) as usize + 16, seed);
        let mut out = Vec::new();
        let mut draw = 0usize;
        while (out.len() as f64) * ENVELOPE_STEP_SECONDS < seconds {
            // 0.12 to 0.6 s per note.
            let frames = 12 + (random[draw].abs() * 48.0) as usize;
            let level = 0.15 + 0.6 * random[draw + 1].abs();
            let rest = random[draw + 2] > 0.6;
            draw += 3;
            for i in 0..frames {
                out.push(if rest {
                    0.002
                } else {
                    level * (-4.0 * i as f32 * ENVELOPE_STEP_SECONDS as f32).exp()
                });
            }
        }
        out.truncate((seconds / ENVELOPE_STEP_SECONDS) as usize);
        out
    }

    /// `source` as it would be in a recording that starts `delay` seconds
    /// later and runs at `speed` times the length.
    fn warp(source: &[f32], delay: f64, speed: f64, gain: f32) -> Vec<f32> {
        let count = ((source.len() as f64 * speed) + delay / ENVELOPE_STEP_SECONDS) as usize;
        (0..count)
            .map(|i| {
                let t = (i as f64 * ENVELOPE_STEP_SECONDS - delay) / speed;
                let position = t / ENVELOPE_STEP_SECONDS;
                if position < 0.0 || position as usize + 1 >= source.len() {
                    return 0.0005;
                }
                let below = position as usize;
                let blend = (position - below as f64) as f32;
                gain * (source[below] * (1.0 - blend) + source[below + 1] * blend)
            })
            .collect()
    }

    #[test]
    fn finds_a_plain_offset_in_either_direction() {
        let a = envelope(90.0, 1);
        for delay in [0.0, 1.234, 17.5] {
            let b = warp(&a, delay, 1.0, 0.6);
            let forward = align(&a, &b).unwrap();
            assert_eq!(forward.quality, AlignmentQuality::Good);
            assert!(forward.confidence > 0.9);
            assert_eq!(forward.speed_ratio, 1.0);
            assert!(
                (forward.offset_seconds - delay).abs() < 0.003,
                "delay {delay}: found {}",
                forward.offset_seconds
            );
            let backward = align(&b, &a).unwrap();
            assert!((backward.offset_seconds + delay).abs() < 0.003);
            assert!((forward.to_other(10.0) - (10.0 + delay)).abs() < 0.003);
            assert!((forward.to_reference(forward.to_other(42.0)) - 42.0).abs() < 1e-9);
        }
    }

    #[test]
    fn finds_a_speed_difference() {
        let a = envelope(180.0, 2);
        for speed in [1.002, 0.995, 1.012] {
            let b = warp(&a, 2.5, speed, 1.3);
            let found = align(&a, &b).unwrap();
            assert_eq!(found.quality, AlignmentQuality::Good, "{found:?}");
            assert!(
                (found.speed_ratio - speed).abs() < 5e-5,
                "speed {speed}: found {}",
                found.speed_ratio
            );
            // Check the mapping itself, start and end.
            for t in [5.0, 170.0] {
                let expected = 2.5 + speed * t;
                assert!(
                    (found.to_other(t) - expected).abs() < 0.01,
                    "at {t}: {} vs {expected}",
                    found.to_other(t)
                );
            }
        }
    }

    #[test]
    fn a_different_mastering_still_aligns() {
        let a = envelope(60.0, 3);
        // Heavy compression (a power law) plus a noise floor.
        let hiss = noise(a.len() + 400, 9);
        let b: Vec<f32> = warp(&a, 3.0, 1.0, 1.0)
            .iter()
            .zip(&hiss)
            .map(|(v, n)| v.powf(0.5) * 0.8 + n.abs() * 0.01)
            .collect();
        let found = align(&a, &b).unwrap();
        assert_ne!(found.quality, AlignmentQuality::Poor, "{found:?}");
        assert!((found.offset_seconds - 3.0).abs() < 0.01, "{found:?}");
    }

    #[test]
    fn unrelated_recordings_are_a_poor_match() {
        let found = align(&envelope(60.0, 4), &envelope(60.0, 5)).unwrap();
        assert_eq!(found.quality, AlignmentQuality::Poor, "{found:?}");
        assert!(found.confidence < UNCERTAIN_CONFIDENCE);
    }

    #[test]
    fn recordings_too_short_to_align_are_declined() {
        assert!(align(&envelope(2.0, 1), &envelope(60.0, 1)).is_none());
        // Ten seconds is enough, by way of the whole-recording fallback.
        let a = envelope(10.0, 6);
        let short = align(&a, &warp(&a, 0.75, 1.0, 1.0)).unwrap();
        assert!((short.offset_seconds - 0.75).abs() < 0.005, "{short:?}");
        assert_ne!(short.quality, AlignmentQuality::Poor, "{short:?}");
        assert!(align(&[], &[]).is_none());
    }

    fn analysis(midi_at: impl Fn(f64) -> f64, seconds: f64) -> Analysis {
        Analysis::from_track(track(
            44_100,
            &synth(44_100, seconds, &VOICE, midi_at, |_| 0.4),
        ))
    }

    fn plain(offset: f64) -> Alignment {
        Alignment {
            offset_seconds: offset,
            speed_ratio: 1.0,
            confidence: 1.0,
            quality: AlignmentQuality::Good,
        }
    }

    #[test]
    fn identical_pitch_compares_as_identical() {
        let melody = |t: f64| 60.0 + (t * 2.0).floor() * 2.0;
        let a = analysis(melody, 2.0);
        let same = compare_pitch(&a, &a, &plain(0.0));
        assert!((same.compared_seconds - 2.0).abs() < 0.1);
        assert!(same.median_difference_cents.unwrap().abs() < 0.01);
        assert!(same.typical_difference_cents.unwrap() < 0.01);
        assert_eq!(same.share_within_10_cents, Some(1.0));
        assert!(same.regions.is_empty());
    }

    #[test]
    fn a_retuned_passage_is_found_and_measured() {
        // The reference sags 40 cents flat between 1.0 and 1.5 s; the other
        // version holds the note.
        let reference = analysis(
            |t| 64.0 - if (1.0..1.5).contains(&t) { 0.4 } else { 0.0 },
            2.5,
        );
        // The other version starts 0.5 s later.
        let other = analysis(|_| 64.0, 3.0);
        let result = compare_pitch(&reference, &other, &plain(0.5));
        assert_eq!(result.regions.len(), 1, "{:#?}", result.regions);
        let region = result.regions[0];
        assert!((region.start_seconds - 1.0).abs() < 0.04, "{region:?}");
        assert!((region.end_seconds - 1.5).abs() < 0.04, "{region:?}");
        assert!(
            (region.mean_difference_cents - 40.0).abs() < 3.0,
            "{region:?}"
        );
        assert!(result.median_difference_cents.unwrap().abs() < 1.0);
        let share = result.share_within_10_cents.unwrap();
        assert!((0.74..0.86).contains(&share), "share {share}");
    }

    #[test]
    fn a_constant_shift_is_one_fact_not_many_regions() {
        let melody = |t: f64| 62.0 + (t * 2.0).floor();
        let reference = analysis(melody, 2.0);
        let sharper = analysis(|t| melody(t) + 0.35, 2.0);
        let result = compare_pitch(&reference, &sharper, &plain(0.0));
        assert!((result.median_difference_cents.unwrap() - 35.0).abs() < 1.0);
        assert!(result.typical_difference_cents.unwrap() < 1.0);
        assert!(result.regions.is_empty(), "{:#?}", result.regions);
    }

    #[test]
    fn nothing_in_common_gives_an_empty_comparison() {
        let a = analysis(|_| 60.0, 1.0);
        let result = compare_pitch(&a, &a, &plain(30.0));
        assert_eq!(result.compared_seconds, 0.0);
        assert_eq!(result.median_difference_cents, None);
        assert_eq!(result.share_within_10_cents, None);
        assert!(result.regions.is_empty());
    }
}
