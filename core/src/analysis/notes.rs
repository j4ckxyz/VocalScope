//! Turning a pitch curve into notes, and measuring each note.
//!
//! Notes are found without reference to any scale: a note is a stretch of
//! voiced pitch that stays near its own centre. Only afterwards is each
//! centre compared with equal temperament, which is what makes "how close to
//! the scale is this singer" a measurement rather than an assumption.

use serde::Serialize;

use super::pitch::{midi_to_hz, PitchTrack};

/// Voiced stretches shorter than this are not considered at all.
const MIN_RUN_FRAMES: usize = 5;
/// Notes shorter than this are ornaments or glides, not notes.
const MIN_NOTE_SECONDS: f64 = 0.08;
/// Half-width, in frames, of the median filter that hides vibrato while
/// boundaries are being found (17 frames = 170 ms, one cycle at 5.9 Hz).
const SMOOTHING_RADIUS: usize = 8;
/// A new note starts when the smoothed pitch has left the current note's
/// centre by more than this many semitones...
const BOUNDARY_SEMITONES: f32 = 0.55;
/// ...for this many frames in a row.
const BOUNDARY_FRAMES: usize = 3;
/// Notes at least this long have a settled middle worth measuring.
const MIN_MEASURED_SECONDS: f64 = 0.25;
/// Share of a note trimmed from each end before measuring its middle.
const EDGE_TRIM: f64 = 0.2;
/// Vibrato search range and what counts as vibrato.
const VIBRATO_MIN_HZ: f64 = 3.5;
const VIBRATO_MAX_HZ: f64 = 9.0;
const VIBRATO_MIN_EXTENT_CENTS: f64 = 8.0;
const VIBRATO_MIN_EXPLAINED: f64 = 0.5;
const VIBRATO_MIN_CYCLES: f64 = 2.0;
/// Two notes are joined by a transition when the pitch never stops between
/// them and they are at least this far apart.
const TRANSITION_MIN_SEMITONES: f32 = 0.8;
const TRANSITION_MAX_GAP_SECONDS: f64 = 0.3;
/// Averaging length of the tracker beyond one period (its window), used to
/// undo the slight flattening of vibrato it causes.
const TRACKER_WINDOW_SECONDS: f64 = 0.032;

const NOTE_NAMES: [&str; 12] = [
    "C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B",
];

/// Regular pitch wobble within a note.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, uniffi::Record)]
pub struct Vibrato {
    /// Cycles per second.
    pub rate_hz: f32,
    /// How far the pitch swings to each side of the note's centre, in cents.
    pub extent_cents: f32,
}

/// One sung note.
#[derive(Debug, Clone, Serialize, PartialEq, uniffi::Record)]
pub struct Note {
    pub start_seconds: f64,
    pub end_seconds: f64,
    /// Centre of the note: the median pitch of its settled middle.
    pub frequency_hz: f32,
    /// The same as a fractional MIDI note number (69 = A4 = 440 Hz).
    pub midi_pitch: f32,
    /// Nearest equal-tempered note.
    pub midi_note: u8,
    /// Name of the nearest note, e.g. `A4` or `C♯5`.
    pub name: String,
    /// Distance from the nearest note in cents (positive is sharp), with
    /// A4 = 440 Hz.
    pub deviation_cents: f32,
    /// How much the pitch wanders within the settled middle of the note once
    /// any steady drift and vibrato are taken out: one standard deviation,
    /// in cents. `None` for notes too short to measure.
    pub steadiness_cents: Option<f32>,
    /// Steady rise (positive) or fall across the settled middle, in cents.
    pub drift_cents: Option<f32>,
    pub vibrato: Option<Vibrato>,
    /// When the pitch moved here from the previous note without a break:
    /// how long the middle 60% of that move took, in milliseconds.
    pub transition_in_ms: Option<f32>,
    /// Average level over the note in dBFS.
    pub level_db: f32,
}

impl Note {
    pub fn duration_seconds(&self) -> f64 {
        self.end_seconds - self.start_seconds
    }
}

/// Name of a MIDI note number, e.g. 69 → `A4`.
pub fn note_name(midi_note: u8) -> String {
    format!(
        "{}{}",
        NOTE_NAMES[midi_note as usize % 12],
        midi_note as i32 / 12 - 1
    )
}

/// Nearest note name and the distance from it in cents, for any pitch.
pub fn describe_pitch(midi_pitch: f32) -> (String, f32) {
    let nearest = midi_pitch.round().clamp(0.0, 127.0);
    (note_name(nearest as u8), (midi_pitch - nearest) * 100.0)
}

