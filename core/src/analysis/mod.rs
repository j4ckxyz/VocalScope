//! Pitch analysis: the pitch curve, the notes in it, and what they suggest.
//!
//! [`analyse_file`] makes one streaming pass over a file and produces a
//! [`PitchTrack`]. Everything else here — notes, the summary, the correction
//! indicators — is derived from the track in a few milliseconds, so only the
//! track is cached on disk.

pub mod compare;
pub mod indicators;
pub mod notes;
pub mod pitch;

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::audio::decode::AudioReader;
use crate::error::{AppError, AppResult};
use indicators::IndicatorReport;
use notes::Note;
use pitch::{PitchAnalyzer, PitchTrack};

const CACHE_MAGIC: &[u8; 4] = b"VSPT";
/// Bump whenever the tracker changes in a way that alters its output.
const CACHE_VERSION: u32 = 1;

/// Overall facts about the pitch in a recording.
#[derive(Debug, Clone, Serialize, PartialEq, uniffi::Record)]
pub struct PitchSummary {
    /// Total time with a detectable pitch.
    pub voiced_seconds: f64,
    pub note_count: u32,
    /// Lowest and highest note centres, e.g. `A2` and `E5`.
    pub lowest_note: Option<String>,
    pub highest_note: Option<String>,
    pub lowest_frequency_hz: Option<f32>,
    pub highest_frequency_hz: Option<f32>,
    /// Median pitch of all voiced frames.
    pub median_frequency_hz: Option<f32>,
    pub median_note: Option<String>,
    /// How far the scale the singer follows sits from A4 = 440 Hz, in cents.
    pub tuning_offset_cents: Option<f32>,
    /// The same expressed as the frequency of A4.
    pub reference_pitch_hz: Option<f32>,
}

/// A stretch of pitch curve resampled for drawing.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PitchCurve {
    /// Time of the first value.
    pub start_seconds: f64,
    /// Spacing between values.
    pub step_seconds: f64,
    /// Fractional MIDI note numbers (69 = A4); `NaN` where there is no pitch.
    pub midi: Vec<f32>,
}

/// Everything known about the pitch of one recording.
#[derive(Debug, Clone, PartialEq)]
pub struct Analysis {
    pub track: PitchTrack,
    pub notes: Vec<Note>,
    pub summary: PitchSummary,
    pub indicators: IndicatorReport,
}

impl Analysis {
    pub fn from_track(track: PitchTrack) -> Self {
        let notes = notes::find_notes(&track);
        let indicators = indicators::assess(&notes);
        let summary = summarise(&track, &notes);
        Self {
            track,
            notes,
            summary,
            indicators,
        }
    }

    /// Median pitch (MIDI) of the voiced frames between two times, or `NaN`
    /// when fewer than half of them are voiced.
    fn sample(&self, from_seconds: f64, to_seconds: f64, scratch: &mut Vec<f32>) -> f32 {
        let hop = self.track.hop_seconds;
        let count = self.track.len();
        if count == 0 || !(to_seconds >= from_seconds) || to_seconds < 0.0 {
            return f32::NAN;
        }
        // Frame i covers the hop centred on i * hop.
        let first = ((from_seconds / hop + 0.5).floor().max(0.0)) as usize;
        let last = ((to_seconds / hop + 0.5).floor().max(0.0) as usize).max(first + 1);
        if first >= count {
            return f32::NAN;
        }
        let last = last.min(count);
        scratch.clear();
        scratch.extend((first..last).filter_map(|i| self.track.midi(i)));
        if scratch.is_empty() || scratch.len() * 2 < last - first {
            return f32::NAN;
        }
        let mid = scratch.len() / 2;
        *scratch.select_nth_unstable_by(mid, f32::total_cmp).1
    }

