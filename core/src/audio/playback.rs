//! Playback engine.
//!
//! One dedicated thread owns the audio output stream and the current player.
//! The rest of the application talks to it through [`PlaybackHandle`], which
//! sends an operation and waits for the resulting [`PlaybackStatus`].
//!
//! The output device is opened lazily on first play, so importing and
//! inspecting audio works on machines with no audio output at all.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rodio::cpal::traits::{DeviceTrait, HostTrait};
use rodio::{cpal, Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use serde::Serialize;

use crate::error::{AppError, AppResult};

/// How often the engine refreshes its published position while playing.
const TICK: Duration = Duration::from_millis(16);
/// A reply slower than this means the audio thread is stuck.
const REPLY_TIMEOUT: Duration = Duration::from_secs(8);
/// Pressing play this close to the end restarts from the beginning.
const END_TOLERANCE_SECONDS: f64 = 0.05;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum TransportState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Serialize, PartialEq, uniffi::Record)]
pub struct PlaybackStatus {
    pub state: TransportState,
    pub position_seconds: f64,
    pub duration_seconds: Option<f64>,
    /// Slider position, 0.0–1.0. The applied gain is the square of this.
    pub volume: f32,
    pub muted: bool,
    pub has_track: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, uniffi::Record)]
pub struct OutputDevice {
    pub name: String,
    pub is_default: bool,
}

#[derive(Debug)]
enum Op {
    Load(PathBuf, Option<f64>),
    Switch(PathBuf, Option<f64>, f64),
    Unload,
    Play,
    Pause,
    Stop,
    Seek(f64),
    SetVolume(f32),
    SetMuted(bool),
    SetDuration(f64),
    SetOutputDevice(Option<String>),
}

type Request = (Op, Sender<AppResult<PlaybackStatus>>);

/// Cheap, thread-safe handle to the playback thread.
pub struct PlaybackHandle {
    tx: Sender<Request>,
    status: Arc<Mutex<PlaybackStatus>>,
}

impl PlaybackHandle {
    /// Starts the playback thread. `on_status` is called from that thread
    /// whenever the transport changes (a command ran, the recording ended,
    /// the output device failed). It is *not* called for the steady advance
    /// of the position while playing: UIs read [`PlaybackHandle::status`]
    /// from their own display-rate timer instead, which is far cheaper than
    /// a cross-language callback 30 times a second.
    pub fn spawn(
        initial_volume: f32,
        output_device: Option<String>,
        on_status: impl Fn(&PlaybackStatus) + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<Request>();
        let engine_volume = initial_volume.clamp(0.0, 1.0);
        let status = Arc::new(Mutex::new(PlaybackStatus {
            state: TransportState::Stopped,
            position_seconds: 0.0,
            duration_seconds: None,
            volume: engine_volume,
            muted: false,
            has_track: false,
        }));
        let shared = status.clone();
        std::thread::Builder::new()
            .name("playback".into())
            .spawn(move || {
                let engine = Engine::new(engine_volume, output_device);
                run(engine, rx, shared, on_status);
            })
            .expect("failed to start the playback thread");
        Self { tx, status }
    }

    /// Latest published status, without a round trip to the audio thread.
    pub fn status(&self) -> PlaybackStatus {
        self.status
            .lock()
            .expect("playback status poisoned")
            .clone()
    }

    fn request(&self, op: Op) -> AppResult<PlaybackStatus> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send((op, reply_tx))
            .map_err(|_| AppError::Playback("the playback thread has stopped".into()))?;
        reply_rx
            .recv_timeout(REPLY_TIMEOUT)
            .map_err(|_| AppError::Playback("the audio engine did not respond".into()))?
    }

    /// Makes `path` the current track. When the caller has already probed
    /// the file it passes the duration it found, and the file is not opened
    /// a second time here; with `None` the file is opened to validate it and
    /// read its duration.
    pub fn load(&self, path: &Path, known_duration: Option<f64>) -> AppResult<PlaybackStatus> {
        self.request(Op::Load(path.to_path_buf(), known_duration))
    }

    /// Replaces the current track with another one that is to be heard in
    /// its place — the other version in an A/B comparison, or a recording's
    /// isolated vocals — at `position`, carrying on playing if it was.
    pub fn switch_track(
        &self,
        path: &Path,
        known_duration: Option<f64>,
        position: f64,
    ) -> AppResult<PlaybackStatus> {
        self.request(Op::Switch(path.to_path_buf(), known_duration, position))
    }

    pub fn unload(&self) -> AppResult<PlaybackStatus> {
        self.request(Op::Unload)
    }

    pub fn play(&self) -> AppResult<PlaybackStatus> {
        self.request(Op::Play)
    }

    pub fn pause(&self) -> AppResult<PlaybackStatus> {
        self.request(Op::Pause)
    }

    pub fn stop(&self) -> AppResult<PlaybackStatus> {
        self.request(Op::Stop)
    }

    pub fn seek(&self, seconds: f64) -> AppResult<PlaybackStatus> {
        self.request(Op::Seek(seconds))
    }

    pub fn set_volume(&self, volume: f32) -> AppResult<PlaybackStatus> {
        self.request(Op::SetVolume(volume))
    }

    pub fn set_muted(&self, muted: bool) -> AppResult<PlaybackStatus> {
        self.request(Op::SetMuted(muted))
    }

    /// Supplies the exact duration once a full decode has measured it.
    pub fn set_duration(&self, seconds: f64) -> AppResult<PlaybackStatus> {
        self.request(Op::SetDuration(seconds))
    }

    /// Switches output device; `None` follows the system default.
    pub fn set_output_device(&self, name: Option<String>) -> AppResult<PlaybackStatus> {
        self.request(Op::SetOutputDevice(name))
    }
}

