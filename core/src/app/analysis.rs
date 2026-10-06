//! Pitch analysis as the UI sees it: background jobs, the results, the
//! curves to draw and the exports.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use uuid::Uuid;

use super::{comparison, AppCore, Inner, PROGRESS_INTERVAL};
use crate::analysis::indicators::IndicatorReport;
use crate::analysis::notes::{describe_pitch, Note};
use crate::analysis::pitch::{hz_to_midi, PitchTrack};
use crate::analysis::{analyse_file, Analysis, PitchCurve, PitchSummary};
use crate::error::{AppError, CoreError, UserError};
use crate::export::{self, ComparedWith, ExportContext, ExportFormat, FULL_MIX_CAUTION};
use crate::project::{AnalysisSource, SourceKind};
use crate::session::SessionView;

/// Upper bound on points per pitch-curve request.
const MAX_CURVE_COLUMNS: u32 = 16_384;
/// A cached pitch track up to this long (ten minutes) is turned into notes
/// before the open returns, so the pitch is on screen in the first frame.
const SYNCHRONOUS_FRAMES: usize = 60_000;
/// Semitones of headroom above and below the sung range when drawing.
const DISPLAY_MARGIN_SEMITONES: f32 = 2.5;
/// The range shown when there are no notes to fit: C3 to C5.
const DEFAULT_DISPLAY_RANGE: (f32, f32) = (48.0, 72.0);
/// Never show less than an octave; small ranges would magnify noise.
const MIN_DISPLAY_SPAN_SEMITONES: f32 = 12.0;

/// The finished pitch analysis of one recording.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct AnalysisView {
    pub recording_id: Uuid,
    /// The analysis was made from the vocals VocalScope isolated, rather
    /// than from the recording as it is.
    pub isolated_vocals: bool,
    /// Shown with the results when they were made from what may be a full
    /// mix.
    pub caution: Option<String>,
    pub summary: PitchSummary,
    pub indicators: IndicatorReport,
    pub notes: Vec<Note>,
    /// The vertical range worth drawing, as MIDI note numbers.
    pub display_low_midi: f32,
    pub display_high_midi: f32,
}

/// The pitch at one moment, for a read-out under the pointer.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PitchReading {
    pub frequency_hz: f32,
    pub midi_pitch: f32,
    /// Nearest note, e.g. `A4`.
    pub note_name: String,
    /// Distance from that note in cents; positive is sharp.
    pub deviation_cents: f32,
    pub confidence: f32,
}

fn display_range(notes: &[Note]) -> (f32, f32) {
    let low = notes.iter().map(|n| n.midi_pitch).fold(f32::MAX, f32::min);
    let high = notes.iter().map(|n| n.midi_pitch).fold(f32::MIN, f32::max);
    if notes.is_empty() {
        return DEFAULT_DISPLAY_RANGE;
    }
    let mut low = (low - DISPLAY_MARGIN_SEMITONES).floor();
    let mut high = (high + DISPLAY_MARGIN_SEMITONES).ceil();
    let missing = MIN_DISPLAY_SPAN_SEMITONES - (high - low);
    if missing > 0.0 {
        low -= (missing / 2.0).ceil();
        high += (missing / 2.0).ceil();
    }
    (low, high)
}

/// Stores a finished analysis (or its failure), brings the comparison up to
/// date and tells the UI.
fn finish_analysis_job(
    inner: &Arc<Inner>,
    recording_id: Uuid,
    cancel: &Arc<AtomicBool>,
    outcome: Result<Arc<Analysis>, UserError>,
    isolated_vocals: bool,
    notify: bool,
) {
    let stored = inner
        .session()
        .finish_analysis(recording_id, cancel, outcome, isolated_vocals);
    if !stored {
        return;
    }
    comparison::refresh_pitch_comparison(inner);
    if notify {
        inner.publish_session();
    }
}

