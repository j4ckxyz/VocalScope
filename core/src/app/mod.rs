//! The application service: the one object a native UI talks to.
//!
//! A UI creates an [`AppCore`], hands it an [`AppObserver`], and from then on
//! calls methods in response to user actions and redraws when the observer
//! fires. Everything a UI could get wrong on its own — when to cache, how to
//! name things, what counts as unsaved — is decided here, once.
//!
//! Threading: every method may be called from any thread and returns
//! promptly. Long work (decoding a file for its waveform) runs on background
//! threads. Observer callbacks arrive on whichever thread caused them, often
//! a background one, so UIs must hop to their main thread before touching
//! views. No lock is held while an observer is being called.

mod analysis;
mod comparison;
mod isolation;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use uuid::Uuid;

pub use analysis::{AnalysisView, PitchReading};
pub use isolation::{IsolationStage, IsolationStatus};

use crate::audio::decode;
use crate::audio::peaks::{compute_peaks, Peaks};
use crate::audio::playback::{self, OutputDevice, PlaybackHandle, PlaybackStatus, TransportState};
use crate::db::recents::{RecentEntry, RecentItem, RecentKind};
use crate::db::settings::Settings;
use crate::db::Database;
use crate::error::{AppError, AppResult, CoreError};
use crate::hardware::{self, HardwareProfile};
use crate::paths::{self, AppPaths};
use crate::project::{io, Project, Recording, RecordingLabel, SourceKind, PROJECT_FILE_EXTENSION};
use crate::separation::models::ModelStore;
use crate::session::{Session, SessionView};

/// Minimum spacing between progress callbacks of any background job.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(80);
/// Upper bound on columns per waveform request (an 8K display is 7 680).
const MAX_WAVEFORM_COLUMNS: u32 = 16_384;

/// Where the application keeps its files. Each platform supplies its own
/// conventional locations.
#[derive(Debug, Clone, uniffi::Record)]
pub struct AppConfig {
    /// Durable data: the settings and recent-files database.
    pub data_directory: PathBuf,
    /// Disposable data: waveform summaries and pitch tracks, which can
    /// always be rebuilt.
    pub cache_directory: PathBuf,
    pub log_directory: PathBuf,
}

/// Receives state changes. Implemented by each platform's UI.
#[uniffi::export(with_foreign)]
pub trait AppObserver: Send + Sync {
    /// The open project or its runtime state (waveform readiness, unsaved
    /// changes) changed.
    fn session_changed(&self, session: SessionView);
    /// The transport changed: started, paused, stopped, sought, ended, or a
    /// volume change. Not called for the steady advance of the position
    /// during playback; poll [`AppCore::playback_status`] for that.
    fn playback_changed(&self, status: PlaybackStatus);
    /// Decode progress for a recording's waveform, 0–1, or `None` when the
    /// file's length is not known until it has been fully read.
    fn waveform_progress(&self, recording_id: Uuid, fraction: Option<f32>);
    /// Progress of a recording's pitch analysis, in the same form. The
    /// result arrives through `session_changed`.
    fn analysis_progress(&self, recording_id: Uuid, fraction: Option<f32>);
    /// A vocal-isolation job started, moved on, finished or failed.
    fn isolation_changed(&self, status: IsolationStatus);
    fn recents_changed(&self, recents: Vec<RecentItem>);
    fn settings_changed(&self, settings: Settings);
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct Diagnostics {
    pub application_version: String,
    pub hardware: HardwareProfile,
    pub database_schema_version: Option<u32>,
    pub data_directory: PathBuf,
    pub cache_directory: PathBuf,
    pub log_directory: PathBuf,
}

struct Inner {
    db: Mutex<Database>,
    session: Mutex<Session>,
    playback: PlaybackHandle,
    paths: AppPaths,
    models: ModelStore,
    isolation: Mutex<isolation::IsolationJob>,
    observer: Arc<dyn AppObserver>,
}

impl Inner {
    /// A poisoned lock (a panic while it was held) is recovered rather than
    /// propagated: the data is plain state and the app should keep running.
    fn session(&self) -> MutexGuard<'_, Session> {
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn db(&self) -> MutexGuard<'_, Database> {
        self.db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn recents(&self) -> AppResult<Vec<RecentItem>> {
        let db = self.db();
        let limit = db.load_settings()?.general.recent_file_count;
        db.list_recents(limit)
    }

    fn publish_session(&self) -> SessionView {
        let view = self.session().view();
        self.observer.session_changed(view.clone());
        view
    }

    fn publish_recents(&self) {
        match self.recents() {
            Ok(recents) => self.observer.recents_changed(recents),
            Err(err) => log::warn!("could not read the recent list: {err}"),
        }
    }

    fn remember(&self, entry: RecentEntry) {
        // Failing to update the recent list must never fail the open itself.
        if let Err(err) = self.db().touch_recent(&entry) {
            log::warn!("could not update the recent list: {err}");
        }
        self.publish_recents();
    }
}

/// The application. Create one per process.
#[derive(uniffi::Object)]
pub struct AppCore {
    inner: Arc<Inner>,
}

fn is_project_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(PROJECT_FILE_EXTENSION))
}

fn audio_recent_entry(recording: &Recording) -> RecentEntry {
    RecentEntry {
        kind: RecentKind::Audio,
        path: recording.source.path.clone(),
        title: recording.display_title(),
        artist: recording.audio.tags.artist.clone(),
        detail: recording.display_detail(),
        duration_seconds: recording.duration_seconds(),
    }
}

fn project_recent_entry(project: &Project, path: &Path) -> RecentEntry {
    let first = project.recordings.first();
    RecentEntry {
        kind: RecentKind::Project,
        path: path.to_path_buf(),
        title: path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| project.name.clone()),
        artist: first.and_then(|r| r.audio.tags.artist.clone()),
        detail: Some(match project.recordings.len() {
            1 => first.map(Recording::display_title).unwrap_or_default(),
            n => format!("{n} recordings"),
        }),
        duration_seconds: first.and_then(Recording::duration_seconds),
    }
}

/// Stores a finished waveform (or its failure), starts what depends on it
/// and tells the UI.
fn finish_waveform_job(
    inner: &Arc<Inner>,
    recording_id: Uuid,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
    outcome: Result<Arc<Peaks>, crate::error::UserError>,
    notify: bool,
) {
    let succeeded = outcome.is_ok();
    let duration = outcome.as_ref().ok().map(|peaks| peaks.duration_seconds());
    let (stored, is_active) = {
        let mut session = inner.session();
        let stored = session.finish_waveform(recording_id, cancel, outcome);
        (
            stored,
            session
                .active_recording()
                .is_some_and(|r| r.id == recording_id),
        )
    };
    if !stored {
        return;
    }
    if let (true, Some(duration)) = (is_active, duration) {
        // The container's duration can be an estimate (MP3 without a length
        // header); the full decode is exact.
        let _ = inner.playback.set_duration(duration);
    }
    if succeeded {
        // A readable file is worth analysing, and two of them worth aligning.
        // Stored quietly when cached: the publish below (or the caller's)
        // covers it.
        analysis::start_analysis_job(inner, recording_id, false);
        comparison::start_alignment(inner);
    }
    if notify {
        inner.publish_session();
    }
}