fn run(
    mut engine: Engine,
    rx: Receiver<Request>,
    shared: Arc<Mutex<PlaybackStatus>>,
    on_status: impl Fn(&PlaybackStatus),
) {
    let publish = |engine: &Engine, notify: bool| {
        let status = engine.status();
        *shared.lock().expect("playback status poisoned") = status.clone();
        if notify {
            on_status(&status);
        }
        status
    };
    loop {
        match rx.recv_timeout(TICK) {
            Ok((op, reply)) => {
                let result = engine.apply(op);
                let status = publish(&engine, true);
                let _ = reply.send(result.map(|()| status));
            }
            Err(RecvTimeoutError::Timeout) => {
                let before = engine.state;
                if engine.tick() {
                    publish(&engine, engine.state != before);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

struct Engine {
    output: Option<MixerDeviceSink>,
    output_device: Option<String>,
    /// Set from the audio callback when the output stream reports an error
    /// (for example the device was unplugged).
    output_failed: Arc<AtomicBool>,
    player: Option<Player>,
    track: Option<PathBuf>,
    duration: Option<f64>,
    state: TransportState,
    /// Authoritative while there is no live player; mirrors it otherwise.
    position: f64,
    volume: f32,
    muted: bool,
}

impl Engine {
    fn new(volume: f32, output_device: Option<String>) -> Self {
        Self {
            output: None,
            output_device,
            output_failed: Arc::new(AtomicBool::new(false)),
            player: None,
            track: None,
            duration: None,
            state: TransportState::Stopped,
            position: 0.0,
            volume,
            muted: false,
        }
    }

    fn status(&self) -> PlaybackStatus {
        PlaybackStatus {
            state: self.state,
            position_seconds: self.position,
            duration_seconds: self.duration,
            volume: self.volume,
            muted: self.muted,
            has_track: self.track.is_some(),
        }
    }

    /// Perceptual volume taper: equal slider steps sound like roughly equal
    /// loudness steps.
    fn gain(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            self.volume * self.volume
        }
    }

    fn apply(&mut self, op: Op) -> AppResult<()> {
        match op {
            Op::Load(path, known_duration) => {
                self.unload();
                self.duration = match known_duration {
                    Some(duration) => Some(duration),
                    None => open_decoder(&path)?
                        .total_duration()
                        .map(|d| d.as_secs_f64()),
                };
                self.track = Some(path);
                Ok(())
            }
            Op::Switch(path, known_duration, position) => {
                let resume = self.state == TransportState::Playing;
                let duration = match known_duration {
                    Some(duration) => Some(duration),
                    None => open_decoder(&path)?
                        .total_duration()
                        .map(|d| d.as_secs_f64()),
                };
                self.player = None;
                self.track = Some(path);
                self.duration = duration;
                let position = if position.is_finite() {
                    position.max(0.0)
                } else {
                    0.0
                };
                self.position = duration.map_or(position, |d| position.min(d));
                if resume {
                    // If the new track cannot be played, the transport is
                    // left paused at the same place rather than stopped.
                    self.state = TransportState::Paused;
                    self.play()?;
                }
                Ok(())
            }
            Op::Unload => {
                self.unload();
                Ok(())
            }
            Op::Play => self.play(),
            Op::Pause => {
                if self.state == TransportState::Playing {
                    self.sync_position();
                    if let Some(player) = &self.player {
                        player.pause();
                    }
                    self.state = TransportState::Paused;
                }
                Ok(())
            }
            Op::Stop => {
                self.player = None;
                self.position = 0.0;
                self.state = TransportState::Stopped;
                Ok(())
            }
            Op::Seek(seconds) => self.seek(seconds),
            Op::SetVolume(volume) => {
                self.volume = if volume.is_finite() {
                    volume.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                self.apply_gain();
                Ok(())
            }
            Op::SetMuted(muted) => {
                self.muted = muted;
                self.apply_gain();
                Ok(())
            }
            Op::SetDuration(seconds) => {
                if seconds.is_finite() && seconds >= 0.0 {
                    self.duration = Some(seconds);
                }
                Ok(())
            }
            Op::SetOutputDevice(name) => self.set_output_device(name),
        }
    }

    fn unload(&mut self) {
        self.player = None;
        self.track = None;
        self.duration = None;
        self.position = 0.0;
        self.state = TransportState::Stopped;
    }

    fn apply_gain(&self) {
        if let Some(player) = &self.player {
            player.set_volume(self.gain());
        }
    }

    fn sync_position(&mut self) {
        if let Some(player) = &self.player {
            if !player.empty() {
                self.position = player.get_pos().as_secs_f64();
            }
        }
    }

    fn play(&mut self) -> AppResult<()> {
        if self.track.is_none() {
            return Err(AppError::Playback("no recording is loaded".into()));
        }
        if self.state == TransportState::Playing {
            return Ok(());
        }
        let at_end = self
            .duration
            .is_some_and(|d| self.position >= d - END_TOLERANCE_SECONDS);
        if at_end {
            self.player = None;
            self.position = 0.0;
        }
        if self.player.as_ref().is_none_or(|p| p.empty()) {
            self.start_player()?;
        }
        if let Some(player) = &self.player {
            player.play();
        }
        self.state = TransportState::Playing;
        Ok(())
    }

    fn seek(&mut self, seconds: f64) -> AppResult<()> {
        if self.track.is_none() {
            return Ok(());
        }
        let mut target = if seconds.is_finite() {
            seconds.max(0.0)
        } else {
            0.0
        };
        if let Some(duration) = self.duration {
            target = target.min(duration);
        }
        let live = self.player.as_ref().filter(|p| !p.empty());
        if let Some(player) = live {
            match player.try_seek(Duration::from_secs_f64(target)) {
                Ok(()) => {}
                Err(err) if err.source_intact() => {
                    return Err(AppError::Playback(format!(
                        "seeking is not possible: {err}"
                    )));
                }
                Err(err) => {
                    // The decoder is unusable after a failed seek; rebuild it
                    // at the requested position.
                    log::warn!("seek failed, rebuilding the decoder: {err}");
                    let resume = self.state == TransportState::Playing;
                    self.player = None;
                    self.position = target;
                    self.start_player()?;
                    if resume {
                        if let Some(player) = &self.player {
                            player.play();
                        }
                    }
                    return Ok(());
                }
            }
        } else {
            self.player = None;
        }
        self.position = target;
        Ok(())
    }

    fn set_output_device(&mut self, name: Option<String>) -> AppResult<()> {
        if name == self.output_device {
            return Ok(());
        }
        let resume = self.state == TransportState::Playing;
        self.sync_position();
        self.player = None;
        self.output = None;
        self.output_device = name;
        if resume {
            self.state = TransportState::Paused;
            self.play()?;
        }
        Ok(())
    }

    fn ensure_output(&mut self) -> AppResult<&MixerDeviceSink> {
        if self.output.is_none() {
            let failed = self.output_failed.clone();
            failed.store(false, Ordering::SeqCst);
            let on_error = move |err: cpal::StreamError| {
                log::warn!("audio output stream error: {err}");
                failed.store(true, Ordering::SeqCst);
            };
            let device_error = |err: rodio::DeviceSinkError| AppError::AudioDevice(err.to_string());

            let named = self.output_device.as_deref().and_then(find_output_device);
            if self.output_device.is_some() && named.is_none() {
                log::warn!("the selected output device is not connected; using the system default");
            }
            let builder = match named {
                Some(device) => DeviceSinkBuilder::from_device(device),
                None => DeviceSinkBuilder::from_default_device(),
            }
            .map_err(device_error)?;
            let mut sink = builder
                .with_error_callback(on_error)
                .open_sink_or_fallback()
                .map_err(device_error)?;
            sink.log_on_drop(false);
            self.output = Some(sink);
        }
        Ok(self.output.as_ref().expect("output was just opened"))
    }

    /// Creates a paused player positioned at `self.position`.
    fn start_player(&mut self) -> AppResult<()> {
        let path = self
            .track
            .clone()
            .ok_or_else(|| AppError::Playback("no recording is loaded".into()))?;
        let source = open_decoder(&path)?;
        let gain = self.gain();
        let position = self.position;
        let player = Player::connect_new(self.ensure_output()?.mixer());
        player.pause();
        player.set_volume(gain);
        player.append(source);
        if position > 0.0 {
            player
                .try_seek(Duration::from_secs_f64(position))
                .map_err(|err| AppError::Playback(format!("could not seek: {err}")))?;
        }
        self.player = Some(player);
        Ok(())
    }

    /// Periodic housekeeping. Returns `true` when the status changed.
    fn tick(&mut self) -> bool {
        if self.output_failed.swap(false, Ordering::SeqCst) {
            // Drop the broken stream; the next play reopens an output.
            self.sync_position();
            self.player = None;
            self.output = None;
            if self.state == TransportState::Playing {
                self.state = TransportState::Paused;
            }
            return true;
        }
        if self.state != TransportState::Playing {
            return false;
        }
        match &self.player {
            Some(player) if !player.empty() => {
                self.position = player.get_pos().as_secs_f64();
            }
            _ => {
                // Reached the end of the recording.
                self.player = None;
                self.state = TransportState::Stopped;
                if let Some(duration) = self.duration {
                    self.position = duration;
                }
            }
        }
        true
    }
}

fn open_decoder(path: &Path) -> AppResult<Decoder<File>> {
    let file = File::open(path).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => AppError::FileNotFound(path.to_path_buf()),
        _ => AppError::Io(err),
    })?;
    let byte_len = file.metadata()?.len();
    let mut builder = Decoder::builder()
        .with_data(file)
        .with_byte_len(byte_len)
        .with_seekable(true)
        .with_gapless(true);
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        builder = builder.with_hint(ext);
    }
    builder.build().map_err(|err| AppError::UnsupportedFormat {
        path: path.to_path_buf(),
        details: err.to_string(),
    })
}

fn device_name(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_string())
}

fn find_output_device(name: &str) -> Option<cpal::Device> {
    cpal::default_host()
        .output_devices()
        .ok()?
        .find(|device| device_name(device).as_deref() == Some(name))
}

/// Output devices currently available on the system.
pub fn list_output_devices() -> AppResult<Vec<OutputDevice>> {
    let host = cpal::default_host();
    let default_name = host.default_output_device().as_ref().and_then(device_name);
    let devices = host
        .output_devices()
        .map_err(|err| AppError::AudioDevice(err.to_string()))?
        .filter_map(|device| device_name(&device))
        .map(|name| OutputDevice {
            is_default: Some(&name) == default_name.as_ref(),
            name,
        })
        .collect();
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::super::decode::test_support::write_sine_wav;
    use super::*;

    fn handle() -> PlaybackHandle {
        PlaybackHandle::spawn(0.8, None, |_| {})
    }

    #[test]
    fn loads_a_track_without_opening_an_output_device() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        write_sine_wav(&path, 44_100, 2.0, 0.2, &[440.0]);

        let playback = handle();
        let status = playback.load(&path, None).unwrap();
        assert!(status.has_track);
        assert_eq!(status.state, TransportState::Stopped);
        assert_eq!(status.position_seconds, 0.0);
        assert!((status.duration_seconds.unwrap() - 2.0).abs() < 0.01);
        assert_eq!(playback.status(), status);
    }

    #[test]
    fn a_known_duration_is_trusted_without_opening_the_file() {
        let playback = handle();
        // The file does not exist; with a duration supplied it is not touched.
        let status = playback
            .load(Path::new("/no/such/file.flac"), Some(12.5))
            .unwrap();
        assert!(status.has_track);
        assert_eq!(status.duration_seconds, Some(12.5));
        // The problem surfaces, specifically, when playback is attempted.
        assert!(matches!(
            playback.play().unwrap_err(),
            AppError::FileNotFound(_)
        ));
        assert_eq!(playback.status().state, TransportState::Stopped);
    }

    #[test]
    fn seeking_while_stopped_moves_and_clamps_the_position() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        write_sine_wav(&path, 44_100, 2.0, 0.2, &[440.0]);

        let playback = handle();
        playback.load(&path, None).unwrap();
        assert_eq!(playback.seek(1.25).unwrap().position_seconds, 1.25);
        assert_eq!(playback.seek(-3.0).unwrap().position_seconds, 0.0);
        assert!((playback.seek(99.0).unwrap().position_seconds - 2.0).abs() < 0.01);
        assert_eq!(playback.seek(f64::NAN).unwrap().position_seconds, 0.0);
        assert_eq!(playback.stop().unwrap().position_seconds, 0.0);
    }

    #[test]
    fn volume_and_mute_are_tracked_and_clamped() {
        let playback = handle();
        assert_eq!(playback.status().volume, 0.8);
        assert_eq!(playback.set_volume(1.7).unwrap().volume, 1.0);
        assert_eq!(playback.set_volume(-1.0).unwrap().volume, 0.0);
        assert!(playback.set_muted(true).unwrap().muted);
        assert!(!playback.set_muted(false).unwrap().muted);
    }

    #[test]
    fn switching_tracks_keeps_the_place_and_the_transport_state() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("a.wav");
        let second = dir.path().join("b.wav");
        write_sine_wav(&first, 44_100, 4.0, 0.2, &[440.0]);
        write_sine_wav(&second, 44_100, 3.0, 0.2, &[660.0]);

        let playback = handle();
        playback.load(&first, None).unwrap();
        playback.seek(1.5).unwrap();
        let status = playback.switch_track(&second, None, 2.0).unwrap();
        assert!(status.has_track);
        assert_eq!(status.state, TransportState::Stopped);
        assert_eq!(status.position_seconds, 2.0);
        assert!((status.duration_seconds.unwrap() - 3.0).abs() < 0.01);

        // Positions beyond the new track are clamped; nonsense becomes zero.
        let status = playback.switch_track(&first, Some(4.0), 9.0).unwrap();
        assert_eq!(status.position_seconds, 4.0);
        let status = playback.switch_track(&first, Some(4.0), f64::NAN).unwrap();
        assert_eq!(status.position_seconds, 0.0);

        let missing = playback.switch_track(&dir.path().join("no.wav"), None, 0.0);
        assert!(matches!(missing.unwrap_err(), AppError::FileNotFound(_)));
        assert!(playback.status().has_track, "the old track stays loaded");
    }

    #[test]
    fn load_errors_are_specific() {
        let playback = handle();
        let missing = playback
            .load(Path::new("/no/such/file.flac"), None)
            .unwrap_err();
        assert!(matches!(missing, AppError::FileNotFound(_)));

        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("bogus.wav");
        std::fs::write(&bogus, b"not audio ".repeat(500)).unwrap();
        let unsupported = playback.load(&bogus, None).unwrap_err();
        assert!(matches!(unsupported, AppError::UnsupportedFormat { .. }));
        assert!(!playback.status().has_track);
    }

    #[test]
    fn play_without_a_track_is_an_error_and_unload_resets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        write_sine_wav(&path, 44_100, 1.0, 0.2, &[440.0]);

        let playback = handle();
        assert!(matches!(
            playback.play().unwrap_err(),
            AppError::Playback(_)
        ));
        playback.load(&path, None).unwrap();
        playback.seek(0.5).unwrap();
        let status = playback.unload().unwrap();
        assert!(!status.has_track);
        assert_eq!(status.position_seconds, 0.0);
        assert_eq!(status.duration_seconds, None);
    }

    /// Exercises the real output device, silently. Run explicitly with
    /// `cargo test -- --ignored playback_smoke`; it is skipped in CI, where
    /// no audio device exists.
    #[test]
    #[ignore = "requires an audio output device"]
    fn playback_smoke() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        write_sine_wav(&path, 44_100, 3.0, 0.2, &[440.0]);

        let playback = handle();
        playback.set_muted(true).unwrap();
        playback.load(&path, None).unwrap();

        assert_eq!(playback.play().unwrap().state, TransportState::Playing);
        std::thread::sleep(Duration::from_millis(600));
        let playing = playback.status();
        assert!(
            playing.position_seconds > 0.3 && playing.position_seconds < 1.0,
            "position after 600 ms was {}",
            playing.position_seconds
        );

        let paused = playback.pause().unwrap();
        assert_eq!(paused.state, TransportState::Paused);
        std::thread::sleep(Duration::from_millis(200));
        assert!((playback.status().position_seconds - paused.position_seconds).abs() < 0.02);

        let sought = playback.seek(2.0).unwrap();
        assert!((sought.position_seconds - 2.0).abs() < 0.02);
        playback.play().unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert!(playback.status().position_seconds > 2.2);

        // Let it run off the end.
        std::thread::sleep(Duration::from_millis(1_000));
        let ended = playback.status();
        assert_eq!(ended.state, TransportState::Stopped);
        assert!((ended.position_seconds - 3.0).abs() < 0.05);

        // Play again restarts from the top.
        playback.play().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let restarted = playback.status();
        assert_eq!(restarted.state, TransportState::Playing);
        assert!(restarted.position_seconds < 1.0);
        playback.stop().unwrap();
    }
}