    /// The pitch curve between two times at no more than `columns` points,
    /// ready to draw. `map` converts a time on the caller's timeline into a
    /// time in this recording (the identity unless two recordings are being
    /// compared).
    pub fn curve(
        &self,
        start_seconds: f64,
        end_seconds: f64,
        columns: usize,
        map: impl Fn(f64) -> f64,
    ) -> PitchCurve {
        let span = end_seconds - start_seconds;
        if !(span > 0.0) || columns == 0 || self.track.is_empty() {
            return PitchCurve {
                start_seconds,
                step_seconds: self.track.hop_seconds,
                midi: Vec::new(),
            };
        }
        // Never finer than the track itself.
        let hop = self.track.hop_seconds;
        let step = (span / columns as f64).max(hop);
        let first = (start_seconds / step).floor();
        let count = (end_seconds / step).ceil() - first;
        let mut scratch = Vec::new();
        let midi = (0..count.max(0.0) as usize)
            .map(|i| {
                // Each point summarises the step centred on it.
                let centre = (first + i as f64) * step;
                self.sample(
                    map(centre - step / 2.0),
                    map(centre + step / 2.0),
                    &mut scratch,
                )
            })
            .collect();
        PitchCurve {
            start_seconds: first * step,
            step_seconds: step,
            midi,
        }
    }

    /// The note sounding at a time, if any.
    pub fn note_at(&self, seconds: f64) -> Option<&Note> {
        let index = self.notes.partition_point(|n| n.end_seconds <= seconds);
        self.notes.get(index).filter(|n| n.start_seconds <= seconds)
    }
}

fn summarise(track: &PitchTrack, notes: &[Note]) -> PitchSummary {
    let mut voiced: Vec<f32> = track
        .frequency_hz
        .iter()
        .copied()
        .filter(|hz| *hz > 0.0)
        .collect();
    let median_frequency_hz = (!voiced.is_empty()).then(|| {
        let mid = voiced.len() / 2;
        *voiced.select_nth_unstable_by(mid, f32::total_cmp).1
    });
    let lowest = notes
        .iter()
        .min_by(|a, b| a.midi_pitch.total_cmp(&b.midi_pitch));
    let highest = notes
        .iter()
        .max_by(|a, b| a.midi_pitch.total_cmp(&b.midi_pitch));
    let tuning = indicators::estimate_tuning(notes);
    PitchSummary {
        voiced_seconds: voiced.len() as f64 * track.hop_seconds,
        note_count: notes.len() as u32,
        lowest_note: lowest.map(|n| n.name.clone()),
        highest_note: highest.map(|n| n.name.clone()),
        lowest_frequency_hz: lowest.map(|n| n.frequency_hz),
        highest_frequency_hz: highest.map(|n| n.frequency_hz),
        median_frequency_hz,
        median_note: median_frequency_hz.map(|hz| notes::describe_pitch(pitch::hz_to_midi(hz)).0),
        tuning_offset_cents: tuning.map(|t| t.offset_cents as f32),
        reference_pitch_hz: tuning.map(|t| (440.0 * 2f64.powf(t.offset_cents / 1200.0)) as f32),
    }
}

/// Decodes `path` and tracks its pitch. `on_progress` receives 0–1, or
/// `None` when the file's length is not known in advance.
pub fn analyse_file(
    path: &Path,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(Option<f32>),
) -> AppResult<PitchTrack> {
    let mut reader = AudioReader::open(path)?;
    reader.prime()?;
    let rate = reader.sample_rate().ok_or_else(|| AppError::Decode {
        path: path.to_path_buf(),
        details: "sample rate could not be determined".into(),
    })?;
    let channels = reader.channel_count().unwrap_or(1).max(1);
    let declared = reader.declared_frame_count();

    let mut analyzer = PitchAnalyzer::new(rate, channels);
    let mut frames_read = 0u64;
    while let Some(block) = reader.next_block()? {
        if cancel.load(Ordering::Relaxed) {
            return Err(AppError::Cancelled);
        }
        analyzer.push(block);
        frames_read += (block.len() / channels as usize) as u64;
        on_progress(
            declared
                .filter(|total| *total > 0)
                .map(|total| (frames_read as f32 / total as f32).min(1.0)),
        );
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(AppError::Cancelled);
    }
    Ok(analyzer.finish())
}

