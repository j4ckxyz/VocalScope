//! The project data model and its on-disk format.
//!
//! A project is a small JSON document (`*.vocalscope`) that *references*
//! audio files; it never contains audio. Field names are spelled out in full
//! because project and export files are meant to be readable by other tools.
//!
//! ```text
//! Project
//!   └── Recording
//!         ├── source   (where the audio lives, what kind of audio it is)
//!         ├── audio    (technical metadata + embedded tags, as read)
//!         ├── waveform (facts measured by a full decode)
//!         └── label    (what the user says this recording is)
//! ```
//!
//! A project holds one recording, or two when versions are being compared
//! (the first is the reference). Analyses and isolated vocals are not stored
//! here: they are derived from the audio, cached by the application, and
//! made again when missing, which is what keeps a project file tiny.

pub mod io;

use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::ffi_types::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::audio::peaks::WaveformSummary;
use crate::audio::AudioInfo;

/// Bump when the document shape changes incompatibly, and teach
/// [`io::load_project`] to upgrade the previous version.
pub const PROJECT_FORMAT_VERSION: u32 = 1;
pub const PROJECT_FILE_EXTENSION: &str = "vocalscope";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, uniffi::Record)]
pub struct Project {
    pub project_format_version: u32,
    /// Version of VocalScope that last wrote the file.
    pub application_version: String,
    pub id: Uuid,
    pub name: String,
    pub created_at: Timestamp,
    pub modified_at: Timestamp,
    pub recordings: Vec<Recording>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, uniffi::Record)]
pub struct Recording {
    pub id: Uuid,
    pub source: SourceFile,
    pub audio: AudioInfo,
    /// Present once the file has been fully decoded at least once.
    #[serde(default)]
    pub waveform: Option<WaveformSummary>,
    #[serde(default)]
    pub label: RecordingLabel,
    /// Which audio the pitch analysis listens to.
    #[serde(default)]
    pub analysis_source: AnalysisSource,
    pub added_at: Timestamp,
}

/// What the pitch analysis is run on.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisSource {
    /// The vocals VocalScope isolated from this recording, once that has
    /// been done; the recording itself until then.
    #[default]
    IsolatedVocalsWhenAvailable,
    /// Always the recording as it is.
    Original,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, uniffi::Record)]
pub struct SourceFile {
    /// Absolute path at the time the file was added or last located.
    pub path: PathBuf,
    /// Path relative to the project file, written on save so a project folder
    /// can be moved together with its audio.
    #[serde(default)]
    pub relative_path: Option<PathBuf>,
    pub size_bytes: u64,
    #[serde(default)]
    pub modified_at: Option<Timestamp>,
    #[serde(default)]
    pub kind: SourceKind,
}

/// What the audio is, as declared by the user. VocalScope does not guess.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    #[default]
    Unspecified,
    /// A complete song with instruments.
    FullMix,
    /// A vocals-only stem supplied by the user (not produced by VocalScope).
    VocalStem,
}

impl SourceKind {
    pub fn description(self) -> Option<&'static str> {
        match self {
            SourceKind::Unspecified => None,
            SourceKind::FullMix => Some("Full mix"),
            SourceKind::VocalStem => Some("Vocal stem"),
        }
    }
}

/// Free-form labelling, mainly for telling versions apart in comparisons.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, uniffi::Record)]
#[serde(default)]
pub struct RecordingLabel {
    pub recording_name: Option<String>,
    /// For example "Original release" or "2011 remaster".
    pub version: Option<String>,
    pub release_year: Option<i32>,
    pub notes: Option<String>,
}

impl RecordingLabel {
    /// Trims text and turns blank strings into `None`.
    pub fn normalized(self) -> Self {
        fn clean(value: Option<String>) -> Option<String> {
            value
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        }
        Self {
            recording_name: clean(self.recording_name),
            version: clean(self.version),
            release_year: self
                .release_year
                .filter(|year| (1000..=2999).contains(year)),
            notes: clean(self.notes),
        }
    }
}

impl Recording {
    pub fn new(path: &Path, audio: AudioInfo) -> Self {
        let metadata = std::fs::metadata(path).ok();
        Self {
            id: Uuid::new_v4(),
            source: SourceFile {
                path: path.to_path_buf(),
                relative_path: None,
                size_bytes: metadata.as_ref().map_or(audio.file_size_bytes, |m| m.len()),
                modified_at: metadata
                    .and_then(|m| m.modified().ok())
                    .map(Timestamp::from),
                kind: SourceKind::Unspecified,
            },
            audio,
            waveform: None,
            label: RecordingLabel::default(),
            analysis_source: AnalysisSource::default(),
            added_at: Utc::now(),
        }
    }

