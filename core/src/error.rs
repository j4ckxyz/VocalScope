//! Application error type.
//!
//! Every error that can reach the UI is converted into a [`UserError`]: a
//! plain-language title and message, an optional suggestion, and the raw
//! technical details kept separately for the "Show technical details" view.

use std::path::PathBuf;

use serde::Serialize;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("file not found: {0}")]
    FileNotFound(PathBuf),

    #[error("unsupported or unrecognised audio format ({details})")]
    UnsupportedFormat { path: PathBuf, details: String },

    #[error("audio decoding failed ({details})")]
    Decode { path: PathBuf, details: String },

    #[error("audio output device unavailable ({0})")]
    AudioDevice(String),

    #[error("playback failed ({0})")]
    Playback(String),

    #[error("invalid project file ({details})")]
    ProjectFormat { path: PathBuf, details: String },

    #[error("project format version {found} is newer than supported version {supported}")]
    ProjectVersion { found: u32, supported: u32 },

    #[error("no project is open")]
    NoProject,

    #[error("recording not found: {0}")]
    RecordingNotFound(String),

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    InvalidInput(String),

    #[error("operation cancelled")]
    Cancelled,

    #[error("internal error: {0}")]
    Internal(String),
}

/// What the UI shows. `details` is only revealed on request.
#[derive(Debug, Clone, Serialize, PartialEq, uniffi::Record)]
pub struct UserError {
    /// Stable machine-readable identifier, e.g. `file_not_found`.
    pub code: String,
    pub title: String,
    pub message: String,
    pub suggestion: Option<String>,
    pub details: Option<String>,
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

impl AppError {
    pub fn code(&self) -> &'static str {
        match self {
            AppError::FileNotFound(_) => "file_not_found",
            AppError::UnsupportedFormat { .. } => "unsupported_format",
            AppError::Decode { .. } => "decode_failed",
            AppError::AudioDevice(_) => "audio_device_unavailable",
            AppError::Playback(_) => "playback_failed",
            AppError::ProjectFormat { .. } => "project_format_invalid",
            AppError::ProjectVersion { .. } => "project_version_unsupported",
            AppError::NoProject => "no_project",
            AppError::RecordingNotFound(_) => "recording_not_found",
            AppError::Database(_) => "database_error",
            AppError::Io(_) => "io_error",
            AppError::InvalidInput(_) => "invalid_input",
            AppError::Cancelled => "cancelled",
            AppError::Internal(_) => "internal_error",
        }
    }

    pub fn to_user_error(&self) -> UserError {
        let details = Some(self.to_string());
        let (title, message, suggestion): (&str, String, Option<&str>) = match self {
            AppError::FileNotFound(path) => (
                "File not found",
                format!(
                    "“{}” could not be found. It may have been moved, renamed or deleted, or it may be on a drive that is not connected.",
                    file_name(path)
                ),
                Some("Reconnect the drive, or locate the file again."),
            ),
            AppError::UnsupportedFormat { path, .. } => (
                "This file could not be opened",
                format!(
                    "“{}” is not in an audio format VocalScope can read, or the file is damaged.",
                    file_name(path)
                ),
                Some("Supported formats are WAV, AIFF, FLAC, MP3, AAC/M4A, ALAC and Ogg Vorbis."),
            ),
            AppError::Decode { path, .. } => (
                "The audio could not be decoded",
                format!(
                    "VocalScope recognised “{}” but ran into a problem reading its audio data. The file may be incomplete or corrupted.",
                    file_name(path)
                ),
                Some("Try re-exporting or re-downloading the file."),
            ),
            AppError::AudioDevice(_) => (
                "No audio output available",
                "VocalScope could not open an audio output device, so playback is unavailable. Analysis and the waveform still work.".to_string(),
                Some("Check that speakers or headphones are connected, or choose a different output device in Settings › Playback."),
            ),
            AppError::Playback(_) => (
                "Playback failed",
                "Something went wrong while playing this recording.".to_string(),
                Some("Try stopping and starting playback again."),
            ),
            AppError::ProjectFormat { path, .. } => (
                "This project could not be opened",
                format!(
                    "“{}” is not a valid VocalScope project file, or it has been damaged.",
                    file_name(path)
                ),
                None,
            ),
            AppError::ProjectVersion { found, supported } => (
                "This project was made with a newer version of VocalScope",
                format!(
                    "The project uses format version {found}, but this version of VocalScope only understands up to version {supported}."
                ),
                Some("Update VocalScope to open this project."),
            ),
            AppError::NoProject => (
                "Nothing is open",
                "Open an audio file or a project first.".to_string(),
                None,
            ),
            AppError::RecordingNotFound(_) => (
                "Recording not found",
                "That recording is no longer part of the open project.".to_string(),
                None,
            ),
            AppError::Database(_) => (
                "VocalScope could not access its local database",
                "Recent files and settings could not be read or saved.".to_string(),
                Some("Check that your disk is not full. You can keep working; this session's changes may not be remembered."),
            ),
            AppError::Io(err) => (
                "A file operation failed",
                match err.kind() {
                    std::io::ErrorKind::PermissionDenied => {
                        "VocalScope does not have permission to read or write that location.".to_string()
                    }
                    std::io::ErrorKind::NotFound => {
                        "The file or folder could not be found.".to_string()
                    }
                    _ => "The file could not be read or written.".to_string(),
                },
                Some("Check the file's location and permissions, and that your disk is not full."),
            ),
            AppError::InvalidInput(message) => ("That didn't work", message.clone(), None),
            AppError::Cancelled => (
                "Cancelled",
                "The operation was cancelled.".to_string(),
                None,
            ),
            AppError::Internal(_) => (
                "Something went wrong",
                "VocalScope ran into an unexpected problem.".to_string(),
                Some("If this keeps happening, please report it and include the technical details."),
            ),
        };
        UserError {
            code: self.code().to_string(),
            title: title.to_string(),
            message,
            suggestion: suggestion.map(str::to_string),
            details,
        }
    }
}

/// The error type that crosses the FFI boundary: always a [`UserError`],
/// ready to be shown as-is by a native alert.
#[derive(Debug, Clone, thiserror::Error, uniffi::Error)]
pub enum CoreError {
    #[error("{}: {}", error.title, error.message)]
    Failure { error: UserError },
}

impl From<AppError> for CoreError {
    fn from(err: AppError) -> Self {
        CoreError::Failure {
            error: err.to_user_error(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_error_hides_path_but_names_file() {
        let err = AppError::FileNotFound(PathBuf::from("/Users/someone/Music/take one.wav"));
        let user = err.to_user_error();
        assert_eq!(user.code, "file_not_found");
        assert!(user.message.contains("take one.wav"));
        assert!(!user.message.contains("/Users/someone"));
        assert!(user.details.unwrap().contains("/Users/someone/Music"));
    }

    #[test]
    fn converts_to_the_ffi_error() {
        let CoreError::Failure { error } = AppError::ProjectVersion {
            found: 9,
            supported: 1,
        }
        .into();
        assert_eq!(error.code, "project_version_unsupported");
        assert!(error.message.contains("version 9"));
        assert!(error.suggestion.is_some());
    }
}
