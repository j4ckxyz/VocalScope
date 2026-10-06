//! A/B comparison as the UI sees it: a second recording in the project,
//! lined up with the first, switched between while playing, and overlaid on
//! the timeline.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use uuid::Uuid;

use super::{load_playback, start_recording_jobs, AppCore, Inner};
use crate::analysis::compare::{align, compare_pitch, ENVELOPE_STEP_SECONDS};
use crate::analysis::PitchCurve;
use crate::audio::decode;
use crate::error::{AppError, CoreError};
use crate::project::Recording;
use crate::session::SessionView;

/// Lines the two compared recordings up, once both waveforms are ready and
/// if that has not been done. Runs on its own thread; the result arrives in
/// the session.
pub(super) fn start_alignment(inner: &Arc<Inner>) {
    let inputs = {
        let session = inner.session();
        session
            .compared_pair()
            .filter(|_| session.alignment().is_none())
            .and_then(|(reference, other)| {
                Some((
                    reference,
                    other,
                    session.peaks(reference)?,
                    session.peaks(other)?,
                ))
            })
    };
    let Some((reference, other, reference_peaks, other_peaks)) = inputs else {
        return;
    };
    let inner = inner.clone();
    let spawned = std::thread::Builder::new()
        .name("alignment".into())
        .spawn(move || {
            let started = Instant::now();
            let alignment = align(
                &reference_peaks.envelope(ENVELOPE_STEP_SECONDS),
                &other_peaks.envelope(ENVELOPE_STEP_SECONDS),
            );
            match &alignment {
                Some(found) => log::info!(
                    "aligned in {} ms: offset {:.3} s, speed {:.5}, confidence {:.2}",
                    started.elapsed().as_millis(),
                    found.offset_seconds,
                    found.speed_ratio,
                    found.confidence
                ),
                None => log::info!("the recordings are too short to align"),
            }
            {
                let mut session = inner.session();
                // The pair may have changed while this ran.
                let still_current = session.compared_pair() == Some((reference, other))
                    && session
                        .peaks(reference)
                        .is_some_and(|p| Arc::ptr_eq(&p, &reference_peaks))
                    && session
                        .peaks(other)
                        .is_some_and(|p| Arc::ptr_eq(&p, &other_peaks));
                if !still_current {
                    return;
                }
                session.set_alignment(alignment);
            }
            refresh_pitch_comparison(&inner);
            inner.publish_session();
        });
    if let Err(err) = spawned {
        log::error!("could not start the alignment thread: {err}");
    }
}

/// Compares the two recordings' pitch when everything it needs is ready and
/// it has not been done. Quick enough to run wherever it is called.
pub(super) fn refresh_pitch_comparison(inner: &Arc<Inner>) {
    let inputs = {
        let session = inner.session();
        session
            .compared_pair()
            .filter(|_| !session.has_pitch_comparison())
            .and_then(|(reference, other)| {
                Some((
                    session.alignment()?,
                    session.analysis(reference)?,
                    session.analysis(other)?,
                ))
            })
    };
    let Some((alignment, reference, other)) = inputs else {
        return;
    };
    let comparison = compare_pitch(&reference, &other, &alignment);
    let mut session = inner.session();
    // Only if nothing it was computed from has been replaced meanwhile.
    let unchanged = session.alignment() == Some(alignment)
        && session.compared_pair().is_some_and(|(a, b)| {
            session
                .analysis(a)
                .is_some_and(|x| Arc::ptr_eq(&x, &reference))
                && session.analysis(b).is_some_and(|x| Arc::ptr_eq(&x, &other))
        });
    if unchanged {
        session.set_pitch_comparison(Some(comparison));
    }
}

#[uniffi::export]
impl AppCore {
    /// Adds a second recording to the project, to compare with the first.
    /// It is lined up with the first automatically.
    pub fn add_recording(&self, path: PathBuf) -> Result<SessionView, CoreError> {
        if super::is_project_file(&path) {
            return Err(AppError::InvalidInput(
                "Choose an audio file to compare with, not a project.".into(),
            )
            .into());
        }
        let info = decode::probe(&path).map_err(CoreError::from)?;
        let recording = Recording::new(&path, info);
        let id = recording.id;
        self.inner
            .session()
            .add_recording(recording)
            .map_err(CoreError::from)?;
        start_recording_jobs(&self.inner, id);
        Ok(self.inner.publish_session())
    }

    /// Removes a recording from the project (the audio file is untouched).
    pub fn remove_recording(&self, recording_id: Uuid) -> Result<SessionView, CoreError> {
        let was_active = self
            .inner
            .session()
            .active_recording()
            .is_some_and(|recording| recording.id == recording_id);
        self.inner
            .session()
            .remove_recording(recording_id)
            .map_err(CoreError::from)?;
        if was_active {
            load_playback(&self.inner).map_err(CoreError::from)?;
        }
        Ok(self.inner.publish_session())
    }

    /// Makes another recording the one that is shown and heard. Playback
    /// moves to the matching moment in it and carries on if it was playing,
    /// which is what makes switching back and forth a comparison.
    pub fn set_active_recording(&self, recording_id: Uuid) -> Result<SessionView, CoreError> {
        let position = self.inner.playback.status().position_seconds;
        let target = {
            let mut session = self.inner.session();
            let previous = session.active_recording().map(|recording| recording.id);
            if previous == Some(recording_id) {
                None
            } else {
                session.set_active(recording_id).map_err(CoreError::from)?;
                let mapped = previous
                    .and_then(|from| session.map_time(from, recording_id, position))
                    .unwrap_or(position);
                let duration = session
                    .active_recording()
                    .and_then(|recording| recording.duration_seconds());
                session
                    .playback_path(recording_id)
                    .map(|path| (path, duration, mapped))
            }
        };
        if let Some((path, duration, position)) = target {
            if path.exists() {
                self.inner
                    .playback
                    .switch_track(&path, duration, position)
                    .map_err(CoreError::from)?;
            } else {
                self.inner.playback.unload().map_err(CoreError::from)?;
            }
        }
        Ok(self.inner.publish_session())
    }

    /// Converts a time in one compared recording to the matching time in
    /// the other. `None` until they have been lined up.
    pub fn map_time(
        &self,
        from_recording_id: Uuid,
        to_recording_id: Uuid,
        seconds: f64,
    ) -> Option<f64> {
        self.inner
            .session()
            .map_time(from_recording_id, to_recording_id, seconds)
    }

    /// The pitch curve of the recording being compared with `recording_id`,
    /// placed on `recording_id`'s timeline so the two can be drawn together.
    /// Empty when there is nothing to compare or it is not ready.
    pub fn comparison_curve(
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
        let found = {
            let session = self.inner.session();
            session.other_recording(recording_id).and_then(|other| {
                // Probing the mapping also confirms the alignment exists.
                session.map_time(recording_id, other, 0.0)?;
                let (reference, _) = session.compared_pair()?;
                Some((
                    session.analysis(other)?,
                    session.alignment()?,
                    recording_id == reference,
                ))
            })
        };
        let Some((analysis, alignment, from_reference)) = found else {
            return empty;
        };
        analysis.curve(
            start_seconds.max(0.0),
            end_seconds,
            columns.min(16_384) as usize,
            |t| {
                if from_reference {
                    alignment.to_other(t)
                } else {
                    alignment.to_reference(t)
                }
            },
        )
    }
}
