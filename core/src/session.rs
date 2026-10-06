//! The open project and its runtime state.
//!
//! This is plain data and logic with no dependency on Tauri, so it can be
//! unit-tested. `commands/` wraps it with events, threads and the UI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Serialize;
use uuid::Uuid;

use crate::audio::peaks::{Peaks, WaveformSummary};
use crate::error::{AppError, AppResult, UserError};
use crate::project::{Project, Recording, RecordingLabel, SourceKind};

/// Where a recording's waveform summary stands.
enum WaveformSlot {
    /// A background decode is running; setting the flag cancels it.
    Pending(Arc<AtomicBool>),
    Ready(Arc<Peaks>),
    Failed(UserError),
}

#[derive(Default)]
pub struct Session {
    project: Option<Project>,
    project_path: Option<PathBuf>,
    dirty: bool,
    active_recording_id: Option<Uuid>,
    waveforms: HashMap<Uuid, WaveformSlot>,
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
        self.cancel_all_waveforms();
        self.project = None;
        self.project_path = None;
        self.dirty = false;
        self.active_recording_id = None;
    }

    fn cancel_all_waveforms(&mut self) {
        for (_, slot) in self.waveforms.drain() {
            if let WaveformSlot::Pending(cancel) = slot {
                cancel.store(true, Ordering::Relaxed);
            }
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
        if let Some(WaveformSlot::Pending(cancel)) = self.waveforms.remove(&id) {
            cancel.store(true, Ordering::Relaxed);
        }
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
                RecordingRuntime {
                    recording_id: recording.id,
                    display_title: recording.display_title(),
                    display_detail: recording.display_detail(),
                    source_exists: recording.source.path.exists(),
                    waveform_status,
                    waveform_error,
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