fn median(values: &mut [f32]) -> f32 {
    if values.is_empty() {
        return f32::NAN;
    }
    let mid = values.len() / 2;
    let (_, value, _) = values.select_nth_unstable_by(mid, f32::total_cmp);
    *value
}

/// Finds the notes in a pitch track.
pub fn find_notes(track: &PitchTrack) -> Vec<Note> {
    let hop = track.hop_seconds;
    let midi: Vec<f32> = (0..track.len())
        .map(|i| track.midi(i).unwrap_or(f32::NAN))
        .collect();
    let min_note_frames = (MIN_NOTE_SECONDS / hop).round().max(1.0) as usize;

    // (first frame, one past the last frame) of each note.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    while start < midi.len() {
        if midi[start].is_nan() {
            start += 1;
            continue;
        }
        let end = (start..midi.len())
            .find(|i| midi[*i].is_nan())
            .unwrap_or(midi.len());
        if end - start >= MIN_RUN_FRAMES {
            split_run(&midi[start..end], start, min_note_frames, &mut spans);
        }
        start = end;
    }

    let mut notes: Vec<Note> = Vec::with_capacity(spans.len());
    for (index, (first, last)) in spans.iter().copied().enumerate() {
        let mut note = measure(track, &midi, first, last);
        if let Some((previous_first, previous_last)) = index.checked_sub(1).map(|i| spans[i]) {
            let gap = (first - previous_last) as f64 * hop;
            let continuous = midi[previous_last..first].iter().all(|m| !m.is_nan());
            let previous = &notes[index - 1];
            if continuous
                && gap <= TRANSITION_MAX_GAP_SECONDS
                && (note.midi_pitch - previous.midi_pitch).abs() >= TRANSITION_MIN_SEMITONES
            {
                note.transition_in_ms = transition_time(
                    &midi,
                    (previous_first + previous_last) / 2,
                    (first + last) / 2,
                    previous.midi_pitch,
                    note.midi_pitch,
                )
                .map(|frames| (frames * hop * 1000.0) as f32);
            }
        }
        notes.push(note);
    }
    notes
}

/// Splits one unbroken voiced stretch into notes.
fn split_run(run: &[f32], offset: usize, min_note_frames: usize, spans: &mut Vec<(usize, usize)>) {
    // A median filter hides vibrato but, unlike an average, keeps the step
    // between two notes sharp and in the right place.
    let mut window = Vec::with_capacity(2 * SMOOTHING_RADIUS + 1);
    let smooth: Vec<f32> = (0..run.len())
        .map(|i| {
            let from = i.saturating_sub(SMOOTHING_RADIUS);
            let to = (i + SMOOTHING_RADIUS + 1).min(run.len());
            window.clear();
            window.extend_from_slice(&run[from..to]);
            median(&mut window)
        })
        .collect();

    let mut emit = |first: usize, last: usize| {
        if last - first >= min_note_frames {
            spans.push((offset + first, offset + last));
        }
    };
    let mut first = 0usize;
    let mut sum = 0f64;
    let mut away = 0usize;
    for (i, value) in smooth.iter().enumerate() {
        // The centre is the mean of the frames that agreed with it.
        let members = i - first - away;
        let centre = if members > 0 {
            (sum / members as f64) as f32
        } else {
            *value
        };
        if (value - centre).abs() > BOUNDARY_SEMITONES {
            away += 1;
            if away >= BOUNDARY_FRAMES {
                let boundary = i + 1 - away;
                emit(first, boundary);
                first = boundary;
                sum = smooth[boundary..=i].iter().map(|v| *v as f64).sum();
                away = 0;
            }
        } else {
            // Frames that strayed briefly and came back belong to the note.
            sum += smooth[i - away..=i].iter().map(|v| *v as f64).sum::<f64>();
            away = 0;
        }
    }
    emit(first, smooth.len());
}

