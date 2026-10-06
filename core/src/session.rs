//! The open project and its runtime state.
//!
//! This is plain data and logic with no dependency on any UI or thread, so
//! it can be unit-tested. `app` wraps it with background jobs and callbacks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Serialize;
use uuid::Uuid;

use crate::analysis::compare::{Alignment, PitchComparison};
use crate::analysis::Analysis;
use crate::audio::peaks::{Peaks, WaveformSummary};
use crate::error::{AppError, AppResult, UserError};
use crate::project::{AnalysisSource, Project, Recording, RecordingLabel, SourceKind};

/// A project holds one recording, or two for comparison.
pub const MAX_RECORDINGS: usize = 2;

/// Where a recording's waveform summary stands.
enum WaveformSlot {
    /// A background decode is running; setting the flag cancels it.
    Pending(Arc<AtomicBool>),
    Ready(Arc<Peaks>),
    Failed(UserError),
}

/// Where a recording's pitch analysis stands.
enum AnalysisSlot {
    Pending(Arc<AtomicBool>),
    Ready {
        analysis: Arc<Analysis>,
        /// Whether it was made from the isolated vocals.
        isolated_vocals: bool,
    },
    Failed(UserError),
}

#[derive(Default)]
pub struct Session {
    project: Option<Project>,
    project_path: Option<PathBuf>,
    dirty: bool,
    active_recording_id: Option<Uuid>,
    waveforms: HashMap<Uuid, WaveformSlot>,
    analyses: HashMap<Uuid, AnalysisSlot>,
    /// Isolated vocals found for each recording.
    stems: HashMap<Uuid, VocalStem>,
    /// Play the isolated vocals instead of the recordings themselves.
    listening_to_vocals: bool,
    alignment: Option<Alignment>,
    pitch_comparison: Option<PitchComparison>,
}

/// Where a recording's pitch analysis stands, as the UI sees it.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisStatus {
    /// Not started (the waveform is not ready, or the file is missing).
    Unavailable,
    Pending,
    Ready,
    Failed,
}

/// The vocals VocalScope isolated from a recording.
#[derive(Debug, Clone, Serialize, PartialEq, uniffi::Record)]
pub struct VocalStem {
    pub model_id: String,
    pub model_name: String,
    pub path: PathBuf,
}

/// Two versions of a recording, lined up.
#[derive(Debug, Clone, Serialize, PartialEq, uniffi::Record)]
pub struct ComparisonView {
    /// The first recording in the project; times in `pitch` are on its
    /// timeline.
    pub reference_recording_id: Uuid,
    pub other_recording_id: Uuid,
    /// `None` until both waveforms are ready, or when the recordings are too
    /// short to line up.
    pub alignment: Option<Alignment>,
    /// `None` until both pitch analyses are ready.
    pub pitch: Option<PitchComparison>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum WaveformStatus {
    /// Not started (for example the source file is missing).
    Unavailable,
    Pending,
    Ready,
    Failed,
}

/// Per-recording state that is not part of the saved project.
#[derive(Debug, Clone, Serialize, PartialEq, uniffi::Record)]
pub struct RecordingRuntime {
    pub recording_id: Uuid,
    pub display_title: String,
    pub display_detail: Option<String>,
    pub source_exists: bool,
    pub waveform_status: WaveformStatus,
    pub waveform_error: Option<UserError>,
    pub analysis_status: AnalysisStatus,
    pub analysis_error: Option<UserError>,
    /// The finished analysis was made from the isolated vocals rather than
    /// the recording itself.
    pub analysed_isolated_vocals: bool,
    pub vocal_stem: Option<VocalStem>,
}

/// Everything the UI needs to draw the current document.
#[derive(Debug, Clone, Serialize, PartialEq, uniffi::Record)]
pub struct SessionView {
    pub project: Option<Project>,
    pub project_path: Option<PathBuf>,
    pub project_file_name: Option<String>,
    /// Unsaved changes exist.
    pub dirty: bool,
    pub active_recording_id: Option<Uuid>,
    pub recordings: Vec<RecordingRuntime>,
    /// Playback is of the isolated vocals rather than the recordings.
    pub listening_to_vocals: bool,
    /// Present when the project holds two recordings.
    pub comparison: Option<ComparisonView>,
}

impl Session {
    pub fn project(&self) -> Option<&Project> {
        self.project.as_ref()
    }