impl PitchTrack {
    /// Writes the track to a compact cache file.
    pub fn save(&self, path: &Path) -> AppResult<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut w = BufWriter::new(File::create(&tmp)?);
            w.write_all(CACHE_MAGIC)?;
            w.write_all(&CACHE_VERSION.to_le_bytes())?;
            w.write_all(&self.hop_seconds.to_le_bytes())?;
            w.write_all(&(self.len() as u64).to_le_bytes())?;
            for values in [&self.frequency_hz, &self.confidence, &self.level_db] {
                for value in values {
                    w.write_all(&value.to_le_bytes())?;
                }
            }
            w.flush()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Loads a file written by [`PitchTrack::save`]; `None` if it is missing,
    /// damaged or from a different version of the tracker.
    pub fn load(path: &Path) -> Option<Self> {
        let mut r = BufReader::new(File::open(path).ok()?);
        let mut four = [0u8; 4];
        r.read_exact(&mut four).ok()?;
        if &four != CACHE_MAGIC {
            return None;
        }
        r.read_exact(&mut four).ok()?;
        if u32::from_le_bytes(four) != CACHE_VERSION {
            return None;
        }
        let mut eight = [0u8; 8];
        r.read_exact(&mut eight).ok()?;
        let hop_seconds = f64::from_le_bytes(eight);
        r.read_exact(&mut eight).ok()?;
        let count = u64::from_le_bytes(eight) as usize;
        // A day of audio; anything larger is a damaged file.
        if !(hop_seconds > 0.0) || count > 24 * 3600 * 1000 {
            return None;
        }
        let mut read_values = || -> Option<Vec<f32>> {
            let mut bytes = vec![0u8; count * 4];
            r.read_exact(&mut bytes).ok()?;
            Some(
                bytes
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
            )
        };
        let frequency_hz = read_values()?;
        let confidence = read_values()?;
        let level_db = read_values()?;
        Some(Self {
            hop_seconds,
            frequency_hz,
            confidence,
            level_db,
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;

    use super::pitch::test_support::{noise, synth};

    /// Harmonics of a plain sung-sounding tone.
    pub const VOICE: [f64; 5] = [1.0, 0.6, 0.4, 0.25, 0.15];

    /// A short phrase: three notes joined by glides, a breath, two more.
    pub fn phrase(rate: u32) -> Vec<f32> {
        let melody = |t: f64| {
            let glide = |at: f64| ((t - at) / 0.08).clamp(0.0, 1.0);
            60.0 + 2.0 * glide(0.5) + 2.0 * glide(1.0)
        };
        let mut samples = synth(rate, 1.5, &VOICE, melody, |_| 0.4);
        samples.extend(noise(rate as usize / 4, 5).iter().map(|s| s * 0.01));
        let second = |t: f64| 67.0 - 3.0 * ((t - 0.6) / 0.08).clamp(0.0, 1.0);
        samples.extend(synth(rate, 1.2, &VOICE, second, |_| 0.35));
        samples
    }

    pub fn write_wav(path: &Path, rate: u32, channels: &[&[f32]]) {
        let spec = hound::WavSpec {
            channels: channels.len() as u16,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..channels[0].len() {
            for channel in channels {
                writer
                    .write_sample((channel[i].clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
                    .unwrap();
            }
        }
        writer.finalize().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::pitch::test_support::track;
    use super::test_support::*;
    use super::*;

    const RATE: u32 = 44_100;

    fn analysis() -> Analysis {
        Analysis::from_track(track(RATE, &phrase(RATE)))
    }

    #[test]
    fn a_phrase_is_summarised() {
        let analysis = analysis();
        let names: Vec<&str> = analysis.notes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["C4", "D4", "E4", "G4", "E4"]);
        let summary = &analysis.summary;
        assert_eq!(summary.note_count, 5);
        assert_eq!(summary.lowest_note.as_deref(), Some("C4"));
        assert_eq!(summary.highest_note.as_deref(), Some("G4"));
        assert!((summary.voiced_seconds - 2.7).abs() < 0.1, "{summary:?}");
        assert!((summary.lowest_frequency_hz.unwrap() - 261.63).abs() < 0.2);
        assert_eq!(summary.median_note.as_deref(), Some("E4"));
        assert!(summary.tuning_offset_cents.unwrap().abs() < 1.0);
        assert!((summary.reference_pitch_hz.unwrap() - 440.0).abs() < 0.3);
        // Three joined moves, all taking about 50 ms.
        let timed: Vec<f32> = analysis
            .notes
            .iter()
            .filter_map(|n| n.transition_in_ms)
            .collect();
        assert_eq!(timed.len(), 3);
        assert!(
            timed.iter().all(|ms| (30.0..70.0).contains(ms)),
            "{timed:?}"
        );
        assert_eq!(analysis.note_at(0.2).unwrap().name, "C4");
        assert_eq!(analysis.note_at(2.0).unwrap().name, "G4");
        assert!(analysis.note_at(1.6).is_none());
        assert!(analysis.note_at(99.0).is_none());
    }

    #[test]
    fn curves_are_resampled_for_drawing() {
        let analysis = analysis();
        // Zoomed in: one value per frame, never more.
        let fine = analysis.curve(0.1, 0.3, 4_000, |t| t);
        assert_eq!(fine.step_seconds, pitch::HOP_SECONDS);
        assert!((fine.start_seconds - 0.1).abs() < 1e-9);
        assert_eq!(fine.midi.len(), 20);
        assert!(fine.midi.iter().all(|m| (m - 60.0).abs() < 0.02));

        // Zoomed out: one value per column, gaps kept as gaps.
        let coarse = analysis.curve(0.0, 3.0, 30, |t| t);
        assert_eq!(coarse.midi.len(), 30);
        assert!((coarse.step_seconds - 0.1).abs() < 1e-9);
        assert!((coarse.midi[2] - 60.0).abs() < 0.02);
        assert!(coarse.midi[16].is_nan(), "the breath should be a gap");
        assert!((coarse.midi[19] - 67.0).abs() < 0.02);

        // Through a time mapping, as when another recording is overlaid.
        let shifted = analysis.curve(10.0, 13.0, 30, |t| t - 10.0);
        assert_eq!(shifted.start_seconds, 10.0);
        for (a, b) in shifted.midi.iter().zip(&coarse.midi) {
            assert!(a.is_nan() && b.is_nan() || (a - b).abs() < 1e-3);
        }

        assert!(analysis.curve(1.0, 1.0, 100, |t| t).midi.is_empty());
        assert!(analysis.curve(0.0, 1.0, 0, |t| t).midi.is_empty());
        assert!(analysis
            .curve(50.0, 51.0, 10, |t| t)
            .midi
            .iter()
            .all(|m| m.is_nan()));
    }

    #[test]
    fn analyses_a_file_with_progress_and_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phrase.wav");
        let samples = phrase(RATE);
        write_wav(&path, RATE, &[&samples, &samples]);

        let mut reports = Vec::new();
        let from_file = analyse_file(&path, &AtomicBool::new(false), |f| reports.push(f)).unwrap();
        assert!(reports.len() > 3);
        assert_eq!(*reports.last().unwrap(), Some(1.0));
        let direct = track(RATE, &samples);
        assert_eq!(from_file.len(), direct.len());
        // 16-bit quantisation is the only difference.
        for (a, b) in from_file.frequency_hz.iter().zip(&direct.frequency_hz) {
            assert!((a - b).abs() < 0.05, "{a} vs {b}");
        }

        let cancelled = analyse_file(&path, &AtomicBool::new(true), |_| {});
        assert!(matches!(cancelled.unwrap_err(), AppError::Cancelled));
        let missing = analyse_file(&dir.path().join("no.wav"), &AtomicBool::new(false), |_| {});
        assert!(matches!(missing.unwrap_err(), AppError::FileNotFound(_)));
    }

    #[test]
    fn tracks_survive_the_cache_and_bad_files_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("a.vspt");
        let track = track(RATE, &phrase(RATE));
        track.save(&file).unwrap();
        assert_eq!(PitchTrack::load(&file).unwrap(), track);

        let bytes = std::fs::read(&file).unwrap();
        std::fs::write(&file, &bytes[..bytes.len() - 7]).unwrap();
        assert!(PitchTrack::load(&file).is_none(), "truncated");
        let mut wrong = bytes.clone();
        wrong[4] = 99;
        std::fs::write(&file, &wrong).unwrap();
        assert!(PitchTrack::load(&file).is_none(), "other version");
        std::fs::write(&file, b"nope").unwrap();
        assert!(PitchTrack::load(&file).is_none());
        assert!(PitchTrack::load(&dir.path().join("absent")).is_none());
    }

    #[test]
    fn an_empty_track_has_an_empty_analysis() {
        let analysis = Analysis::from_track(track(RATE, &[]));
        assert!(analysis.notes.is_empty());
        assert_eq!(analysis.summary.note_count, 0);
        assert_eq!(analysis.summary.median_frequency_hz, None);
        assert_eq!(analysis.summary.lowest_note, None);
        assert!(analysis.curve(0.0, 1.0, 10, |t| t).midi.is_empty());
    }
}