fn measure(track: &PitchTrack, midi: &[f32], first: usize, last: usize) -> Note {
    let hop = track.hop_seconds;
    let frames = last - first;
    let trim = ((frames as f64 * EDGE_TRIM) as usize).min(frames.saturating_sub(1) / 2);
    let middle = &midi[first + trim..last - trim];

    let mut sorted = middle.to_vec();
    let midi_pitch = median(&mut sorted);
    let nearest = midi_pitch.round().clamp(0.0, 127.0);
    let level_db = {
        let levels = &track.level_db[first..last];
        let power: f64 = levels.iter().map(|db| 10f64.powf(*db as f64 / 10.0)).sum();
        (10.0 * (power / levels.len() as f64).log10()) as f32
    };

    let mut note = Note {
        start_seconds: first as f64 * hop,
        end_seconds: last as f64 * hop,
        frequency_hz: midi_to_hz(midi_pitch),
        midi_pitch,
        midi_note: nearest as u8,
        name: note_name(nearest as u8),
        deviation_cents: (midi_pitch - nearest) * 100.0,
        steadiness_cents: None,
        drift_cents: None,
        vibrato: None,
        transition_in_ms: None,
        level_db,
    };

    if frames as f64 * hop >= MIN_MEASURED_SECONDS && middle.len() >= 8 {
        let cents: Vec<f64> = middle
            .iter()
            .map(|m| (*m - midi_pitch) as f64 * 100.0)
            .collect();
        let period_seconds = 1.0 / note.frequency_hz.max(1.0) as f64;
        let (mut slope, mut residual) = detrend(&cents);
        if let Some(fit) = fit_vibrato(&cents, hop, TRACKER_WINDOW_SECONDS + period_seconds) {
            note.vibrato = Some(fit.vibrato);
            slope = fit.slope;
            residual = fit.residual;
        }
        note.drift_cents = Some((slope * (cents.len() - 1) as f64) as f32);
        note.steadiness_cents = Some(standard_deviation(&residual) as f32);
    }
    note
}

/// Removes the least-squares straight line. Returns its slope per sample and
/// what is left.
fn detrend(values: &[f64]) -> (f64, Vec<f64>) {
    let n = values.len() as f64;
    let mean_x = (n - 1.0) / 2.0;
    let mean_y = values.iter().sum::<f64>() / n;
    let (mut covariance, mut variance) = (0.0, 0.0);
    for (i, y) in values.iter().enumerate() {
        let dx = i as f64 - mean_x;
        covariance += dx * (y - mean_y);
        variance += dx * dx;
    }
    let slope = if variance > 0.0 {
        covariance / variance
    } else {
        0.0
    };
    let residual = values
        .iter()
        .enumerate()
        .map(|(i, y)| y - mean_y - slope * (i as f64 - mean_x))
        .collect();
    (slope, residual)
}

fn standard_deviation(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64).sqrt()
}

/// Solves the least-squares problem `columns · x ≈ y` for a handful of
/// columns, by Gaussian elimination on the normal equations.
fn least_squares(columns: &[&[f64]], y: &[f64]) -> Option<Vec<f64>> {
    let n = columns.len();
    let mut matrix = vec![vec![0f64; n + 1]; n];
    for row in 0..n {
        for col in 0..n {
            matrix[row][col] = columns[row]
                .iter()
                .zip(columns[col])
                .map(|(a, b)| a * b)
                .sum();
        }
        matrix[row][n] = columns[row].iter().zip(y).map(|(a, b)| a * b).sum();
    }
    for pivot in 0..n {
        let best = (pivot..n)
            .max_by(|a, b| matrix[*a][pivot].abs().total_cmp(&matrix[*b][pivot].abs()))?;
        matrix.swap(pivot, best);
        let lead = matrix[pivot][pivot];
        if lead.abs() < 1e-9 {
            return None;
        }
        for row in 0..n {
            if row == pivot {
                continue;
            }
            let factor = matrix[row][pivot] / lead;
            for col in pivot..=n {
                matrix[row][col] -= factor * matrix[pivot][col];
            }
        }
    }
    Some((0..n).map(|i| matrix[i][n] / matrix[i][i]).collect())
}

struct VibratoFit {
    vibrato: Vibrato,
    /// Steady drift per frame, fitted together with the vibrato.
    slope: f64,
    /// The contour with both drift and vibrato removed.
    residual: Vec<f64>,
}