/// Gets a recording's pitch analysis: from the cache when the same audio was
/// analysed before (stored before returning, and announced only if
/// `notify_when_cached`), otherwise on a background thread.
pub(super) fn start_analysis_job(inner: &Arc<Inner>, recording_id: Uuid, notify_when_cached: bool) {
    let input = inner.session().analysis_input(recording_id);
    let Some((input, isolated_vocals)) = input.filter(|(path, _)| path.exists()) else {
        return;
    };
    let cancel = inner.session().begin_analysis(recording_id);
    let cache_file = inner.paths.analysis_cache_file(&input);

    let started = Instant::now();
    if let Some(track) = cache_file.as_deref().and_then(PitchTrack::load) {
        let long = track.len() > SYNCHRONOUS_FRAMES;
        let inner_for_job = inner.clone();
        let job_cancel = cancel.clone();
        let finish = move |notify: bool| {
            let analysis = Arc::new(Analysis::from_track(track));
            log::info!(
                "pitch analysis ready: {} notes in {:.1} ms (cached)",
                analysis.notes.len(),
                started.elapsed().as_secs_f64() * 1000.0
            );
            finish_analysis_job(
                &inner_for_job,
                recording_id,
                &job_cancel,
                Ok(analysis),
                isolated_vocals,
                notify,
            );
        };
        if long {
            // Finding the notes of a long recording takes a noticeable
            // fraction of a second; do not hold the open up for it.
            if let Err(err) = std::thread::Builder::new()
                .name("analysis".into())
                .spawn(move || finish(true))
            {
                log::error!("could not start the analysis thread: {err}");
            }
        } else {
            finish(notify_when_cached);
        }
        return;
    }

    let inner = inner.clone();
    let spawned = std::thread::Builder::new()
        .name("analysis".into())
        .spawn(move || {
            let mut last_report: Option<Instant> = None;
            let result = analyse_file(&input, &cancel, |fraction| {
                if last_report.is_none_or(|at| at.elapsed() >= PROGRESS_INTERVAL) {
                    last_report = Some(Instant::now());
                    inner.observer.analysis_progress(recording_id, fraction);
                }
            });
            let outcome = match result {
                Err(AppError::Cancelled) => return,
                Ok(track) => {
                    if let Some(cache_file) = &cache_file {
                        if let Err(err) = track.save(cache_file) {
                            log::warn!("could not cache the pitch track: {err}");
                        }
                    }
                    let analysis = Analysis::from_track(track);
                    log::info!(
                        "pitch analysis ready: {:.1} s of audio, {} notes, in {} ms",
                        analysis.track.duration_seconds(),
                        analysis.notes.len(),
                        started.elapsed().as_millis()
                    );
                    Ok(Arc::new(analysis))
                }
                Err(err) => {
                    log::error!("pitch analysis failed: {err}");
                    Err(err.to_user_error())
                }
            };
            finish_analysis_job(
                &inner,
                recording_id,
                &cancel,
                outcome,
                isolated_vocals,
                true,
            );
        });
    if let Err(err) = spawned {
        log::error!("could not start the analysis thread: {err}");
    }
}

#[uniffi::export]
impl AppCore {
    /// The finished pitch analysis of a recording, or `None` while it is
    /// still being made (see `RecordingRuntime::analysis_status`).
    pub fn analysis(&self, recording_id: Uuid) -> Option<AnalysisView> {
        let session = self.inner.session();
        let analysis = session.analysis(recording_id)?;
        let recording = session.project()?.recording(recording_id)?;
        let isolated_vocals = session.analysis_used_vocals(recording_id);
        let (display_low_midi, display_high_midi) = display_range(&analysis.notes);
        Some(AnalysisView {
            recording_id,
            isolated_vocals,
            caution: (!isolated_vocals && recording.source.kind != SourceKind::VocalStem)
                .then(|| FULL_MIX_CAUTION.to_string()),
            summary: analysis.summary.clone(),
            indicators: analysis.indicators.clone(),
            notes: analysis.notes.clone(),
            display_low_midi,
            display_high_midi,
        })
    }

    /// The pitch curve between two times, at no more than `columns` points,
    /// for drawing. Empty until the analysis is ready.
    pub fn pitch_curve(
        &self,
        recording_id: Uuid,
        start_seconds: f64,
        end_seconds: f64,
        columns: u32,
    ) -> PitchCurve {
        let empty = PitchCurve {
            start_seconds,
            step_seconds: crate::analysis::pitch::HOP_SECONDS,
            midi: Vec::new(),
        };
        if !start_seconds.is_finite() || !end_seconds.is_finite() {
            return empty;
        }
        let Some(analysis) = self.inner.session().analysis(recording_id) else {
            return empty;
        };
        analysis.curve(
            start_seconds.max(0.0),
            end_seconds,
            columns.min(MAX_CURVE_COLUMNS) as usize,
            |t| t,
        )
    }

    /// The pitch at one moment, or `None` where there is none.
    pub fn pitch_at(&self, recording_id: Uuid, seconds: f64) -> Option<PitchReading> {
        let analysis = self.inner.session().analysis(recording_id)?;
        if !(seconds >= 0.0) {
            return None;
        }
        let frame = (seconds / analysis.track.hop_seconds).round() as usize;
        let frequency_hz = *analysis.track.frequency_hz.get(frame)?;
        if frequency_hz <= 0.0 {
            return None;
        }
        let midi_pitch = hz_to_midi(frequency_hz);
        let (note_name, deviation_cents) = describe_pitch(midi_pitch);
        Some(PitchReading {
            frequency_hz,
            midi_pitch,
            note_name,
            deviation_cents,
            confidence: analysis.track.confidence[frame],
        })
    }