/// Gets a recording's waveform summary.
///
/// A summary cached from an earlier session is loaded right here, before
/// returning, so reopening a file shows its waveform in the very first frame
/// (loading takes about a millisecond per ten minutes of audio). Otherwise
/// the file is decoded on a background thread and the UI is told when it is
/// ready.
fn start_waveform_job(inner: &Arc<Inner>, recording_id: Uuid, source: PathBuf) {
    let cancel = inner.session().begin_waveform(recording_id);
    let cache_file = inner.paths.waveform_cache_file(&source);

    let started = Instant::now();
    if let Some(peaks) = cache_file.as_deref().and_then(Peaks::load) {
        log::info!(
            "waveform ready: {:.1} s of audio in {:.1} ms (cached)",
            peaks.duration_seconds(),
            started.elapsed().as_secs_f64() * 1000.0
        );
        // The caller publishes the session itself once the open completes.
        finish_waveform_job(inner, recording_id, &cancel, Ok(Arc::new(peaks)), false);
        return;
    }

    let inner = inner.clone();
    let spawned = std::thread::Builder::new()
        .name("waveform".into())
        .spawn(move || {
            let mut last_report: Option<Instant> = None;
            let result = compute_peaks(&source, &cancel, |fraction| {
                if last_report.is_none_or(|at| at.elapsed() >= PROGRESS_INTERVAL) {
                    last_report = Some(Instant::now());
                    inner.observer.waveform_progress(recording_id, fraction);
                }
            });
            let outcome = match result {
                Err(AppError::Cancelled) => return,
                Ok(peaks) => {
                    if let Some(cache_file) = &cache_file {
                        if let Err(err) = peaks.save(cache_file) {
                            log::warn!("could not cache the waveform summary: {err}");
                        }
                    }
                    log::info!(
                        "waveform ready: {:.1} s of audio in {} ms",
                        peaks.duration_seconds(),
                        started.elapsed().as_millis()
                    );
                    Ok(Arc::new(peaks))
                }
                Err(err) => {
                    log::error!("waveform analysis failed: {err}");
                    Err(err.to_user_error())
                }
            };
            finish_waveform_job(&inner, recording_id, &cancel, outcome, true);
        });
    if let Err(err) = spawned {
        log::error!("could not start the waveform thread: {err}");
    }
}

/// Points the player at what should be heard for the active recording: the
/// recording itself, or its isolated vocals when those are being listened to.
fn load_playback(inner: &Arc<Inner>) -> AppResult<()> {
    let active = {
        let session = inner.session();
        session.active_recording().and_then(|recording| {
            let path = session.playback_path(recording.id)?;
            Some((path, recording.duration_seconds()))
        })
    };
    match active {
        Some((path, duration)) if path.exists() => {
            inner.playback.load(&path, duration)?;
        }
        _ => {
            inner.playback.unload()?;
        }
    }
    Ok(())
}

/// Starts everything that is derived from one recording's audio: finds any
/// vocals already isolated from it, then its waveform, which in turn starts
/// its pitch analysis. A missing source is not an error: the project still
/// opens and the UI offers to locate the file.
fn start_recording_jobs(inner: &Arc<Inner>, recording_id: Uuid) {
    let source = inner
        .session()
        .project()
        .and_then(|project| project.recording(recording_id))
        .map(|recording| recording.source.path.clone());
    let Some(source) = source.filter(|source| source.exists()) else {
        return;
    };
    let stem = isolation::find_stem(&inner.paths, &source);
    inner.session().set_stem(recording_id, stem);
    start_waveform_job(inner, recording_id, source);
}

/// Loads the active recording for playback and starts the background work
/// for every recording in the project.
fn activate(inner: &Arc<Inner>) -> AppResult<()> {
    let ids: Vec<Uuid> = inner
        .session()
        .project()
        .map(|project| project.recordings.iter().map(|r| r.id).collect())
        .unwrap_or_default();
    load_playback(inner)?;
    for id in ids {
        start_recording_jobs(inner, id);
    }
    Ok(())
}

fn open_audio_file(inner: &Arc<Inner>, path: &Path) -> AppResult<SessionView> {
    let info = decode::probe(path)?;
    log::info!(
        "opened audio: {} ({}, {} Hz, {} ch)",
        info.file_name,
        info.codec,
        info.sample_rate_hz,
        info.channel_count
    );
    let recording = Recording::new(path, info);
    let entry = audio_recent_entry(&recording);
    let mut project = Project::new(recording.display_title());
    project.recordings.push(recording);

    // A project made by opening a file has nothing unsaved in it yet.
    inner.session().open(project, None, false);
    if let Err(err) = activate(inner) {
        inner.session().close();
        inner.publish_session();
        return Err(err);
    }
    inner.remember(entry);
    Ok(inner.publish_session())
}

fn open_project_file(inner: &Arc<Inner>, path: &Path) -> AppResult<SessionView> {
    let project = io::load_project(path)?;
    log::info!(
        "opened project with {} recording(s), format version {}",
        project.recordings.len(),
        project.project_format_version
    );
    let entry = project_recent_entry(&project, path);
    inner
        .session()
        .open(project, Some(path.to_path_buf()), false);
    if let Err(err) = activate(inner) {
        // The project itself is fine; only its audio could not be loaded.
        log::warn!("the project's audio could not be loaded: {err}");
        let _ = inner.playback.unload();
    }
    inner.remember(entry);
    Ok(inner.publish_session())
}

#[uniffi::export]
impl AppCore {
    /// Starts the core. This is on the application's launch path, so it does
    /// only cheap work: open the database, start the (idle) playback thread
    /// and trim the cache. No audio device is opened until playback starts.
    #[uniffi::constructor]
    pub fn new(config: AppConfig, observer: Arc<dyn AppObserver>) -> Arc<Self> {
        crate::logging::init(&config.log_directory);
        log::info!(
            "VocalScope {} starting on {} ({})",
            application_version(),
            std::env::consts::OS,
            std::env::consts::ARCH
        );

        let paths = AppPaths {
            data_dir: config.data_directory,
            cache_dir: config.cache_directory,
            log_dir: config.log_directory,
        };
        // Without its database the app still works for the session; it just
        // cannot remember recent files or settings.
        let db = Database::open(&paths.database_file()).unwrap_or_else(|err| {
            log::error!("could not open the database, continuing without persistence: {err}");
            Database::open_in_memory().expect("an in-memory database can always be created")
        });
        let settings = db.load_settings().unwrap_or_default();

        let status_observer = observer.clone();
        let playback = PlaybackHandle::spawn(
            settings.playback.default_volume,
            settings.playback.output_device.clone(),
            move |status| status_observer.playback_changed(status.clone()),
        );

        let stores = [
            (
                paths.waveform_cache_dir(),
                paths::WAVEFORM_CACHE_LIMIT_BYTES,
            ),
            (
                paths.analysis_cache_dir(),
                paths::ANALYSIS_CACHE_LIMIT_BYTES,
            ),
            (paths.stems_dir(), paths::STEM_STORE_LIMIT_BYTES),
        ];
        // Housekeeping and the hardware profile are not needed to show the
        // window; keep them off the launch path.
        std::thread::spawn(move || {
            for (directory, limit) in stores {
                let removed = paths::prune_cache_dir(&directory, limit);
                if removed > 0 {
                    log::info!("removed {removed} old file(s) from {}", directory.display());
                }
            }
            log::info!("hardware: {}", hardware::detect(true).log_line());
        });

        Arc::new(Self {
            inner: Arc::new(Inner {
                db: Mutex::new(db),
                session: Mutex::new(Session::default()),
                playback,
                models: ModelStore::new(paths.models_dir()),
                isolation: Mutex::new(isolation::IsolationJob::default()),
                paths,
                observer,
            }),
        })
    }

