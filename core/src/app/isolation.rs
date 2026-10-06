//! Vocal isolation as the UI sees it: choosing a model, the one background
//! job that downloads it if need be and runs it, and what becomes of the
//! result.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use uuid::Uuid;

use super::{analysis, AppCore, Inner, PROGRESS_INTERVAL};
use crate::error::{AppError, AppResult, CoreError, UserError};
use crate::hardware;
use crate::paths::AppPaths;
use crate::separation::isolate_vocals_with;
use crate::separation::mdx::{OnnxModel, SpectrogramModel};
use crate::separation::models::{
    find_model, inference_threads, ModelSpec, SeparationModel, MODELS,
};
use crate::session::{SessionView, VocalStem};

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum IsolationStage {
    /// Nothing is running.
    Idle,
    /// The model is being downloaded.
    Downloading,
    /// The model is being loaded.
    Preparing,
    /// The model is running over the recording.
    Isolating,
    /// The last job failed; `error` says why. Cleared by the next job.
    Failed,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct IsolationStatus {
    pub stage: IsolationStage,
    pub recording_id: Option<Uuid>,
    pub model_id: Option<String>,
    /// Progress of the current stage, 0–1, when it can be known.
    pub fraction: Option<f32>,
    pub error: Option<UserError>,
}

impl Default for IsolationStatus {
    fn default() -> Self {
        Self {
            stage: IsolationStage::Idle,
            recording_id: None,
            model_id: None,
            fraction: None,
            error: None,
        }
    }
}

#[derive(Default)]
pub(super) struct IsolationJob {
    status: IsolationStatus,
    cancel: Option<Arc<AtomicBool>>,
}

impl IsolationJob {
    fn is_running(&self) -> bool {
        self.cancel.is_some()
    }
}

/// The newest vocals already isolated from `source`, if any were made by a
/// model this version knows.
pub(super) fn find_stem(paths: &AppPaths, source: &Path) -> Option<VocalStem> {
    paths
        .stems_for(source)
        .into_iter()
        .find_map(|(model_id, path)| {
            let model = find_model(&model_id).ok()?;
            Some(VocalStem {
                model_id,
                model_name: model.name.to_string(),
                path,
            })
        })
}

impl Inner {
    fn isolation_job(&self) -> std::sync::MutexGuard<'_, IsolationJob> {
        self.isolation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records a new status and tells the UI. No lock is held during the
    /// callback.
    fn set_isolation_status(&self, change: impl FnOnce(&mut IsolationStatus)) {
        let status = {
            let mut job = self.isolation_job();
            change(&mut job.status);
            job.status.clone()
        };
        self.observer.isolation_changed(status);
    }
}

/// How the model is brought into memory; replaced in tests.
type ModelLoader = dyn Fn(&Path, usize) -> AppResult<Box<dyn SpectrogramModel>> + Send;

fn load_onnx(path: &Path, threads: usize) -> AppResult<Box<dyn SpectrogramModel>> {
    Ok(Box::new(OnnxModel::load(path, threads)?))
}

/// The body of an isolation job. Returns the stem it made.
fn run_job(
    inner: &Arc<Inner>,
    source: &Path,
    model: &'static ModelSpec,
    cancel: &Arc<AtomicBool>,
    load: &ModelLoader,
) -> AppResult<VocalStem> {
    let mut last_report: Option<Instant> = None;
    let mut throttled = |inner: &Inner, fraction: Option<f32>| {
        if last_report.is_none_or(|at| at.elapsed() >= PROGRESS_INTERVAL) {
            last_report = Some(Instant::now());
            inner.set_isolation_status(|status| status.fraction = fraction);
        }
    };

    if !inner.models.is_installed(model) {
        log::info!("downloading separation model {}", model.id);
        inner.models.download(model, cancel, |fraction| {
            throttled(inner, Some(fraction));
        })?;
    }

    inner.set_isolation_status(|status| {
        status.stage = IsolationStage::Preparing;
        status.fraction = None;
    });
    let threads = inference_threads(&hardware::detect(false));
    let loaded = load(&inner.models.path(model), threads)?;
    let output = inner
        .paths
        .stem_file(source, model.id)
        .ok_or_else(|| AppError::FileNotFound(source.to_path_buf()))?;

    inner.set_isolation_status(|status| {
        status.stage = IsolationStage::Isolating;
        status.fraction = Some(0.0);
    });
    let started = Instant::now();
    isolate_vocals_with(
        source,
        loaded,
        model.parameters,
        &output,
        cancel,
        |fraction| {
            throttled(inner, fraction);
        },
    )?;
    log::info!(
        "isolated vocals with {} on {threads} thread(s) in {:.1} s",
        model.id,
        started.elapsed().as_secs_f64()
    );
    Ok(VocalStem {
        model_id: model.id.to_string(),
        model_name: model.name.to_string(),
        path: output,
    })
}

/// Starts the one isolation job, or explains why it cannot start.
fn start_job(
    inner: &Arc<Inner>,
    recording_id: Uuid,
    model_id: &str,
    load: Box<ModelLoader>,
) -> AppResult<IsolationStatus> {
    let model = find_model(model_id)?;
    let source = inner
        .session()
        .project()
        .ok_or(AppError::NoProject)?
        .recording(recording_id)
        .map(|recording| recording.source.path.clone())
        .ok_or_else(|| AppError::RecordingNotFound(recording_id.to_string()))?;
    if !source.exists() {
        return Err(AppError::FileNotFound(source));
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let status = {
        let mut job = inner.isolation_job();
        if job.is_running() {
            return Err(AppError::Busy("isolating vocals".into()));
        }
        job.cancel = Some(cancel.clone());
        job.status = IsolationStatus {
            stage: if inner.models.is_installed(model) {
                IsolationStage::Preparing
            } else {
                IsolationStage::Downloading
            },
            recording_id: Some(recording_id),
            model_id: Some(model.id.to_string()),
            fraction: None,
            error: None,
        };
        job.status.clone()
    };
    inner.observer.isolation_changed(status.clone());

    let thread_inner = inner.clone();
    let spawned = std::thread::Builder::new()
        .name("isolation".into())
        .spawn(move || {
            let inner = thread_inner;
            let result = run_job(&inner, &source, model, &cancel, &load);
            let failure = match result {
                Ok(stem) => {
                    // The recording may have been closed, removed or pointed
                    // at another file while the model ran; the stem is then
                    // simply kept for when that audio is next opened.
                    let still_there = {
                        let mut session = inner.session();
                        let matches = session
                            .project()
                            .and_then(|project| project.recording(recording_id))
                            .is_some_and(|recording| recording.source.path == source);
                        if matches {
                            session.set_stem(recording_id, Some(stem));
                        }
                        matches
                    };
                    if still_there {
                        analysis::start_analysis_job(&inner, recording_id, false);
                        inner.publish_session();
                    }
                    None
                }
                Err(AppError::Cancelled) => None,
                Err(err) => {
                    log::error!("vocal isolation failed: {err}");
                    Some(err.to_user_error())
                }
            };
            let status = {
                let mut job = inner.isolation_job();
                job.cancel = None;
                job.status = match failure {
                    Some(error) => IsolationStatus {
                        stage: IsolationStage::Failed,
                        fraction: None,
                        error: Some(error),
                        ..job.status.clone()
                    },
                    None => IsolationStatus::default(),
                };
                job.status.clone()
            };
            inner.observer.isolation_changed(status);
        });
    if let Err(err) = spawned {
        inner.isolation_job().cancel = None;
        inner.set_isolation_status(|status| *status = IsolationStatus::default());
        return Err(AppError::Internal(format!(
            "could not start the isolation thread: {err}"
        )));
    }
    Ok(status)
}

#[uniffi::export]
impl AppCore {
    /// The vocal-isolation models on offer, with the one suggested for this
    /// computer marked.
    pub fn separation_models(&self) -> Vec<SeparationModel> {
        self.inner.models.list(&hardware::detect(false))
    }

    /// Isolates the vocals of a recording in the background, downloading
    /// the model first if it has not been already. Progress and the outcome
    /// arrive through `AppObserver::isolation_changed`; the stem itself
    /// appears in the session. One job runs at a time.
    ///
    /// A model that is not installed is downloaded by this call, so the UI
    /// must have shown its size and licence and been told to go ahead.
    pub fn isolate_vocals(
        &self,
        recording_id: Uuid,
        model_id: String,
    ) -> Result<IsolationStatus, CoreError> {
        Ok(start_job(
            &self.inner,
            recording_id,
            &model_id,
            Box::new(load_onnx),
        )?)
    }

    /// Stops the running isolation job, if there is one. It ends shortly
    /// afterwards and leaves nothing behind.
    pub fn cancel_isolation(&self) {
        if let Some(cancel) = &self.inner.isolation_job().cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn isolation_status(&self) -> IsolationStatus {
        self.inner.isolation_job().status.clone()
    }

    /// Deletes a downloaded model to free its disk space.
    pub fn remove_separation_model(&self, model_id: String) -> Result<(), CoreError> {
        let model = find_model(&model_id).map_err(CoreError::from)?;
        if self.inner.isolation_job().is_running() {
            return Err(AppError::Busy("isolating vocals".into()).into());
        }
        self.inner.models.remove(model).map_err(CoreError::from)?;
        Ok(())
    }

    /// Deletes the vocals isolated from a recording, made by any model.
    pub fn remove_vocal_stem(&self, recording_id: Uuid) -> Result<SessionView, CoreError> {
        let source = self
            .inner
            .session()
            .project()
            .ok_or(AppError::NoProject)?
            .recording(recording_id)
            .map(|recording| recording.source.path.clone())
            .ok_or_else(|| AppError::RecordingNotFound(recording_id.to_string()))?;
        let was_listening = self.inner.session().listening_to_vocals();
        let had_vocals_analysis = self.inner.session().analysis_used_vocals(recording_id);

        for (_, file) in self.inner.paths.stems_for(&source) {
            std::fs::remove_file(file).map_err(|err| CoreError::from(AppError::Io(err)))?;
        }
        self.inner.session().set_stem(recording_id, None);
        if was_listening {
            self.reload_playback_in_place()?;
        }
        if had_vocals_analysis {
            analysis::start_analysis_job(&self.inner, recording_id, false);
        }
        Ok(self.inner.publish_session())
    }

    /// Saves a copy of a recording's isolated vocals as a WAV file.
    pub fn export_vocal_stem(
        &self,
        recording_id: Uuid,
        path: PathBuf,
    ) -> Result<PathBuf, CoreError> {
        let stem = self
            .inner
            .session()
            .stem(recording_id)
            .cloned()
            .ok_or_else(|| {
                AppError::InvalidInput(
                    "The vocals of this recording have not been isolated.".into(),
                )
            })?;
        let path = if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
        {
            path
        } else {
            let mut name = path.into_os_string();
            name.push(".wav");
            PathBuf::from(name)
        };
        std::fs::copy(&stem.path, &path).map_err(|err| CoreError::from(AppError::Io(err)))?;
        Ok(path)
    }

    /// Switches playback between the recordings themselves and their
    /// isolated vocals, staying at the same place.
    pub fn set_listening_to_vocals(&self, listening: bool) -> Result<SessionView, CoreError> {
        let changed = {
            let mut session = self.inner.session();
            let before = session.listening_to_vocals();
            session.set_listening_to_vocals(listening);
            session.listening_to_vocals() != before
        };
        if changed {
            self.reload_playback_in_place()?;
        }
        Ok(self.inner.publish_session())
    }
}

impl AppCore {
    /// Points the player at the file the active recording should now be
    /// heard from, keeping the position and carrying on if it was playing.
    pub(super) fn reload_playback_in_place(&self) -> Result<(), CoreError> {
        let target = {
            let session = self.inner.session();
            session.active_recording().and_then(|recording| {
                let path = session.playback_path(recording.id)?;
                Some((path, recording.duration_seconds()))
            })
        };
        if let Some((path, duration)) = target.filter(|(path, _)| path.exists()) {
            let position = self.inner.playback.status().position_seconds;
            self.inner
                .playback
                .switch_track(&path, duration, position)
                .map_err(CoreError::from)?;
        }
        Ok(())
    }

    /// [`AppCore::isolate_vocals`] with a stand-in for the model, so the job
    /// can be exercised without a 60 MB download.
    #[cfg(test)]
    pub(super) fn isolate_vocals_with_loader(
        &self,
        recording_id: Uuid,
        model_id: &str,
        load: Box<ModelLoader>,
    ) -> AppResult<IsolationStatus> {
        start_job(&self.inner, recording_id, model_id, load)
    }

    /// Where a model would be installed.
    #[cfg(test)]
    pub(super) fn model_path(&self, model_id: &str) -> PathBuf {
        self.inner.models.path(find_model(model_id).unwrap())
    }
}

/// Number of models on offer; lets a UI lay out its list before asking.
#[uniffi::export]
pub fn separation_model_count() -> u32 {
    MODELS.len() as u32
}