    pub fn project_path(&self) -> Option<&Path> {
        self.project_path.as_deref()
    }

    pub fn has_project(&self) -> bool {
        self.project.is_some()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn active_recording(&self) -> Option<&Recording> {
        let id = self.active_recording_id?;
        self.project.as_ref()?.recording(id)
    }

    fn project_mut(&mut self) -> AppResult<&mut Project> {
        self.project.as_mut().ok_or(AppError::NoProject)
    }

    fn recording_mut(&mut self, id: Uuid) -> AppResult<&mut Recording> {
        self.project_mut()?
            .recording_mut(id)
            .ok_or_else(|| AppError::RecordingNotFound(id.to_string()))
    }

    /// Replaces whatever is open. Any background work for the previous
    /// project is cancelled.
    pub fn open(&mut self, project: Project, project_path: Option<PathBuf>, dirty: bool) {
        self.close();
        self.active_recording_id = project.recordings.first().map(|r| r.id);
        self.project = Some(project);
        self.project_path = project_path;
        self.dirty = dirty;
    }

    pub fn close(&mut self) {
        self.cancel_all_jobs();
        self.project = None;
        self.project_path = None;
        self.dirty = false;
        self.active_recording_id = None;
        self.stems.clear();
        self.listening_to_vocals = false;
        self.alignment = None;
        self.pitch_comparison = None;
    }

    fn cancel_all_jobs(&mut self) {
        for (_, slot) in self.waveforms.drain() {
            if let WaveformSlot::Pending(cancel) = slot {
                cancel.store(true, Ordering::Relaxed);
            }
        }
        for (_, slot) in self.analyses.drain() {
            if let AnalysisSlot::Pending(cancel) = slot {
                cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Forgets everything derived from one recording's audio.
    fn forget_recording_state(&mut self, id: Uuid) {
        if let Some(WaveformSlot::Pending(cancel)) = self.waveforms.remove(&id) {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(AnalysisSlot::Pending(cancel)) = self.analyses.remove(&id) {
            cancel.store(true, Ordering::Relaxed);
        }
        self.stems.remove(&id);
        self.alignment = None;
        self.pitch_comparison = None;
    }

    // ── Recordings ─────────────────────────────────────────────────────

    /// Adds a second recording to compare with the first.
    pub fn add_recording(&mut self, recording: Recording) -> AppResult<()> {
        let project = self.project_mut()?;
        if project.recordings.len() >= MAX_RECORDINGS {
            return Err(AppError::InvalidInput(
                "A project compares two recordings. Remove one before adding another.".into(),
            ));
        }
        project.recordings.push(recording);
        self.dirty = true;
        Ok(())
    }

    /// Removes a recording. The project must keep at least one.
    pub fn remove_recording(&mut self, id: Uuid) -> AppResult<()> {
        let project = self.project_mut()?;
        let index = project
            .recordings
            .iter()
            .position(|r| r.id == id)
            .ok_or_else(|| AppError::RecordingNotFound(id.to_string()))?;
        if project.recordings.len() == 1 {
            return Err(AppError::InvalidInput(
                "This is the only recording in the project.".into(),
            ));
        }
        project.recordings.remove(index);
        let first = project.recordings.first().map(|r| r.id);
        self.forget_recording_state(id);
        if self.active_recording_id == Some(id) {
            self.active_recording_id = first;
        }
        self.dirty = true;
        Ok(())
    }

    pub fn set_active(&mut self, id: Uuid) -> AppResult<()> {
        self.recording_mut(id)?;
        self.active_recording_id = Some(id);
        Ok(())
    }

    /// The reference recording and the one compared with it.
    pub fn compared_pair(&self) -> Option<(Uuid, Uuid)> {
        match self.project.as_ref()?.recordings.as_slice() {
            [first, second, ..] => Some((first.id, second.id)),
            _ => None,
        }
    }

    /// The recording that is not `id`, when two are being compared.
    pub fn other_recording(&self, id: Uuid) -> Option<Uuid> {
        let (first, second) = self.compared_pair()?;
        if id == first {
            Some(second)
        } else if id == second {
            Some(first)
        } else {
            None
        }
    }

    pub fn set_analysis_source(&mut self, id: Uuid, source: AnalysisSource) -> AppResult<bool> {
        let recording = self.recording_mut(id)?;
        if recording.analysis_source == source {
            return Ok(false);
        }
        recording.analysis_source = source;
        self.dirty = true;
        Ok(true)
    }

    // ── Isolated vocals ────────────────────────────────────────────────

    pub fn set_stem(&mut self, id: Uuid, stem: Option<VocalStem>) {
        match stem {
            Some(stem) => {
                self.stems.insert(id, stem);
            }
            None => {
                self.stems.remove(&id);
            }
        }
        if self.stems.is_empty() {
            self.listening_to_vocals = false;
        }
    }

    pub fn stem(&self, id: Uuid) -> Option<&VocalStem> {
        self.stems.get(&id)
    }

    pub fn listening_to_vocals(&self) -> bool {
        self.listening_to_vocals
    }

    pub fn set_listening_to_vocals(&mut self, listening: bool) {
        self.listening_to_vocals = listening && !self.stems.is_empty();
    }

    /// The file playback should use for a recording: its isolated vocals
    /// when those are being listened to and exist, otherwise the recording.
    pub fn playback_path(&self, id: Uuid) -> Option<PathBuf> {
        let recording = self.project.as_ref()?.recording(id)?;
        if self.listening_to_vocals {
            if let Some(stem) = self.stems.get(&id) {
                return Some(stem.path.clone());
            }
        }
        Some(recording.source.path.clone())
    }

    /// The file the pitch analysis should read for a recording, and whether
    /// that is the isolated vocals.
    pub fn analysis_input(&self, id: Uuid) -> Option<(PathBuf, bool)> {
        let recording = self.project.as_ref()?.recording(id)?;
        match (recording.analysis_source, self.stems.get(&id)) {
            (AnalysisSource::IsolatedVocalsWhenAvailable, Some(stem)) => {
                Some((stem.path.clone(), true))
            }
            _ => Some((recording.source.path.clone(), false)),
        }
    }

    /// Records a successful save.
    pub fn mark_saved(&mut self, project: Project, path: PathBuf) {
        self.project = Some(project);
        self.project_path = Some(path);
        self.dirty = false;
    }

    pub fn set_label(
        &mut self,
        id: Uuid,
        label: RecordingLabel,
        kind: SourceKind,
    ) -> AppResult<()> {
        let label = label.normalized();
        let recording = self.recording_mut(id)?;
        if recording.label != label || recording.source.kind != kind {
            recording.label = label;
            recording.source.kind = kind;
            self.dirty = true;
        }
        Ok(())
    }

    /// Points a recording at a new file after the original went missing.
    /// `replacement` describes the newly chosen file; the recording keeps its
    /// identity and labels.
    pub fn relocate(&mut self, id: Uuid, replacement: Recording) -> AppResult<()> {
        self.forget_recording_state(id);
        let recording = self.recording_mut(id)?;
        let kind = recording.source.kind;
        recording.source = replacement.source;
        recording.source.kind = kind;
        recording.audio = replacement.audio;
        recording.waveform = None;
        self.dirty = true;
        Ok(())
    }

    /// Registers a waveform job for `id` and returns its cancellation flag.
    /// A job already running for the same recording is cancelled.
    pub fn begin_waveform(&mut self, id: Uuid) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(WaveformSlot::Pending(previous)) = self
            .waveforms
            .insert(id, WaveformSlot::Pending(cancel.clone()))
        {
            previous.store(true, Ordering::Relaxed);
        }
        cancel
    }

    /// Stores the outcome of a waveform job. Returns `false` (and stores
    /// nothing) when the job is stale: the project was closed, the recording
    /// was relocated, or a newer job replaced it.
    pub fn finish_waveform(
        &mut self,
        id: Uuid,
        job: &Arc<AtomicBool>,
        outcome: Result<Arc<Peaks>, UserError>,
    ) -> bool {
        let is_current = matches!(
            self.waveforms.get(&id),
            Some(WaveformSlot::Pending(current)) if Arc::ptr_eq(current, job)
        );
        if !is_current {
            return false;
        }
        match outcome {
            Ok(peaks) => {
                let summary = peaks.summary();
                if let Ok(recording) = self.recording_mut(id) {
                    apply_summary(recording, &summary);
                }
                self.waveforms.insert(id, WaveformSlot::Ready(peaks));
            }
            Err(error) => {
                self.waveforms.insert(id, WaveformSlot::Failed(error));
            }
        }
        true
    }

    pub fn peaks(&self, id: Uuid) -> Option<Arc<Peaks>> {
        match self.waveforms.get(&id) {
            Some(WaveformSlot::Ready(peaks)) => Some(peaks.clone()),
            _ => None,
        }
    }

    // ── Pitch analysis ─────────────────────────────────────────────────

    /// Registers an analysis job for `id` and returns its cancellation flag.
    /// A job already running for the same recording is cancelled, and any
    /// earlier result is dropped.
    pub fn begin_analysis(&mut self, id: Uuid) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(AnalysisSlot::Pending(previous)) = self
            .analyses
            .insert(id, AnalysisSlot::Pending(cancel.clone()))
        {
            previous.store(true, Ordering::Relaxed);
        }
        self.pitch_comparison = None;
        cancel
    }

    /// Stores the outcome of an analysis job; `false` when the job is stale.
    pub fn finish_analysis(
        &mut self,
        id: Uuid,
        job: &Arc<AtomicBool>,
        outcome: Result<Arc<Analysis>, UserError>,
        isolated_vocals: bool,
    ) -> bool {
        let is_current = matches!(
            self.analyses.get(&id),
            Some(AnalysisSlot::Pending(current)) if Arc::ptr_eq(current, job)
        );
        if !is_current {
            return false;
        }
        let slot = match outcome {
            Ok(analysis) => AnalysisSlot::Ready {
                analysis,
                isolated_vocals,
            },
            Err(error) => AnalysisSlot::Failed(error),
        };
        self.analyses.insert(id, slot);
        true
    }

    pub fn analysis(&self, id: Uuid) -> Option<Arc<Analysis>> {
        match self.analyses.get(&id) {
            Some(AnalysisSlot::Ready { analysis, .. }) => Some(analysis.clone()),
            _ => None,
        }
    }

    /// Whether the finished analysis of `id` was made from isolated vocals.
    pub fn analysis_used_vocals(&self, id: Uuid) -> bool {
        matches!(
            self.analyses.get(&id),
            Some(AnalysisSlot::Ready {
                isolated_vocals: true,
                ..
            })
        )
    }

    // ── Comparison ─────────────────────────────────────────────────────

    pub fn alignment(&self) -> Option<Alignment> {
        self.alignment
    }

    pub fn set_alignment(&mut self, alignment: Option<Alignment>) {
        self.alignment = alignment;
        self.pitch_comparison = None;
    }

    pub fn set_pitch_comparison(&mut self, comparison: Option<PitchComparison>) {
        self.pitch_comparison = comparison;
    }

    pub fn has_pitch_comparison(&self) -> bool {
        self.pitch_comparison.is_some()
    }

    /// Converts a time on one recording's timeline to the other's, through
    /// the alignment. `None` unless both are in the compared pair and the
    /// alignment is known.
    pub fn map_time(&self, from: Uuid, to: Uuid, seconds: f64) -> Option<f64> {
        let (reference, other) = self.compared_pair()?;
        let alignment = self.alignment?;
        if from == reference && to == other {
            Some(alignment.to_other(seconds))
        } else if from == other && to == reference {
            Some(alignment.to_reference(seconds))
        } else {
            None
        }
    }

    pub fn view(&self) -> SessionView {
        let recordings = self
            .project
            .iter()
            .flat_map(|project| &project.recordings)
            .map(|recording| {
                let (waveform_status, waveform_error) = match self.waveforms.get(&recording.id) {
                    None => (WaveformStatus::Unavailable, None),
                    Some(WaveformSlot::Pending(_)) => (WaveformStatus::Pending, None),
                    Some(WaveformSlot::Ready(_)) => (WaveformStatus::Ready, None),
                    Some(WaveformSlot::Failed(error)) => {
                        (WaveformStatus::Failed, Some(error.clone()))
                    }
                };
                let (analysis_status, analysis_error) = match self.analyses.get(&recording.id) {
                    None => (AnalysisStatus::Unavailable, None),
                    Some(AnalysisSlot::Pending(_)) => (AnalysisStatus::Pending, None),
                    Some(AnalysisSlot::Ready { .. }) => (AnalysisStatus::Ready, None),
                    Some(AnalysisSlot::Failed(error)) => {
                        (AnalysisStatus::Failed, Some(error.clone()))
                    }
                };
                RecordingRuntime {
                    recording_id: recording.id,
                    display_title: recording.display_title(),
                    display_detail: recording.display_detail(),
                    source_exists: recording.source.path.exists(),
                    waveform_status,
                    waveform_error,
                    analysis_status,
                    analysis_error,
                    analysed_isolated_vocals: self.analysis_used_vocals(recording.id),
                    vocal_stem: self.stems.get(&recording.id).cloned(),
                }
            })
            .collect();
        SessionView {
            project: self.project.clone(),
            project_file_name: self
                .project_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned()),
            project_path: self.project_path.clone(),
            dirty: self.dirty,
            active_recording_id: self.active_recording_id,
            recordings,
            listening_to_vocals: self.listening_to_vocals,
            comparison: self
                .compared_pair()
                .map(|(reference, other)| ComparisonView {
                    reference_recording_id: reference,
                    other_recording_id: other,
                    alignment: self.alignment,
                    pitch: self.pitch_comparison.clone(),
                }),
        }
    }
}

/// Folds measured facts into the recording. This refines metadata that was
/// read from the file; it is not a user edit and does not mark the project
/// as changed.
fn apply_summary(recording: &mut Recording, summary: &WaveformSummary) {
    recording.audio.frame_count = Some(summary.frame_count);
    recording.audio.duration_seconds = Some(summary.duration_seconds);
    if summary.duration_seconds > 0.0 {
        recording.audio.average_bitrate_kbps = Some(
            ((recording.audio.file_size_bytes as f64 * 8.0) / summary.duration_seconds / 1000.0)
                .round() as u32,
        );
    }
    recording.waveform = Some(summary.clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::peaks::PeakBuilder;
    use crate::project::test_support::audio_info;

    fn session_with_recording() -> (Session, Uuid) {
        let mut project = Project::new("Test");
        let recording = Recording::new(Path::new("/nowhere/take.wav"), audio_info("take.wav"));
        let id = recording.id;
        project.recordings.push(recording);
        let mut session = Session::default();
        session.open(project, None, false);
        (session, id)
    }

    fn peaks(frames: usize) -> Arc<Peaks> {
        let mut builder = PeakBuilder::new(44_100, 1, None);
        builder.push(&vec![0.25f32; frames]);
        Arc::new(builder.finish())
    }

    fn user_error() -> UserError {
        AppError::Internal("boom".into()).to_user_error()
    }

    #[test]
    fn an_empty_session_has_an_empty_view() {
        let view = Session::default().view();
        assert_eq!(view.project, None);
        assert!(view.recordings.is_empty());
        assert!(!view.dirty);
    }

    #[test]
    fn opening_activates_the_first_recording_and_flags_missing_sources() {
        let (session, id) = session_with_recording();
        let view = session.view();
        assert_eq!(view.active_recording_id, Some(id));
        assert_eq!(view.recordings[0].display_title, "take");
        assert!(!view.recordings[0].source_exists);
        assert_eq!(
            view.recordings[0].waveform_status,
            WaveformStatus::Unavailable
        );
        assert_eq!(session.active_recording().unwrap().id, id);
    }

    #[test]
    fn label_edits_mark_the_project_dirty_only_when_something_changes() {
        let (mut session, id) = session_with_recording();
        session
            .set_label(id, RecordingLabel::default(), SourceKind::Unspecified)
            .unwrap();
        assert!(!session.is_dirty());

        let label = RecordingLabel {
            version: Some(" 2011 remaster ".into()),
            ..Default::default()
        };
        session.set_label(id, label, SourceKind::VocalStem).unwrap();
        assert!(session.is_dirty());
        let recording = session.active_recording().unwrap();
        assert_eq!(recording.label.version.as_deref(), Some("2011 remaster"));
        assert_eq!(recording.source.kind, SourceKind::VocalStem);
        assert_eq!(
            session.view().recordings[0].display_detail.as_deref(),
            Some("2011 remaster · Vocal stem")
        );

        let unknown = session.set_label(
            Uuid::new_v4(),
            RecordingLabel::default(),
            SourceKind::FullMix,
        );
        assert!(matches!(
            unknown.unwrap_err(),
            AppError::RecordingNotFound(_)
        ));
    }

    #[test]
    fn waveform_results_refine_metadata_without_dirtying() {
        let (mut session, id) = session_with_recording();
        let job = session.begin_waveform(id);
        assert_eq!(
            session.view().recordings[0].waveform_status,
            WaveformStatus::Pending
        );
        assert!(session.peaks(id).is_none());

        assert!(session.finish_waveform(id, &job, Ok(peaks(88_200))));
        let view = session.view();
        assert_eq!(view.recordings[0].waveform_status, WaveformStatus::Ready);
        assert!(!view.dirty);
        let recording = session.active_recording().unwrap();
        assert_eq!(recording.audio.frame_count, Some(88_200));
        assert_eq!(recording.duration_seconds(), Some(2.0));
        assert_eq!(session.peaks(id).unwrap().total_frames(), 88_200);
    }

    #[test]
    fn waveform_failures_are_kept_for_display() {
        let (mut session, id) = session_with_recording();
        let job = session.begin_waveform(id);
        assert!(session.finish_waveform(id, &job, Err(user_error())));
        let view = session.view();
        assert_eq!(view.recordings[0].waveform_status, WaveformStatus::Failed);
        assert_eq!(
            view.recordings[0].waveform_error.as_ref().unwrap().code,
            "internal_error"
        );
    }

    #[test]
    fn stale_waveform_jobs_are_cancelled_and_ignored() {
        let (mut session, id) = session_with_recording();
        let first = session.begin_waveform(id);
        let second = session.begin_waveform(id);
        assert!(
            first.load(Ordering::Relaxed),
            "the superseded job should be cancelled"
        );
        assert!(!second.load(Ordering::Relaxed));
        assert!(!session.finish_waveform(id, &first, Ok(peaks(100))));
        assert_eq!(
            session.view().recordings[0].waveform_status,
            WaveformStatus::Pending
        );

        session.close();
        assert!(
            second.load(Ordering::Relaxed),
            "closing cancels running jobs"
        );
        assert!(!session.finish_waveform(id, &second, Ok(peaks(100))));
        assert!(!session.has_project());
    }

    fn analysis() -> Arc<Analysis> {
        use crate::analysis::pitch::test_support::{synth, track};
        let samples = synth(44_100, 0.6, &[1.0, 0.5], |_| 60.0, |_| 0.4);
        Arc::new(Analysis::from_track(track(44_100, &samples)))
    }

    #[test]
    fn analysis_jobs_follow_the_same_rules_as_waveform_jobs() {
        let (mut session, id) = session_with_recording();
        assert_eq!(
            session.view().recordings[0].analysis_status,
            AnalysisStatus::Unavailable
        );
        let stale = session.begin_analysis(id);
        let job = session.begin_analysis(id);
        assert!(stale.load(Ordering::Relaxed));
        assert_eq!(
            session.view().recordings[0].analysis_status,
            AnalysisStatus::Pending
        );
        assert!(!session.finish_analysis(id, &stale, Ok(analysis()), false));
        assert!(session.analysis(id).is_none());

        assert!(session.finish_analysis(id, &job, Ok(analysis()), true));
        let view = session.view();
        assert_eq!(view.recordings[0].analysis_status, AnalysisStatus::Ready);
        assert!(view.recordings[0].analysed_isolated_vocals);
        assert!(!view.dirty, "an analysis is derived, not an edit");
        assert_eq!(session.analysis(id).unwrap().notes.len(), 1);

        let failing = session.begin_analysis(id);
        assert!(session.finish_analysis(id, &failing, Err(user_error()), false));
        let view = session.view();
        assert_eq!(view.recordings[0].analysis_status, AnalysisStatus::Failed);
        assert!(view.recordings[0].analysis_error.is_some());
        assert!(!view.recordings[0].analysed_isolated_vocals);

        let running = session.begin_analysis(id);
        session.close();
        assert!(running.load(Ordering::Relaxed));
    }

    fn stem(name: &str) -> VocalStem {
        VocalStem {
            model_id: "m".into(),
            model_name: "Model".into(),
            path: PathBuf::from(format!("/stems/{name}.wav")),
        }
    }

    #[test]
    fn isolated_vocals_redirect_analysis_and_listening() {
        let (mut session, id) = session_with_recording();
        let original = PathBuf::from("/nowhere/take.wav");
        assert_eq!(session.analysis_input(id), Some((original.clone(), false)));
        assert_eq!(session.playback_path(id), Some(original.clone()));
        // Nothing to listen to yet.
        session.set_listening_to_vocals(true);
        assert!(!session.listening_to_vocals());

        session.set_stem(id, Some(stem("take")));
        assert_eq!(
            session.analysis_input(id),
            Some((PathBuf::from("/stems/take.wav"), true))
        );
        assert_eq!(session.playback_path(id), Some(original.clone()));
        session.set_listening_to_vocals(true);
        assert_eq!(
            session.playback_path(id),
            Some(PathBuf::from("/stems/take.wav"))
        );
        let view = session.view();
        assert!(view.listening_to_vocals);
        assert_eq!(view.recordings[0].vocal_stem, Some(stem("take")));
        assert!(!view.dirty, "finding a stem is not an edit");

        // The user can insist on the original; that is a saved choice.
        assert!(session
            .set_analysis_source(id, AnalysisSource::Original)
            .unwrap());
        assert!(!session
            .set_analysis_source(id, AnalysisSource::Original)
            .unwrap());
        assert!(session.is_dirty());
        assert_eq!(session.analysis_input(id), Some((original.clone(), false)));

        session.set_stem(id, None);
        assert!(!session.listening_to_vocals());
        assert_eq!(session.playback_path(id), Some(original));
        assert_eq!(session.analysis_input(Uuid::new_v4()), None);
    }

    #[test]
    fn a_second_recording_can_be_added_compared_and_removed() {
        use crate::analysis::compare::AlignmentQuality;

        let (mut session, first) = session_with_recording();
        assert!(session.view().comparison.is_none());
        assert_eq!(session.compared_pair(), None);

        let other = Recording::new(
            Path::new("/nowhere/remaster.wav"),
            audio_info("remaster.wav"),
        );
        let second = other.id;
        session.add_recording(other).unwrap();
        assert!(session.is_dirty());
        assert_eq!(session.compared_pair(), Some((first, second)));
        assert_eq!(session.other_recording(first), Some(second));
        assert_eq!(session.other_recording(second), Some(first));
        assert_eq!(session.other_recording(Uuid::new_v4()), None);
        let third = Recording::new(Path::new("/nowhere/c.wav"), audio_info("c.wav"));
        assert!(matches!(
            session.add_recording(third).unwrap_err(),
            AppError::InvalidInput(_)
        ));

        let comparison = session.view().comparison.unwrap();
        assert_eq!(comparison.reference_recording_id, first);
        assert_eq!(comparison.other_recording_id, second);
        assert!(comparison.alignment.is_none() && comparison.pitch.is_none());
        assert_eq!(session.map_time(first, second, 1.0), None);

        session.set_alignment(Some(Alignment {
            offset_seconds: 2.0,
            speed_ratio: 1.0,
            confidence: 0.9,
            quality: AlignmentQuality::Good,
        }));
        assert_eq!(session.map_time(first, second, 1.0), Some(3.0));
        assert_eq!(session.map_time(second, first, 3.0), Some(1.0));
        assert_eq!(session.map_time(first, first, 1.0), None);

        session.set_active(second).unwrap();
        assert_eq!(session.active_recording().unwrap().id, second);
        assert!(session.set_active(Uuid::new_v4()).is_err());

        // Removing the active recording falls back to the one that is left
        // and takes the comparison with it.
        session.remove_recording(second).unwrap();
        assert_eq!(session.active_recording().unwrap().id, first);
        assert!(session.view().comparison.is_none());
        assert_eq!(session.alignment(), None);
        assert!(matches!(
            session.remove_recording(first).unwrap_err(),
            AppError::InvalidInput(_)
        ));
        assert!(matches!(
            session.remove_recording(second).unwrap_err(),
            AppError::RecordingNotFound(_)
        ));
    }

    #[test]
    fn relocating_keeps_identity_and_labels() {
        let (mut session, id) = session_with_recording();
        let label = RecordingLabel {
            recording_name: Some("Lead".into()),
            ..Default::default()
        };
        session.set_label(id, label, SourceKind::VocalStem).unwrap();
        let job = session.begin_waveform(id);

        let mut info = audio_info("found.flac");
        info.sample_rate_hz = 48_000;
        let replacement = Recording::new(Path::new("/elsewhere/found.flac"), info);
        session.relocate(id, replacement).unwrap();

        assert!(job.load(Ordering::Relaxed));
        let recording = session.active_recording().unwrap();
        assert_eq!(recording.id, id);
        assert_eq!(recording.source.path, Path::new("/elsewhere/found.flac"));
        assert_eq!(recording.source.kind, SourceKind::VocalStem);
        assert_eq!(recording.audio.sample_rate_hz, 48_000);
        assert_eq!(recording.label.recording_name.as_deref(), Some("Lead"));
        assert_eq!(
            session.view().recordings[0].waveform_status,
            WaveformStatus::Unavailable
        );
    }
}