    /// Chooses what the pitch analysis listens to and runs it again if that
    /// changes its input.
    pub fn set_analysis_source(
        &self,
        recording_id: Uuid,
        source: AnalysisSource,
    ) -> Result<SessionView, CoreError> {
        let before = self.inner.session().analysis_input(recording_id);
        self.inner
            .session()
            .set_analysis_source(recording_id, source)
            .map_err(CoreError::from)?;
        if self.inner.session().analysis_input(recording_id) != before {
            start_analysis_job(&self.inner, recording_id, false);
        }
        Ok(self.inner.publish_session())
    }

    /// Runs a recording's pitch analysis again from its audio, ignoring any
    /// cached result.
    pub fn reanalyse(&self, recording_id: Uuid) -> Result<SessionView, CoreError> {
        let input = self
            .inner
            .session()
            .analysis_input(recording_id)
            .ok_or_else(|| AppError::RecordingNotFound(recording_id.to_string()))?;
        if let Some(cache_file) = self.inner.paths.analysis_cache_file(&input.0) {
            let _ = std::fs::remove_file(cache_file);
        }
        start_analysis_job(&self.inner, recording_id, false);
        Ok(self.inner.publish_session())
    }

    /// Writes a recording's analysis to a file. The format's extension is
    /// added when `path` lacks it. Returns the path written.
    pub fn export_analysis(
        &self,
        recording_id: Uuid,
        format: ExportFormat,
        path: PathBuf,
    ) -> Result<PathBuf, CoreError> {
        let has_extension = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case(format.extension()));
        let path = if has_extension {
            path
        } else {
            let mut name = path.into_os_string();
            name.push(".");
            name.push(format.extension());
            PathBuf::from(name)
        };

        // Copy what the export needs out of the session, then write without
        // holding its lock.
        let (recording, analysis, isolated_vocals, isolation_model, other) = {
            let session = self.inner.session();
            let recording = session
                .project()
                .ok_or(AppError::NoProject)?
                .recording(recording_id)
                .cloned()
                .ok_or_else(|| AppError::RecordingNotFound(recording_id.to_string()))?;
            let analysis = session.analysis(recording_id).ok_or_else(|| {
                AppError::AnalysisUnavailable("the analysis has not finished".into())
            })?;
            let isolated_vocals = session.analysis_used_vocals(recording_id);
            let isolation_model = isolated_vocals
                .then(|| session.stem(recording_id).map(|s| s.model_name.clone()))
                .flatten();
            let view = session.view();
            let other = view.comparison.map(|comparison| {
                let exported_is_reference = comparison.reference_recording_id == recording_id;
                let other_id = if exported_is_reference {
                    comparison.other_recording_id
                } else {
                    comparison.reference_recording_id
                };
                let title = view
                    .recordings
                    .iter()
                    .find(|r| r.recording_id == other_id)
                    .map(|r| r.display_title.clone())
                    .unwrap_or_default();
                (
                    title,
                    exported_is_reference,
                    comparison.alignment,
                    comparison.pitch,
                )
            });
            (recording, analysis, isolated_vocals, isolation_model, other)
        };
        let context = ExportContext {
            recording: &recording,
            analysis: &analysis,
            isolated_vocals,
            isolation_model,
            compared_with: other
                .as_ref()
                .map(|(title, is_reference, alignment, pitch)| ComparedWith {
                    title: title.clone(),
                    exported_is_reference: *is_reference,
                    alignment: *alignment,
                    pitch: pitch.as_ref(),
                }),
            exported_at: Utc::now(),
        };
        export::write(format, &context, &path).map_err(CoreError::from)?;
        log::info!("exported {format:?}");
        Ok(path)
    }

    /// A file name to offer for an export, without a folder: the
    /// recording's name, what the export holds, and the extension.
    pub fn suggested_export_name(&self, recording_id: Uuid, format: ExportFormat) -> String {
        let title = self
            .inner
            .session()
            .project()
            .and_then(|project| project.recording(recording_id))
            .map(|recording| recording.display_title())
            .unwrap_or_else(|| "VocalScope".to_string());
        // Characters no file system objects to.
        let safe: String = title
            .chars()
            .map(|c| {
                if "/\\:*?\"<>|".contains(c) || c.is_control() {
                    '-'
                } else {
                    c
                }
            })
            .collect();
        format!(
            "{}{}.{}",
            safe.trim(),
            format.name_suffix(),
            format.extension()
        )
    }
}

/// The file extension of an export format, without the dot.
#[uniffi::export]
pub fn export_file_extension(format: ExportFormat) -> String {
    format.extension().to_string()
}