    // ── Project ────────────────────────────────────────────────────────

    pub fn session(&self) -> SessionView {
        self.inner.session().view()
    }

    /// Opens an audio file or a project file, whichever `path` is.
    pub fn open_path(&self, path: PathBuf) -> Result<SessionView, CoreError> {
        let result = if is_project_file(&path) {
            open_project_file(&self.inner, &path)
        } else {
            open_audio_file(&self.inner, &path)
        };
        Ok(result?)
    }

    /// Saves the project. With `path`, saves to that location (Save As);
    /// without, saves over the file it was opened from.
    pub fn save_project(&self, path: Option<PathBuf>) -> Result<SessionView, CoreError> {
        let (mut project, target) = {
            let session = self.inner.session();
            let project = session.project().cloned().ok_or(AppError::NoProject)?;
            let target = path
                .or_else(|| session.project_path().map(Path::to_path_buf))
                .ok_or_else(|| {
                    AppError::InvalidInput("Choose where to save the project.".into())
                })?;
            (project, target)
        };
        let target = if is_project_file(&target) {
            target
        } else {
            target.with_extension(PROJECT_FILE_EXTENSION)
        };
        if let Some(stem) = target.file_stem() {
            project.name = stem.to_string_lossy().into_owned();
        }
        io::save_project(&mut project, &target).map_err(CoreError::from)?;
        log::info!(
            "saved project with {} recording(s)",
            project.recordings.len()
        );

        let entry = project_recent_entry(&project, &target);
        self.inner.session().mark_saved(project, target);
        self.inner.remember(entry);
        Ok(self.inner.publish_session())
    }

    pub fn close_project(&self) -> Result<SessionView, CoreError> {
        self.inner.session().close();
        self.inner.playback.unload().map_err(CoreError::from)?;
        Ok(self.inner.publish_session())
    }

    pub fn update_recording_label(
        &self,
        recording_id: Uuid,
        label: RecordingLabel,
        source_kind: SourceKind,
    ) -> Result<SessionView, CoreError> {
        self.inner
            .session()
            .set_label(recording_id, label, source_kind)
            .map_err(CoreError::from)?;
        Ok(self.inner.publish_session())
    }

    /// Points a recording whose file went missing at a newly chosen file.
    pub fn relocate_recording(
        &self,
        recording_id: Uuid,
        path: PathBuf,
    ) -> Result<SessionView, CoreError> {
        let info = decode::probe(&path).map_err(CoreError::from)?;
        let replacement = Recording::new(&path, info);
        self.inner
            .session()
            .relocate(recording_id, replacement)
            .map_err(CoreError::from)?;
        load_playback(&self.inner).map_err(CoreError::from)?;
        start_recording_jobs(&self.inner, recording_id);
        Ok(self.inner.publish_session())
    }

    /// Min/max pairs (interleaved, 16-bit full scale) for drawing `columns`
    /// columns of waveform between two times. Empty until the recording's
    /// waveform is ready.
    pub fn waveform_peaks(
        &self,
        recording_id: Uuid,
        start_seconds: f64,
        end_seconds: f64,
        columns: u32,
    ) -> Vec<i16> {
        let Some(peaks) = self.inner.session().peaks(recording_id) else {
            return Vec::new();
        };
        if !start_seconds.is_finite() || !end_seconds.is_finite() {
            return Vec::new();
        }
        let rate = peaks.sample_rate_hz() as f64;
        let start = (start_seconds.max(0.0) * rate).round() as u64;
        let end = (end_seconds.max(0.0) * rate).round() as u64;
        peaks.query(start, end, columns.min(MAX_WAVEFORM_COLUMNS) as usize)
    }

    // ── Recent files ───────────────────────────────────────────────────

    pub fn recents(&self) -> Vec<RecentItem> {
        self.inner.recents().unwrap_or_else(|err| {
            log::warn!("could not read the recent list: {err}");
            Vec::new()
        })
    }

    pub fn remove_recent(&self, id: i64) -> Result<(), CoreError> {
        self.inner.db().remove_recent(id).map_err(CoreError::from)?;
        self.inner.publish_recents();
        Ok(())
    }

    pub fn clear_recents(&self) -> Result<(), CoreError> {
        self.inner.db().clear_recents().map_err(CoreError::from)?;
        self.inner.publish_recents();
        Ok(())
    }

    // ── Playback ───────────────────────────────────────────────────────

    /// Latest transport status. Cheap enough to call every display frame.
    pub fn playback_status(&self) -> PlaybackStatus {
        self.inner.playback.status()
    }

    pub fn play(&self) -> Result<PlaybackStatus, CoreError> {
        Ok(self.inner.playback.play()?)
    }

    pub fn pause(&self) -> Result<PlaybackStatus, CoreError> {
        Ok(self.inner.playback.pause()?)
    }

    pub fn toggle_playback(&self) -> Result<PlaybackStatus, CoreError> {
        let playback = &self.inner.playback;
        Ok(if playback.status().state == TransportState::Playing {
            playback.pause()?
        } else {
            playback.play()?
        })
    }

    pub fn stop(&self) -> Result<PlaybackStatus, CoreError> {
        Ok(self.inner.playback.stop()?)
    }

    pub fn seek(&self, seconds: f64) -> Result<PlaybackStatus, CoreError> {
        Ok(self.inner.playback.seek(seconds)?)
    }

    /// `volume` is the slider position, 0–1.
    pub fn set_volume(&self, volume: f32) -> Result<PlaybackStatus, CoreError> {
        Ok(self.inner.playback.set_volume(volume)?)
    }

    pub fn set_muted(&self, muted: bool) -> Result<PlaybackStatus, CoreError> {
        Ok(self.inner.playback.set_muted(muted)?)
    }

    pub fn output_devices(&self) -> Vec<OutputDevice> {
        playback::list_output_devices().unwrap_or_else(|err| {
            log::warn!("could not list output devices: {err}");
            Vec::new()
        })
    }

    // ── Settings ───────────────────────────────────────────────────────

    pub fn settings(&self) -> Settings {
        self.inner.db().load_settings().unwrap_or_default()
    }

    pub fn update_settings(&self, settings: Settings) -> Result<Settings, CoreError> {
        let (previous, current) = {
            let db = self.inner.db();
            let previous = db.load_settings().map_err(CoreError::from)?;
            (
                previous,
                db.save_settings(&settings).map_err(CoreError::from)?,
            )
        };
        self.apply_settings(&previous, &current);
        Ok(current)
    }

    pub fn reset_settings(&self) -> Result<Settings, CoreError> {
        let (previous, current) = {
            let db = self.inner.db();
            let previous = db.load_settings().map_err(CoreError::from)?;
            (previous, db.reset_settings().map_err(CoreError::from)?)
        };
        log::info!("settings were reset to defaults");
        self.apply_settings(&previous, &current);
        Ok(current)
    }

    // ── Diagnostics ────────────────────────────────────────────────────

    /// Collects the hardware profile. Blocks for about 200 ms while CPU load
    /// is sampled, so call it off the main thread.
    pub fn diagnostics(&self) -> Diagnostics {
        Diagnostics {
            application_version: application_version(),
            hardware: hardware::detect(true),
            database_schema_version: self.inner.db().schema_version().ok(),
            data_directory: self.inner.paths.data_dir.clone(),
            cache_directory: self.inner.paths.cache_dir.clone(),
            log_directory: self.inner.paths.log_dir.clone(),
        }
    }
}