    /// Best available one-line name: the user's label, then embedded tags,
    /// then the file name. Never invents information.
    pub fn display_title(&self) -> String {
        if let Some(name) = &self.label.recording_name {
            return name.clone();
        }
        match (&self.audio.tags.artist, &self.audio.tags.title) {
            (Some(artist), Some(title)) => format!("{artist} — {title}"),
            (None, Some(title)) => title.clone(),
            _ => Path::new(&self.audio.file_name)
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .filter(|stem| !stem.is_empty())
                .unwrap_or_else(|| "Unknown track".to_string()),
        }
    }

    /// Secondary line for lists: version label and source kind.
    pub fn display_detail(&self) -> Option<String> {
        let parts: Vec<&str> = [
            self.label.version.as_deref(),
            self.source.kind.description(),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// Exact measured duration when available, otherwise the declared one.
    pub fn duration_seconds(&self) -> Option<f64> {
        self.waveform
            .as_ref()
            .map(|w| w.duration_seconds)
            .or(self.audio.duration_seconds)
    }
}

impl Project {
    pub fn new(name: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            project_format_version: PROJECT_FORMAT_VERSION,
            application_version: env!("CARGO_PKG_VERSION").to_string(),
            id: Uuid::new_v4(),
            name: name.into(),
            created_at: now,
            modified_at: now,
            recordings: Vec::new(),
        }
    }

    pub fn recording(&self, id: Uuid) -> Option<&Recording> {
        self.recordings.iter().find(|r| r.id == id)
    }

    pub fn recording_mut(&mut self, id: Uuid) -> Option<&mut Recording> {
        self.recordings.iter_mut().find(|r| r.id == id)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::audio::Tags;

    pub fn audio_info(file_name: &str) -> AudioInfo {
        AudioInfo {
            file_name: file_name.to_string(),
            file_extension: Some("wav".into()),
            file_size_bytes: 1_234,
            codec: "pcm_s16le".into(),
            codec_description: "PCM Signed 16-bit Little-Endian Interleaved".into(),
            sample_rate_hz: 44_100,
            channel_count: 2,
            bit_depth: Some(16),
            frame_count: Some(441_000),
            duration_seconds: Some(10.0),
            average_bitrate_kbps: Some(1_411),
            tags: Tags::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::audio_info;
    use super::*;

    #[test]
    fn display_title_prefers_label_then_tags_then_file_name() {
        let mut recording = Recording::new(
            Path::new("/music/somebody_to_love.wav"),
            audio_info("somebody_to_love.wav"),
        );
        assert_eq!(recording.display_title(), "somebody_to_love");

        recording.audio.tags.title = Some("Somebody To Love".into());
        assert_eq!(recording.display_title(), "Somebody To Love");

        recording.audio.tags.artist = Some("Queen".into());
        assert_eq!(recording.display_title(), "Queen — Somebody To Love");

        recording.label.recording_name = Some("STL (lead vocal)".into());
        assert_eq!(recording.display_title(), "STL (lead vocal)");
    }

    #[test]
    fn display_detail_combines_version_and_kind() {
        let mut recording = Recording::new(Path::new("a.wav"), audio_info("a.wav"));
        assert_eq!(recording.display_detail(), None);
        recording.source.kind = SourceKind::VocalStem;
        assert_eq!(recording.display_detail().as_deref(), Some("Vocal stem"));
        recording.label.version = Some("Original release".into());
        assert_eq!(
            recording.display_detail().as_deref(),
            Some("Original release · Vocal stem")
        );
    }

    #[test]
    fn label_normalisation_drops_blank_and_implausible_values() {
        let label = RecordingLabel {
            recording_name: Some("  Take 3  ".into()),
            version: Some("   ".into()),
            release_year: Some(42),
            notes: Some(String::new()),
        }
        .normalized();
        assert_eq!(label.recording_name.as_deref(), Some("Take 3"));
        assert_eq!(label.version, None);
        assert_eq!(label.release_year, None);
        assert_eq!(label.notes, None);
    }

    #[test]
    fn measured_duration_wins_over_declared() {
        use crate::audio::StereoContent;
        let mut recording = Recording::new(Path::new("a.wav"), audio_info("a.wav"));
        assert_eq!(recording.duration_seconds(), Some(10.0));
        recording.waveform = Some(WaveformSummary {
            sample_rate_hz: 44_100,
            channel_count: 2,
            frame_count: 441_441,
            duration_seconds: 10.01,
            stereo_content: StereoContent::Stereo,
            peak_amplitude: 0.5,
            peak_dbfs: Some(-6.02),
        });
        assert_eq!(recording.duration_seconds(), Some(10.01));
    }
}