/// Looks for one regular oscillation in a note's pitch contour (cents).
///
/// A straight line and a sinusoid are fitted together at each trial rate,
/// because a line fitted on its own soaks up part of any vibrato that does
/// not complete a whole number of cycles.
fn fit_vibrato(cents: &[f64], hop: f64, averaging_seconds: f64) -> Option<VibratoFit> {
    let n = cents.len();
    let duration = n as f64 * hop;
    let (_, line_residual) = detrend(cents);
    let line_error: f64 = line_residual.iter().map(|v| v * v).sum();
    if line_error <= 0.0 {
        return None;
    }
    let ones = vec![1f64; n];
    let ramp: Vec<f64> = (0..n).map(|i| i as f64 - (n - 1) as f64 / 2.0).collect();
    let mut sine = vec![0f64; n];
    let mut cosine = vec![0f64; n];

    let mut best: Option<(f64, f64, Vec<f64>)> = None; // (error, rate, coefficients)
    let mut rate = VIBRATO_MIN_HZ.max(VIBRATO_MIN_CYCLES / duration);
    while rate <= VIBRATO_MAX_HZ {
        let omega = std::f64::consts::TAU * rate * hop;
        for i in 0..n {
            (sine[i], cosine[i]) = (omega * i as f64).sin_cos();
        }
        let columns: [&[f64]; 4] = [&ones, &ramp, &sine, &cosine];
        if let Some(x) = least_squares(&columns, cents) {
            let error: f64 = (0..n)
                .map(|i| {
                    (cents[i] - x[0] - x[1] * ramp[i] - x[2] * sine[i] - x[3] * cosine[i]).powi(2)
                })
                .sum();
            if best.as_ref().is_none_or(|(e, ..)| error < *e) {
                best = Some((error, rate, x));
            }
        }
        rate += 0.05;
    }
    let (error, rate, x) = best?;
    // Each pitch estimate is an average over a short stretch of audio, which
    // rounds the peaks of a fast wobble off by a known factor.
    let phase = std::f64::consts::PI * rate * averaging_seconds;
    let flattening = if phase > 1e-6 {
        phase.sin() / phase
    } else {
        1.0
    };
    let extent = (x[2] * x[2] + x[3] * x[3]).sqrt() / flattening.max(0.5);
    if 1.0 - error / line_error < VIBRATO_MIN_EXPLAINED || extent < VIBRATO_MIN_EXTENT_CENTS {
        return None;
    }
    let omega = std::f64::consts::TAU * rate * hop;
    let residual = (0..n)
        .map(|i| {
            let (s, c) = (omega * i as f64).sin_cos();
            cents[i] - x[0] - x[1] * ramp[i] - x[2] * s - x[3] * c
        })
        .collect();
    Some(VibratoFit {
        vibrato: Vibrato {
            rate_hz: rate as f32,
            extent_cents: extent as f32,
        },
        slope: x[1],
        residual,
    })
}