impl AppCore {
    /// Puts changed settings into effect and tells the UI.
    fn apply_settings(&self, previous: &Settings, current: &Settings) {
        if previous.playback.output_device != current.playback.output_device {
            let device = current.playback.output_device.clone();
            if let Err(err) = self.inner.playback.set_output_device(device) {
                log::warn!("could not switch output device: {err}");
            }
        }
        self.inner.observer.settings_changed(current.clone());
        if previous.general.recent_file_count != current.general.recent_file_count {
            self.inner.publish_recents();
        }
    }
}

/// The application version. Platform apps display this rather than keeping
/// their own copy.
#[uniffi::export]
pub fn application_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// File extensions the Open Audio dialog offers. Other files can still be
/// opened; the decoder decides what it can read.
#[uniffi::export]
pub fn supported_audio_extensions() -> Vec<String> {
    [
        "wav", "wave", "aif", "aiff", "flac", "mp3", "m4a", "aac", "mp4", "ogg", "oga",
    ]
    .map(String::from)
    .to_vec()
}

/// Extension of VocalScope project files, without the dot.
#[uniffi::export]
pub fn project_file_extension() -> String {
    PROJECT_FILE_EXTENSION.to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::audio::decode::test_support::write_sine_wav;
    use crate::session::WaveformStatus;

    /// Records what the core tells its UI.
    #[derive(Default)]
    struct Recorder {
        sessions: Mutex<Vec<SessionView>>,
        playback: AtomicUsize,
        progress: AtomicUsize,
        analysis_progress: AtomicUsize,
        isolation: Mutex<Vec<IsolationStatus>>,
        recents: Mutex<Vec<Vec<RecentItem>>>,
        settings: Mutex<Vec<Settings>>,
    }

    impl AppObserver for Recorder {
        fn session_changed(&self, session: SessionView) {
            self.sessions.lock().unwrap().push(session);
        }
        fn playback_changed(&self, _status: PlaybackStatus) {
            self.playback.fetch_add(1, Ordering::SeqCst);
        }
        fn waveform_progress(&self, _recording_id: Uuid, _fraction: Option<f32>) {
            self.progress.fetch_add(1, Ordering::SeqCst);
        }
        fn analysis_progress(&self, _recording_id: Uuid, _fraction: Option<f32>) {
            self.analysis_progress.fetch_add(1, Ordering::SeqCst);
        }
        fn isolation_changed(&self, status: IsolationStatus) {
            self.isolation.lock().unwrap().push(status);
        }
        fn recents_changed(&self, recents: Vec<RecentItem>) {
            self.recents.lock().unwrap().push(recents);
        }
        fn settings_changed(&self, settings: Settings) {
            self.settings.lock().unwrap().push(settings);
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        app: Arc<AppCore>,
        recorder: Arc<Recorder>,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Arc::new(Recorder::default());
        let app = AppCore::new(
            AppConfig {
                data_directory: dir.path().join("data"),
                cache_directory: dir.path().join("cache"),
                log_directory: dir.path().join("logs"),
            },
            recorder.clone(),
        );
        Fixture { dir, app, recorder }
    }

    impl Fixture {
        fn wav(&self, name: &str, seconds: f32) -> PathBuf {
            let path = self.dir.path().join(name);
            write_sine_wav(&path, 44_100, seconds, 0.5, &[440.0, 660.0]);
            path
        }

        /// Waits for the background waveform job of the active recording.
        fn wait_for_waveform(&self) -> SessionView {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let view = self.app.session();
                let status = view.recordings.first().map(|r| r.waveform_status);
                if matches!(status, Some(WaveformStatus::Ready | WaveformStatus::Failed)) {
                    return view;
                }
                assert!(Instant::now() < deadline, "waveform never finished");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn user_error(result: Result<SessionView, CoreError>) -> crate::error::UserError {
        match result {
            Err(CoreError::Failure { error }) => error,
            Ok(_) => panic!("expected an error"),
        }
    }

    #[test]
    fn opening_audio_builds_a_session_and_a_waveform() {
        let f = fixture();
        let path = f.wav("take.wav", 2.0);

        let view = f.app.open_path(path.clone()).unwrap();
        let project = view.project.as_ref().unwrap();
        assert_eq!(project.name, "take");
        assert_eq!(project.recordings.len(), 1);
        assert!(!view.dirty);
        assert!(view.recordings[0].source_exists);

        let status = f.app.playback_status();
        assert!(status.has_track);
        assert!((status.duration_seconds.unwrap() - 2.0).abs() < 0.01);

        let ready = f.wait_for_waveform();
        assert_eq!(ready.recordings[0].waveform_status, WaveformStatus::Ready);
        let recording = &ready.project.unwrap().recordings[0];
        assert_eq!(recording.waveform.as_ref().unwrap().frame_count, 88_200);

        let peaks = f.app.waveform_peaks(recording.id, 0.0, 2.0, 400);
        assert_eq!(peaks.len(), 800);
        assert!(peaks.chunks(2).all(|c| c[1] > 10_000 && c[0] < -10_000));

        // The UI was told about the open and about the waveform becoming
        // ready. The second notification is sent by the waveform thread just
        // after the state changes, so give it a moment to arrive.
        let deadline = Instant::now() + Duration::from_secs(5);
        while f.recorder.sessions.lock().unwrap().len() < 2 {
            assert!(
                Instant::now() < deadline,
                "the UI was never told the waveform was ready"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let last = f.recorder.sessions.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last.recordings[0].waveform_status, WaveformStatus::Ready);
        let recents = f.app.recents();
        assert_eq!(recents.len(), 1);
        assert_eq!(recents[0].title, "take");
        assert_eq!(f.recorder.recents.lock().unwrap().last().unwrap().len(), 1);
    }

    #[test]
    fn waveform_requests_before_ready_or_with_bad_input_are_empty() {
        let f = fixture();
        assert!(f
            .app
            .waveform_peaks(Uuid::new_v4(), 0.0, 1.0, 100)
            .is_empty());
        let view = f.app.open_path(f.wav("a.wav", 1.0)).unwrap();
        f.wait_for_waveform();
        let id = view.project.unwrap().recordings[0].id;
        assert!(f.app.waveform_peaks(id, f64::NAN, 1.0, 100).is_empty());
        // Column counts are capped rather than trusted.
        assert_eq!(
            f.app.waveform_peaks(id, 0.0, 1.0, u32::MAX).len(),
            2 * 16_384
        );
    }

    #[test]
    fn the_second_open_uses_the_waveform_cache() {
        let f = fixture();
        let path = f.wav("take.wav", 1.0);
        f.app.open_path(path.clone()).unwrap();
        f.wait_for_waveform();
        let cache_dir = f.dir.path().join("cache").join("waveforms");
        assert_eq!(std::fs::read_dir(&cache_dir).unwrap().count(), 1);

        // Reopening returns with the waveform already in place: no waiting,
        // and nothing for the UI to draw twice.
        f.app.close_project().unwrap();
        let view = f.app.open_path(path).unwrap();
        assert_eq!(view.recordings[0].waveform_status, WaveformStatus::Ready);
        let recording = &view.project.unwrap().recordings[0];
        assert_eq!(recording.waveform.as_ref().unwrap().frame_count, 44_100);
        assert_eq!(
            f.app
                .waveform_peaks(recording.id.clone(), 0.0, 1.0, 50)
                .len(),
            100
        );
        assert_eq!(std::fs::read_dir(&cache_dir).unwrap().count(), 1);
    }

    #[test]
    fn labels_save_and_reload_through_a_project_file() {
        let f = fixture();
        let view = f.app.open_path(f.wav("take.wav", 1.0)).unwrap();
        let id = view.project.unwrap().recordings[0].id;

        // Nothing to save to yet.
        assert_eq!(user_error(f.app.save_project(None)).code, "invalid_input");

        let label = RecordingLabel {
            recording_name: Some("Lead vocal".into()),
            version: Some("Original release".into()),
            release_year: Some(1976),
            notes: None,
        };
        let edited = f
            .app
            .update_recording_label(id, label, SourceKind::VocalStem)
            .unwrap();
        assert!(edited.dirty);
        assert_eq!(edited.recordings[0].display_title, "Lead vocal");

        // The extension is added when the UI's save panel leaves it off.
        let saved = f
            .app
            .save_project(Some(f.dir.path().join("Study")))
            .unwrap();
        assert!(!saved.dirty);
        assert_eq!(saved.project_file_name.as_deref(), Some("Study.vocalscope"));
        let file = f.dir.path().join("Study.vocalscope");
        assert!(file.exists());

        f.app.close_project().unwrap();
        assert!(f.app.session().project.is_none());
        assert!(!f.app.playback_status().has_track);

        let reopened = f.app.open_path(file).unwrap();
        let recording = &reopened.project.as_ref().unwrap().recordings[0];
        assert_eq!(recording.id, id);
        assert_eq!(recording.label.version.as_deref(), Some("Original release"));
        assert_eq!(recording.source.kind, SourceKind::VocalStem);
        assert!(f.app.playback_status().has_track);

        let recents = f.app.recents();
        assert_eq!(recents[0].kind, RecentKind::Project);
        assert_eq!(recents[0].title, "Study");
        assert_eq!(recents[0].detail.as_deref(), Some("Lead vocal"));
    }

    #[test]
    fn a_project_with_missing_audio_still_opens_and_can_be_relocated() {
        let f = fixture();
        let original = f.wav("take.wav", 1.0);
        let view = f.app.open_path(original.clone()).unwrap();
        let id = view.project.unwrap().recordings[0].id;
        let file = f.dir.path().join("p.vocalscope");
        f.app.save_project(Some(file.clone())).unwrap();
        f.app.close_project().unwrap();
        std::fs::remove_file(&original).unwrap();

        let opened = f.app.open_path(file).unwrap();
        assert!(!opened.recordings[0].source_exists);
        assert_eq!(
            opened.recordings[0].waveform_status,
            WaveformStatus::Unavailable
        );
        assert!(!f.app.playback_status().has_track);

        let replacement = f.wav("found.wav", 1.5);
        let relocated = f.app.relocate_recording(id, replacement).unwrap();
        assert!(relocated.recordings[0].source_exists);
        assert!(relocated.dirty);
        assert!(f.app.playback_status().has_track);
        let ready = f.wait_for_waveform();
        let duration = ready.project.unwrap().recordings[0]
            .duration_seconds()
            .unwrap();
        assert!((duration - 1.5).abs() < 0.01);
    }

    #[test]
    fn failed_opens_leave_nothing_half_open_and_explain_themselves() {
        let f = fixture();
        let missing = user_error(f.app.open_path(f.dir.path().join("nope.wav")));
        assert_eq!(missing.code, "file_not_found");
        assert!(missing.message.contains("nope.wav"));

        let bogus = f.dir.path().join("bogus.mp3");
        std::fs::write(&bogus, b"not audio ".repeat(500)).unwrap();
        assert_eq!(
            user_error(f.app.open_path(bogus)).code,
            "unsupported_format"
        );

        let broken = f.dir.path().join("broken.vocalscope");
        std::fs::write(&broken, "{").unwrap();
        assert_eq!(
            user_error(f.app.open_path(broken)).code,
            "project_format_invalid"
        );

        assert!(f.app.session().project.is_none());
        assert!(f.app.recents().is_empty());
        assert_eq!(user_error(f.app.save_project(None)).code, "no_project");
    }

    #[test]
    fn transport_commands_work_without_an_audio_device() {
        let f = fixture();
        f.app.open_path(f.wav("take.wav", 2.0)).unwrap();
        assert_eq!(f.app.seek(1.0).unwrap().position_seconds, 1.0);
        assert_eq!(f.app.set_volume(0.25).unwrap().volume, 0.25);
        assert!(f.app.set_muted(true).unwrap().muted);
        assert_eq!(f.app.stop().unwrap().position_seconds, 0.0);
        assert!(f.recorder.playback.load(Ordering::SeqCst) >= 4);
    }

    #[test]
    fn settings_persist_and_notify() {
        let f = fixture();
        let mut settings = f.app.settings();
        assert_eq!(settings, Settings::default());
        settings.general.recent_file_count = 3;
        settings.playback.default_volume = 0.4;
        let saved = f.app.update_settings(settings.clone()).unwrap();
        assert_eq!(saved, settings);
        assert_eq!(f.app.settings(), settings);
        assert_eq!(f.recorder.settings.lock().unwrap().last(), Some(&settings));

        // The recent list honours the new limit.
        for i in 0..5 {
            f.app.open_path(f.wav(&format!("{i}.wav"), 0.2)).unwrap();
        }
        assert_eq!(f.app.recents().len(), 3);
        f.app.clear_recents().unwrap();
        assert!(f.app.recents().is_empty());

        assert_eq!(f.app.reset_settings().unwrap(), Settings::default());
    }

    // ── Pitch analysis, export, comparison and isolation ───────────────

    use crate::analysis::compare::AlignmentQuality;
    use crate::analysis::pitch::test_support::synth;
    use crate::analysis::test_support::{phrase, write_wav, VOICE};
    use crate::export::ExportFormat;
    use crate::project::AnalysisSource;
    use crate::separation::mdx::test_support::ScalingModel;
    use crate::session::AnalysisStatus;

    const RATE: u32 = 44_100;

    impl Fixture {
        fn audio(&self, name: &str, samples: &[f32]) -> PathBuf {
            let path = self.dir.path().join(name);
            write_wav(&path, RATE, &[samples, samples]);
            path
        }

        /// Waits for background work to bring the session to some state.
        fn wait_until(&self, what: &str, done: impl Fn(&SessionView) -> bool) -> SessionView {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let view = self.app.session();
                if done(&view) {
                    return view;
                }
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn wait_for_analysis(&self) -> SessionView {
            self.wait_until("the pitch analysis", |view| {
                view.recordings
                    .iter()
                    .all(|r| r.analysis_status == AnalysisStatus::Ready)
            })
        }
    }

    /// Fourteen seconds of melody with irregular notes and rests. `sag`
    /// lowers the note sung between 5.0 and 5.6 s by that many semitones.
    fn melody(sag: f64) -> Vec<f32> {
        // (seconds, MIDI note or 0 for a rest)
        const NOTES: [(f64, f64); 24] = [
            (0.45, 60.0),
            (0.30, 62.0),
            (0.25, 0.0),
            (0.70, 64.0),
            (0.35, 65.0),
            (0.40, 0.0),
            (0.55, 67.0),
            (0.30, 65.0),
            (0.90, 64.0),
            (0.80, 0.0),
            (0.60, 62.0),
            (0.45, 60.0),
            (0.35, 0.0),
            (0.75, 67.0),
            (0.40, 69.0),
            (0.50, 0.0),
            (0.85, 65.0),
            (0.30, 64.0),
            (0.65, 0.0),
            (0.55, 62.0),
            (0.95, 60.0),
            (0.45, 0.0),
            (0.70, 64.0),
            (1.45, 60.0),
        ];
        let at = |t: f64| {
            let mut start = 0.0;
            for (seconds, note) in NOTES {
                if t < start + seconds {
                    return (start, seconds, note);
                }
                start += seconds;
            }
            (start, 1.0, 0.0)
        };
        synth(
            RATE,
            14.0,
            &VOICE,
            |t| {
                let (_, _, note) = at(t);
                let note = if note == 0.0 { 60.0 } else { note };
                // The D that lasts from 5.0 to 5.6 s.
                if (5.0..5.6).contains(&t) {
                    note - sag
                } else {
                    note
                }
            },
            |t| {
                let (start, seconds, note) = at(t);
                if note == 0.0 {
                    0.0
                } else {
                    // A short fade in and out, so notes have natural edges.
                    let into = t - start;
                    0.4 * (into / 0.02).min(1.0) * ((seconds - into) / 0.03).min(1.0)
                }
            },
        )
    }

    #[test]
    fn a_recording_is_analysed_after_its_waveform() {
        let f = fixture();
        let path = f.audio("phrase.wav", &phrase(RATE));
        let opened = f.app.open_path(path.clone()).unwrap();
        let id = opened.project.unwrap().recordings[0].id;
        let ready = f.wait_for_analysis();
        assert!(!ready.dirty, "analysing is not an edit");
        assert!(!ready.recordings[0].analysed_isolated_vocals);
        let analysis = f.app.analysis(id).unwrap();
        let names: Vec<&str> = analysis.notes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["C4", "D4", "E4", "G4", "E4"]);
        assert_eq!(analysis.summary.note_count, 5);
        assert_eq!(analysis.indicators.indicators.len(), 4);
        // Nothing says this file is vocals only, so the results carry a caution.
        assert!(analysis
            .caution
            .as_deref()
            .unwrap()
            .contains("isolating the vocals"));
        // C4 to G4 with headroom, widened to at least an octave.
        assert!(analysis.display_low_midi <= 57.5 && analysis.display_high_midi >= 69.5);
        assert!(analysis.display_high_midi - analysis.display_low_midi >= 12.0);

        let curve = f.app.pitch_curve(id, 0.0, 3.0, 300);
        assert_eq!(curve.midi.len(), 300);
        assert!((curve.midi[20] - 60.0).abs() < 0.02);
        assert!(f.app.pitch_curve(id, f64::NAN, 1.0, 10).midi.is_empty());
        assert!(f
            .app
            .pitch_curve(Uuid::new_v4(), 0.0, 1.0, 10)
            .midi
            .is_empty());

        let reading = f.app.pitch_at(id, 0.2).unwrap();
        assert_eq!(reading.note_name, "C4");
        assert!(reading.deviation_cents.abs() < 1.0);
        assert!((reading.frequency_hz - 261.63).abs() < 0.2);
        assert!(reading.confidence > 0.9);
        assert!(f.app.pitch_at(id, 1.6).is_none(), "the breath has no pitch");
        assert!(f.app.pitch_at(id, -1.0).is_none());
        assert!(f.app.pitch_at(id, 99.0).is_none());

        // Told the user it is vocals only: no caution any more.
        f.app
            .update_recording_label(id, RecordingLabel::default(), SourceKind::VocalStem)
            .unwrap();
        assert!(f.app.analysis(id).unwrap().caution.is_none());

        // Reopening finds the analysis in the cache, ready in the first view.
        f.app.close_project().unwrap();
        let again = f.app.open_path(path).unwrap();
        assert_eq!(again.recordings[0].analysis_status, AnalysisStatus::Ready);
        let id = again.project.unwrap().recordings[0].id;
        assert_eq!(f.app.analysis(id).unwrap().notes.len(), 5);

        // Analysing again from scratch gives the same answer.
        let restarted = f.app.reanalyse(id).unwrap();
        assert_eq!(
            restarted.recordings[0].analysis_status,
            AnalysisStatus::Pending
        );
        f.wait_for_analysis();
        assert_eq!(f.app.analysis(id).unwrap().notes.len(), 5);
        assert!(f.recorder.analysis_progress.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn analyses_are_exported_in_every_format() {
        let f = fixture();
        let view = f.app.open_path(f.audio("take.wav", &phrase(RATE))).unwrap();
        let id = view.project.unwrap().recordings[0].id;
        // A name with a character no file system accepts.
        let label = RecordingLabel {
            recording_name: Some("lead: take 1".into()),
            ..Default::default()
        };
        f.app
            .update_recording_label(id, label, SourceKind::Unspecified)
            .unwrap();

        let too_early =
            f.app
                .export_analysis(Uuid::new_v4(), ExportFormat::Json, f.dir.path().join("x"));
        match too_early {
            Err(CoreError::Failure { error }) => assert_eq!(error.code, "recording_not_found"),
            Ok(_) => panic!("expected an error"),
        }
        f.wait_for_analysis();

        assert_eq!(
            f.app.suggested_export_name(id, ExportFormat::PitchCsv),
            "lead- take 1 pitch.csv"
        );
        assert_eq!(
            f.app.suggested_export_name(id, ExportFormat::Report),
            "lead- take 1 report.md"
        );
        assert_eq!(analysis::export_file_extension(ExportFormat::Midi), "mid");

        let out = f.dir.path().join("exports");
        for (format, name, starts_with) in [
            (ExportFormat::Json, "a.json", "{\n"),
            (ExportFormat::PitchCsv, "pitch", "time_seconds,"),
            (ExportFormat::NotesCsv, "notes.CSV", "start_seconds,"),
            (ExportFormat::Midi, "song.mid", "MThd"),
            (
                ExportFormat::Report,
                "report.txt",
                "# VocalScope report: lead: take 1",
            ),
        ] {
            let written = f.app.export_analysis(id, format, out.join(name)).unwrap();
            let bytes = std::fs::read(&written).unwrap();
            assert!(
                bytes.starts_with(starts_with.as_bytes()),
                "{format:?} began with {:?}",
                String::from_utf8_lossy(&bytes[..bytes.len().min(30)])
            );
        }
        // The extension is supplied when missing, and never doubled.
        assert!(out.join("pitch.csv").exists());
        assert!(out.join("notes.CSV").exists());
        assert!(out.join("report.txt.md").exists());
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 5);
    }

    #[test]
    fn two_versions_are_aligned_compared_and_switched_between() {
        let f = fixture();
        let original = f.audio("original.wav", &melody(0.4));
        // The other version starts half a second later and holds the note
        // the original lets sag.
        let mut later = vec![0f32; RATE as usize / 2];
        later.extend(melody(0.0));
        let remaster = f.audio("remaster.wav", &later);

        let opened = f.app.open_path(original).unwrap();
        let first = opened.project.unwrap().recordings[0].id;
        assert!(opened.comparison.is_none());
        let added = f.app.add_recording(remaster.clone()).unwrap();
        assert!(added.dirty);
        let second = added.project.as_ref().unwrap().recordings[1].id;
        assert_eq!(added.active_recording_id, Some(first));

        let ready = f.wait_until("the comparison", |view| {
            view.comparison.as_ref().is_some_and(|c| c.pitch.is_some())
        });
        let comparison = ready.comparison.unwrap();
        assert_eq!(comparison.reference_recording_id, first);
        assert_eq!(comparison.other_recording_id, second);
        let alignment = comparison.alignment.unwrap();
        assert_eq!(alignment.quality, AlignmentQuality::Good, "{alignment:?}");
        assert!(
            (alignment.offset_seconds - 0.5).abs() < 0.01,
            "{alignment:?}"
        );
        assert_eq!(alignment.speed_ratio, 1.0);
        let pitch = comparison.pitch.unwrap();
        assert!(pitch.median_difference_cents.unwrap().abs() < 1.0);
        assert_eq!(pitch.regions.len(), 1, "{:#?}", pitch.regions);
        let region = pitch.regions[0];
        assert!((region.start_seconds - 5.0).abs() < 0.06, "{region:?}");
        assert!((region.end_seconds - 5.6).abs() < 0.06, "{region:?}");
        assert!(
            (region.mean_difference_cents - 40.0).abs() < 4.0,
            "{region:?}"
        );

        // The other version's curve, drawn on the first one's timeline,
        // holds the D where the first one sags.
        let own = f.app.pitch_curve(first, 5.2, 5.4, 20);
        let other = f.app.comparison_curve(first, 5.2, 5.4, 20);
        assert_eq!(own.midi.len(), other.midi.len());
        assert!((own.midi[10] - 61.6).abs() < 0.03, "{}", own.midi[10]);
        assert!((other.midi[10] - 62.0).abs() < 0.03, "{}", other.midi[10]);
        assert!((f.app.map_time(first, second, 2.0).unwrap() - 2.5).abs() < 0.005);
        assert!((f.app.map_time(second, first, 2.5).unwrap() - 2.0).abs() < 0.005);

        // Switching keeps the musical position, not the clock position.
        f.app.seek(2.0).unwrap();
        let switched = f.app.set_active_recording(second).unwrap();
        assert_eq!(switched.active_recording_id, Some(second));
        let status = f.app.playback_status();
        assert!((status.position_seconds - 2.5).abs() < 0.01, "{status:?}");
        assert!((status.duration_seconds.unwrap() - 14.5).abs() < 0.01);
        // And from the second one's side the first is the overlay.
        let overlay = f.app.comparison_curve(second, 5.7, 5.9, 20);
        assert!(
            (overlay.midi[10] - 61.6).abs() < 0.03,
            "{}",
            overlay.midi[10]
        );
        f.app.set_active_recording(first).unwrap();
        assert!((f.app.playback_status().position_seconds - 2.0).abs() < 0.01);

        // A third recording, or a project as one, is refused.
        assert_eq!(
            user_error(f.app.add_recording(remaster)).code,
            "invalid_input"
        );
        assert_eq!(
            user_error(f.app.add_recording(f.dir.path().join("p.vocalscope"))).code,
            "invalid_input"
        );

        // Both recordings survive a save and reopen, and are compared again.
        let file = f.dir.path().join("pair.vocalscope");
        f.app.save_project(Some(file.clone())).unwrap();
        assert_eq!(f.app.recents()[0].detail.as_deref(), Some("2 recordings"));
        f.app.close_project().unwrap();
        let reopened = f.app.open_path(file).unwrap();
        assert_eq!(reopened.project.unwrap().recordings.len(), 2);
        let again = f.wait_until("the comparison after reopening", |view| {
            view.comparison.as_ref().is_some_and(|c| c.pitch.is_some())
        });
        assert_eq!(again.comparison.unwrap().pitch.unwrap().regions.len(), 1);

        // The report of either recording describes the comparison.
        let report = f
            .app
            .export_analysis(first, ExportFormat::Report, f.dir.path().join("r.md"))
            .unwrap();
        let report = std::fs::read_to_string(report).unwrap();
        assert!(report.contains("## Comparison with remaster"), "{report}");
        assert!(report.contains("starts 0.500 s later"), "{report}");

        // Removing the second leaves an ordinary single-recording project.
        f.app.set_active_recording(second).unwrap();
        let removed = f.app.remove_recording(second).unwrap();
        assert_eq!(removed.active_recording_id, Some(first));
        assert!(removed.comparison.is_none());
        assert!(f.app.playback_status().has_track);
        assert!(f.app.comparison_curve(first, 0.0, 1.0, 10).midi.is_empty());
        assert_eq!(f.app.map_time(first, second, 1.0), None);
    }

    /// Makes a model look downloaded without downloading it.
    fn pretend_installed(f: &Fixture, model_id: &str) {
        let spec = crate::separation::models::find_model(model_id).unwrap();
        let path = f.app.model_path(model_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::File::create(&path)
            .unwrap()
            .set_len(spec.size_bytes)
            .unwrap();
    }

    fn wait_for_isolation(f: &Fixture) -> IsolationStatus {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let status = f.app.isolation_status();
            if matches!(status.stage, IsolationStage::Idle | IsolationStage::Failed) {
                return status;
            }
            assert!(Instant::now() < deadline, "isolation never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn isolated_vocals_are_made_analysed_heard_and_removed() {
        const MODEL: &str = "kuielab_b_vocals";
        let f = fixture();
        let view = f.app.open_path(f.audio("song.wav", &phrase(RATE))).unwrap();
        let id = view.project.unwrap().recordings[0].id;
        f.wait_for_analysis();

        let models = f.app.separation_models();
        assert_eq!(models.len(), isolation::separation_model_count() as usize);
        assert!(models.iter().all(|m| !m.installed));
        assert_eq!(models.iter().filter(|m| m.recommended).count(), 1);
        match f.app.isolate_vocals(id, "no_such_model".into()) {
            Err(CoreError::Failure { error }) => assert_eq!(error.code, "model_unavailable"),
            Ok(_) => panic!("expected an error"),
        }

        pretend_installed(&f, MODEL);
        assert!(f
            .app
            .separation_models()
            .iter()
            .any(|m| m.id == MODEL && m.installed));

        // A stand-in for the model that passes the left channel through at
        // half level, so the "vocals" are still the same tune.
        let started = f
            .app
            .isolate_vocals_with_loader(
                id,
                MODEL,
                Box::new(|_, _| {
                    Ok(Box::new(ScalingModel {
                        gain: 0.5,
                        runs: Default::default(),
                    }))
                }),
            )
            .unwrap();
        assert_eq!(started.stage, IsolationStage::Preparing);
        assert_eq!(started.recording_id, Some(id));
        assert_eq!(wait_for_isolation(&f).stage, IsolationStage::Idle);

        let stages: Vec<IsolationStage> = f
            .recorder
            .isolation
            .lock()
            .unwrap()
            .iter()
            .map(|status| status.stage)
            .collect();
        assert_eq!(stages.first(), Some(&IsolationStage::Preparing));
        assert!(stages.contains(&IsolationStage::Isolating));
        assert_eq!(stages.last(), Some(&IsolationStage::Idle));

        // The stem is attached, and the analysis is redone from it.
        let ready = f.wait_until("the analysis of the vocals", |view| {
            view.recordings[0].analysed_isolated_vocals
                && view.recordings[0].analysis_status == AnalysisStatus::Ready
        });
        let stem = ready.recordings[0].vocal_stem.clone().unwrap();
        assert_eq!(stem.model_id, MODEL);
        assert_eq!(stem.model_name, "KUIELab MDX-Net B");
        assert!(stem.path.exists());
        assert!(!ready.dirty);
        let analysis = f.app.analysis(id).unwrap();
        assert!(analysis.isolated_vocals);
        assert!(analysis.caution.is_none());
        assert_eq!(analysis.notes.len(), 5);

        // The user can still ask for the original to be analysed instead.
        let chosen = f
            .app
            .set_analysis_source(id, AnalysisSource::Original)
            .unwrap();
        assert!(chosen.dirty);
        let back = f.wait_for_analysis();
        assert!(!back.recordings[0].analysed_isolated_vocals);
        f.app
            .set_analysis_source(id, AnalysisSource::IsolatedVocalsWhenAvailable)
            .unwrap();
        f.wait_until("the vocals to be analysed again", |view| {
            view.recordings[0].analysed_isolated_vocals
        });

        // Listening switches what is played without losing the place.
        f.app.seek(1.0).unwrap();
        let listening = f.app.set_listening_to_vocals(true).unwrap();
        assert!(listening.listening_to_vocals);
        let status = f.app.playback_status();
        assert!(status.has_track);
        assert_eq!(status.position_seconds, 1.0);

        let copy = f
            .app
            .export_vocal_stem(id, f.dir.path().join("vocals only"))
            .unwrap();
        assert_eq!(copy.file_name().unwrap(), "vocals only.wav");
        let info = decode::probe(&copy).unwrap();
        assert_eq!((info.sample_rate_hz, info.channel_count), (44_100, 2));

        // Reopening the same audio finds the stem again without any project.
        f.app.close_project().unwrap();
        let reopened = f.app.open_path(f.dir.path().join("song.wav")).unwrap();
        let id = reopened.project.unwrap().recordings[0].id;
        assert_eq!(
            reopened.recordings[0].vocal_stem.as_ref().unwrap().model_id,
            MODEL
        );
        assert!(!reopened.listening_to_vocals);
        assert!(reopened.recordings[0].analysed_isolated_vocals);

        // Removing the stem falls back to the recording itself everywhere.
        f.app.set_listening_to_vocals(true).unwrap();
        let removed = f.app.remove_vocal_stem(id).unwrap();
        assert!(removed.recordings[0].vocal_stem.is_none());
        assert!(!removed.listening_to_vocals);
        assert!(!stem.path.exists());
        let plain = f.wait_for_analysis();
        assert!(!plain.recordings[0].analysed_isolated_vocals);
        match f.app.export_vocal_stem(id, f.dir.path().join("none.wav")) {
            Err(CoreError::Failure { error }) => assert_eq!(error.code, "invalid_input"),
            Ok(_) => panic!("expected an error"),
        }

        f.app.remove_separation_model(MODEL.into()).unwrap();
        assert!(f.app.separation_models().iter().all(|m| !m.installed));
    }

    #[test]
    fn a_failed_or_cancelled_isolation_leaves_things_as_they_were() {
        const MODEL: &str = "kuielab_b_vocals";
        let f = fixture();
        let view = f.app.open_path(f.audio("song.wav", &phrase(RATE))).unwrap();
        let id = view.project.unwrap().recordings[0].id;
        f.wait_for_analysis();
        pretend_installed(&f, MODEL);

        // The real loader, given a file that is not a model.
        f.app.isolate_vocals(id, MODEL.into()).unwrap();
        let failed = wait_for_isolation(&f);
        assert_eq!(failed.stage, IsolationStage::Failed);
        assert_eq!(failed.error.as_ref().unwrap().code, "separation_failed");
        assert_eq!(failed.recording_id, Some(id));
        assert!(f.app.session().recordings[0].vocal_stem.is_none());

        // Cancelled while the model is "loading": back to idle, no stem.
        let app = f.app.clone();
        f.app
            .isolate_vocals_with_loader(
                id,
                MODEL,
                Box::new(move |_, _| {
                    app.cancel_isolation();
                    Ok(Box::new(ScalingModel {
                        gain: 1.0,
                        runs: Default::default(),
                    }))
                }),
            )
            .unwrap();
        let cancelled = wait_for_isolation(&f);
        assert_eq!(cancelled, IsolationStatus::default());
        assert!(f.app.session().recordings[0].vocal_stem.is_none());
        let stems = f.dir.path().join("data").join("stems");
        assert!(!stems.exists() || std::fs::read_dir(stems).unwrap().count() == 0);

        // Only one job at a time.
        let (release, wait) = std::sync::mpsc::channel::<()>();
        f.app
            .isolate_vocals_with_loader(
                id,
                MODEL,
                Box::new(move |_, _| {
                    let _ = wait.recv();
                    Err(AppError::Cancelled)
                }),
            )
            .unwrap();
        match f.app.isolate_vocals(id, MODEL.into()) {
            Err(CoreError::Failure { error }) => assert_eq!(error.code, "busy"),
            Ok(_) => panic!("expected an error"),
        }
        match f.app.remove_separation_model(MODEL.into()) {
            Err(CoreError::Failure { error }) => assert_eq!(error.code, "busy"),
            Ok(_) => panic!("expected an error"),
        }
        release.send(()).unwrap();
        assert_eq!(wait_for_isolation(&f).stage, IsolationStage::Idle);
    }

    /// Downloads a real model (about 30 MB) and isolates with it. Run
    /// explicitly with `cargo test -- --ignored isolation_smoke`; it is
    /// skipped in CI, which should not depend on the network.
    #[test]
    #[ignore = "downloads a separation model from the internet"]
    fn isolation_smoke() {
        const MODEL: &str = "kuielab_b_vocals";
        let f = fixture();
        let view = f.app.open_path(f.audio("song.wav", &melody(0.0))).unwrap();
        let id = view.project.unwrap().recordings[0].id;
        f.wait_for_analysis();

        let started = f.app.isolate_vocals(id, MODEL.into()).unwrap();
        assert_eq!(started.stage, IsolationStage::Downloading);
        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            let status = f.app.isolation_status();
            match status.stage {
                IsolationStage::Idle => break,
                IsolationStage::Failed => panic!("isolation failed: {:?}", status.error),
                _ => {}
            }
            assert!(Instant::now() < deadline, "isolation never finished");
            std::thread::sleep(Duration::from_millis(50));
        }
        let stages: Vec<IsolationStage> = f
            .recorder
            .isolation
            .lock()
            .unwrap()
            .iter()
            .map(|status| status.stage)
            .collect();
        for stage in [
            IsolationStage::Downloading,
            IsolationStage::Preparing,
            IsolationStage::Isolating,
        ] {
            assert!(stages.contains(&stage), "{stage:?} was never reported");
        }
        assert!(f
            .app
            .separation_models()
            .iter()
            .any(|m| m.id == MODEL && m.installed));

        let ready = f.wait_until("the analysis of the vocals", |view| {
            view.recordings[0].analysed_isolated_vocals
                && view.recordings[0].analysis_status == AnalysisStatus::Ready
        });
        let stem = ready.recordings[0].vocal_stem.clone().unwrap();
        let info = decode::probe(&stem.path).unwrap();
        assert_eq!((info.sample_rate_hz, info.channel_count), (44_100, 2));
        assert!((info.duration_seconds.unwrap() - 14.0).abs() < 0.01);
    }

    #[test]
    fn reports_diagnostics_and_constants() {
        let f = fixture();
        let diagnostics = f.app.diagnostics();
        assert_eq!(diagnostics.application_version, env!("CARGO_PKG_VERSION"));
        assert!(diagnostics.hardware.logical_cpu_count >= 1);
        assert_eq!(diagnostics.database_schema_version, Some(1));
        assert!(supported_audio_extensions().contains(&"mp3".to_string()));
        assert_eq!(project_file_extension(), "vocalscope");
    }
}
