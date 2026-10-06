//! Application settings.
//!
//! Stored as one JSON document so that adding a setting never needs a schema
//! migration: every field has a default, and unknown or missing fields are
//! tolerated when an older or newer build reads the same database.
//!
//! Only settings that do something today are defined here. Sections for
//! features that have not shipped yet (analysis, vocal separation, updates)
//! are added together with those features.

use std::path::PathBuf;

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use super::Database;
use crate::error::AppResult;

const SETTINGS_KEY: &str = "app_settings";

pub const MIN_RECENT_FILE_COUNT: u32 = 1;
pub const MAX_RECENT_FILE_COUNT: u32 = 30;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, uniffi::Record)]
#[serde(default)]
pub struct GeneralSettings {
    pub theme: Theme,
    /// Folder the Save Project dialog starts in. `None` uses the system default.
    pub default_project_directory: Option<PathBuf>,
    pub recent_file_count: u32,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            default_project_directory: None,
            recent_file_count: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, uniffi::Record)]
#[serde(default)]
pub struct PlaybackSettings {
    /// Output device name. `None` follows the system default device.
    pub output_device: Option<String>,
    /// Volume slider position applied at startup, 0.0–1.0.
    pub default_volume: f32,
}

impl Default for PlaybackSettings {
    fn default() -> Self {
        Self {
            output_device: None,
            default_volume: 0.8,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, uniffi::Record)]
#[serde(default)]
pub struct Settings {
    pub general: GeneralSettings,
    pub playback: PlaybackSettings,
}

impl Settings {
    /// Clamps every value into its valid range.
    pub fn sanitized(mut self) -> Self {
        self.general.recent_file_count = self
            .general
            .recent_file_count
            .clamp(MIN_RECENT_FILE_COUNT, MAX_RECENT_FILE_COUNT);
        self.playback.default_volume = if self.playback.default_volume.is_finite() {
            self.playback.default_volume.clamp(0.0, 1.0)
        } else {
            PlaybackSettings::default().default_volume
        };
        if self
            .playback
            .output_device
            .as_deref()
            .is_some_and(|name| name.trim().is_empty())
        {
            self.playback.output_device = None;
        }
        self
    }
}

impl Database {
    /// Loads settings, falling back to defaults when none are stored or the
    /// stored document cannot be understood.
    pub fn load_settings(&self) -> AppResult<Settings> {
        let raw: Option<String> = self
            .conn()
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [SETTINGS_KEY],
                |row| row.get(0),
            )
            .optional()?;
        let settings = match raw {
            Some(json) => serde_json::from_str::<Settings>(&json).unwrap_or_else(|err| {
                log::warn!("stored settings could not be read, using defaults: {err}");
                Settings::default()
            }),
            None => Settings::default(),
        };
        Ok(settings.sanitized())
    }

    pub fn save_settings(&self, settings: &Settings) -> AppResult<Settings> {
        let settings = settings.clone().sanitized();
        let json = serde_json::to_string(&settings)
            .map_err(|err| crate::error::AppError::Internal(err.to_string()))?;
        self.conn().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            (SETTINGS_KEY, json),
        )?;
        Ok(settings)
    }

    pub fn reset_settings(&self) -> AppResult<Settings> {
        self.conn()
            .execute("DELETE FROM settings WHERE key = ?1", [SETTINGS_KEY])?;
        Ok(Settings::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_nothing_is_stored() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.load_settings().unwrap(), Settings::default());
    }

    #[test]
    fn round_trips_and_resets() {
        let db = Database::open_in_memory().unwrap();
        let mut settings = Settings::default();
        settings.general.theme = Theme::Dark;
        settings.general.recent_file_count = 5;
        settings.general.default_project_directory = Some(PathBuf::from("/tmp/projects"));
        settings.playback.output_device = Some("Studio Monitors".into());
        settings.playback.default_volume = 0.35;

        assert_eq!(db.save_settings(&settings).unwrap(), settings);
        assert_eq!(db.load_settings().unwrap(), settings);
        assert_eq!(db.reset_settings().unwrap(), Settings::default());
        assert_eq!(db.load_settings().unwrap(), Settings::default());
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let db = Database::open_in_memory().unwrap();
        let mut settings = Settings::default();
        settings.general.recent_file_count = 5_000;
        settings.playback.default_volume = 7.0;
        settings.playback.output_device = Some("   ".into());
        let saved = db.save_settings(&settings).unwrap();
        assert_eq!(saved.general.recent_file_count, MAX_RECENT_FILE_COUNT);
        assert_eq!(saved.playback.default_volume, 1.0);
        assert_eq!(saved.playback.output_device, None);
    }

    #[test]
    fn tolerates_documents_from_other_versions() {
        let db = Database::open_in_memory().unwrap();
        // A partial document with a field this build does not know about.
        db.conn()
            .execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)",
                (
                    SETTINGS_KEY,
                    r#"{"general":{"theme":"light","future_option":true}}"#,
                ),
            )
            .unwrap();
        let settings = db.load_settings().unwrap();
        assert_eq!(settings.general.theme, Theme::Light);
        assert_eq!(settings.general.recent_file_count, 10);
        assert_eq!(settings.playback, PlaybackSettings::default());

        // Garbage falls back to defaults rather than failing startup.
        db.conn()
            .execute(
                "UPDATE settings SET value = 'not json' WHERE key = ?1",
                [SETTINGS_KEY],
            )
            .unwrap();
        assert_eq!(db.load_settings().unwrap(), Settings::default());
    }
}