/// Time, in frames, the pitch took to cover the middle 60% of the way from
/// one note's centre to the next, searching between the two notes' middles.
fn transition_time(midi: &[f32], from: usize, to: usize, start: f32, end: f32) -> Option<f64> {
    let progress = |i: usize| ((midi[i] - start) / (end - start)) as f64;
    // Fractional frame at which `progress` crosses `level` between i and i+1.
    let crossing = |i: usize, level: f64| {
        let (a, b) = (progress(i), progress(i + 1));
        i as f64 + if b != a { (level - a) / (b - a) } else { 0.0 }
    };
    // First arrival near the new note, then the last departure from the old
    // one before it.
    let arrive = (from..to).find(|i| progress(*i) < 0.8 && progress(*i + 1) >= 0.8)?;
    let leave = (from..=arrive)
        .rev()
        .find(|i| progress(*i) <= 0.2 && progress(*i + 1) > 0.2)?;
    Some((crossing(arrive, 0.8) - crossing(leave, 0.2)).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::super::pitch::test_support::{noise, synth, track};
    use super::*;

    const RATE: u32 = 44_100;
    const VOICE: [f64; 5] = [1.0, 0.6, 0.4, 0.25, 0.15];

    fn notes_of(samples: &[f32]) -> Vec<Note> {
        find_notes(&track(RATE, samples))
    }

    #[test]
    fn names_notes() {
        assert_eq!(note_name(69), "A4");
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(61), "C♯4");
        assert_eq!(note_name(0), "C-1");
        assert_eq!(note_name(127), "G9");
        let (name, cents) = describe_pitch(68.8);
        assert_eq!(name, "A4");
        assert!((cents + 20.0).abs() < 0.01);
    }

    #[test]
    fn a_melody_with_breaths_becomes_its_notes() {
        // A3, C4 (20 cents sharp), E4, each half a second, separated by breaths.
        let pitches = [57.0, 60.2, 64.0];
        let mut samples = Vec::new();
        for (i, pitch) in pitches.iter().enumerate() {
            samples.extend(synth(RATE, 0.5, &VOICE, |_| *pitch, |_| 0.4));
            samples.extend(
                noise(RATE as usize / 5, i as u64 + 1)
                    .iter()
                    .map(|s| s * 0.01),
            );
        }
        let notes = notes_of(&samples);
        assert_eq!(notes.len(), 3, "{notes:#?}");
        for (note, (pitch, start)) in notes.iter().zip(pitches.iter().zip([0.0, 0.7, 1.4])) {
            assert!((note.midi_pitch as f64 - pitch).abs() < 0.01);
            assert!((note.start_seconds - start).abs() < 0.04, "{note:?}");
            assert!((note.duration_seconds() - 0.5).abs() < 0.06, "{note:?}");
            assert!(note.transition_in_ms.is_none());
            assert!(note.vibrato.is_none());
            assert!(note.steadiness_cents.unwrap() < 0.5);
            assert!(note.drift_cents.unwrap().abs() < 1.0);
        }
        assert_eq!(notes[0].name, "A3");
        assert_eq!(notes[1].name, "C4");
        assert!((notes[1].deviation_cents - 20.0).abs() < 0.5);
        assert!((notes[1].frequency_hz - 264.67).abs() < 0.1);
        assert!(
            (notes[0].level_db + 10.0).abs() < 4.0,
            "{}",
            notes[0].level_db
        );
    }

    #[test]
    fn legato_notes_are_split_and_their_transition_is_timed() {
        for glide in [0.0, 0.05, 0.12] {
            // A4 for 0.6 s, then up a whole tone, moving over `glide` seconds.
            let midi_at = |t: f64| {
                let moved = if glide > 0.0 {
                    ((t - 0.6) / glide).clamp(0.0, 1.0)
                } else if t >= 0.6 {
                    1.0
                } else {
                    0.0
                };
                69.0 + 2.0 * moved
            };
            let notes = notes_of(&synth(RATE, 1.3, &VOICE, midi_at, |_| 0.4));
            assert_eq!(notes.len(), 2, "glide {glide}: {notes:#?}");
            assert_eq!(notes[0].name, "A4");
            assert_eq!(notes[1].name, "B4");
            assert!((notes[0].end_seconds - (0.6 + glide / 2.0)).abs() < 0.05);
            let measured = notes[1].transition_in_ms.expect("a timed transition") as f64;
            // The middle 60% of a straight glide takes 60% of its length;
            // the tracker's own 35 ms of averaging is the floor.
            let expected = (glide * 600.0).max(8.0);
            assert!(
                (measured - expected).abs() < 16.0,
                "glide {glide}: measured {measured:.1} ms, expected about {expected:.1}"
            );
        }
    }

    #[test]
    fn vibrato_is_measured_and_does_not_split_the_note() {
        let midi_at = |t: f64| 64.0 + 0.5 * (std::f64::consts::TAU * 5.5 * t).sin();
        let notes = notes_of(&synth(RATE, 1.5, &VOICE, midi_at, |_| 0.4));
        assert_eq!(notes.len(), 1, "{notes:#?}");
        let note = &notes[0];
        assert_eq!(note.name, "E4");
        assert!(note.deviation_cents.abs() < 6.0, "{}", note.deviation_cents);
        let vibrato = note.vibrato.expect("vibrato");
        assert!((vibrato.rate_hz - 5.5).abs() < 0.15, "{vibrato:?}");
        assert!((vibrato.extent_cents - 50.0).abs() < 3.0, "{vibrato:?}");
        // With the vibrato accounted for, the note is otherwise steady.
        assert!(note.steadiness_cents.unwrap() < 3.0, "{note:?}");
        assert!(note.drift_cents.unwrap().abs() < 3.0, "{note:?}");
    }

    #[test]
    fn drift_and_wander_are_reported_separately() {
        // Falls 30 cents a second, with an irregular wobble on top.
        let midi_at = |t: f64| {
            62.0 - 0.3 * t
                + 0.06 * (std::f64::consts::TAU * 2.3 * t).sin()
                + 0.05 * (std::f64::consts::TAU * 3.1 * t + 1.0).sin()
        };
        let notes = notes_of(&synth(RATE, 1.0, &VOICE, midi_at, |_| 0.4));
        assert_eq!(notes.len(), 1);
        let note = &notes[0];
        assert!(note.vibrato.is_none(), "{note:?}");
        let drift = note.drift_cents.unwrap();
        // The measured middle is 0.6 s long.
        assert!((-28.0..=-9.0).contains(&drift), "drift {drift}");
        let steadiness = note.steadiness_cents.unwrap();
        assert!((3.0..=9.0).contains(&steadiness), "steadiness {steadiness}");
    }

    #[test]
    fn very_short_sounds_are_not_notes() {
        let mut samples = synth(RATE, 0.05, &VOICE, |_| 60.0, |_| 0.4);
        samples.extend(vec![0.0; RATE as usize / 2]);
        assert!(notes_of(&samples).is_empty());
        assert!(find_notes(&track(RATE, &[])).is_empty());
    }
}
