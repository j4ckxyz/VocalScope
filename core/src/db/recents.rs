//! Recently opened audio files and projects.

use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::ffi_types::Timestamp;
use serde::{Deserialize, Serialize};

use super::Database;
use crate::error::AppResult;

/// Rows kept in the table regardless of how many the user chooses to display.
const MAX_STORED_RECENTS: u32 = 100;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum RecentKind {
    Audio,
    Project,
}

impl RecentKind {
    fn as_str(self) -> &'static str {
        match self {
            RecentKind::Audio => "audio",
            RecentKind::Project => "project",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "project" => RecentKind::Project,
            _ => RecentKind::Audio,
        }
    }
}

/// What to remember about something the user just opened.
#[derive(Debug, Clone, PartialEq)]
pub struct RecentEntry {
    pub kind: RecentKind,
    pub path: PathBuf,
    /// Main line, e.g. a track title or project name.
    pub title: String,
    pub artist: Option<String>,
    /// Secondary line, e.g. a version label ("Original release").
    pub detail: Option<String>,
    pub duration_seconds: Option<f64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, uniffi::Record)]
pub struct RecentItem {
    pub id: i64,
    pub kind: RecentKind,
    pub path: PathBuf,
    pub file_name: String,
    pub title: String,
    pub artist: Option<String>,
    pub detail: Option<String>,
    pub duration_seconds: Option<f64>,
    pub last_opened_at: Timestamp,
    /// Whether the file is still present on disk right now.
    pub exists: bool,
}

impl Database {
    /// Records that `entry` was opened now, moving it to the top of the list.
    pub fn touch_recent(&self, entry: &RecentEntry) -> AppResult<()> {
        self.touch_recent_at(entry, Utc::now())
    }

    pub(crate) fn touch_recent_at(&self, entry: &RecentEntry, when: Timestamp) -> AppResult<()> {
        self.conn().execute(
            "INSERT INTO recent_items
                 (kind, path, title, artist, detail, duration_seconds, last_opened_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (path) DO UPDATE SET
                 kind = excluded.kind,
                 title = excluded.title,
                 artist = excluded.artist,
                 detail = excluded.detail,
                 duration_seconds = excluded.duration_seconds,
                 last_opened_at = excluded.last_opened_at",
            (
                entry.kind.as_str(),
                entry.path.to_string_lossy(),
                &entry.title,
                &entry.artist,
                &entry.detail,
                entry.duration_seconds,
                when.to_rfc3339(),
            ),
        )?;
        self.conn().execute(
            "DELETE FROM recent_items WHERE id NOT IN
                 (SELECT id FROM recent_items ORDER BY last_opened_at DESC, id DESC LIMIT ?1)",
            [MAX_STORED_RECENTS],
        )?;
        Ok(())
    }

    /// Most recently opened first. Entries whose file has gone missing are
    /// kept and flagged, so the user can locate the file again.
    pub fn list_recents(&self, limit: u32) -> AppResult<Vec<RecentItem>> {
        let mut statement = self.conn().prepare(
            "SELECT id, kind, path, title, artist, detail, duration_seconds, last_opened_at
             FROM recent_items ORDER BY last_opened_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            let kind: String = row.get(1)?;
            let path: String = row.get(2)?;
            let opened: String = row.get(7)?;
            let path = PathBuf::from(path);
            Ok(RecentItem {
                id: row.get(0)?,
                kind: RecentKind::parse(&kind),
                file_name: path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                exists: path.exists(),
                path,
                title: row.get(3)?,
                artist: row.get(4)?,
                detail: row.get(5)?,
                duration_seconds: row.get(6)?,
                last_opened_at: chrono::DateTime::parse_from_rfc3339(&opened)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn get_recent(&self, id: i64) -> AppResult<Option<RecentItem>> {
        Ok(self
            .list_recents(MAX_STORED_RECENTS)?
            .into_iter()
            .find(|item| item.id == id))
    }

    pub fn remove_recent(&self, id: i64) -> AppResult<()> {
        self.conn()
            .execute("DELETE FROM recent_items WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn remove_recent_by_path(&self, path: &Path) -> AppResult<()> {
        self.conn().execute(
            "DELETE FROM recent_items WHERE path = ?1",
            [path.to_string_lossy()],
        )?;
        Ok(())
    }

    pub fn clear_recents(&self) -> AppResult<()> {
        self.conn().execute("DELETE FROM recent_items", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn entry(path: &Path, title: &str) -> RecentEntry {
        RecentEntry {
            kind: RecentKind::Audio,
            path: path.to_path_buf(),
            title: title.to_string(),
            artist: None,
            detail: None,
            duration_seconds: Some(12.5),
        }
    }

    fn at(minute: u32) -> Timestamp {
        Utc.with_ymd_and_hms(2026, 10, 6, 12, minute, 0).unwrap()
    }

    #[test]
    fn lists_most_recent_first_and_respects_the_limit() {
        let db = Database::open_in_memory().unwrap();
        for (i, name) in ["a.wav", "b.wav", "c.wav"].iter().enumerate() {
            db.touch_recent_at(&entry(Path::new(name), name), at(i as u32))
                .unwrap();
        }
        let titles: Vec<_> = db
            .list_recents(10)
            .unwrap()
            .into_iter()
            .map(|item| item.title)
            .collect();
        assert_eq!(titles, ["c.wav", "b.wav", "a.wav"]);
        assert_eq!(db.list_recents(2).unwrap().len(), 2);
    }

    #[test]
    fn reopening_moves_an_item_to_the_top_without_duplicating_it() {
        let db = Database::open_in_memory().unwrap();
        db.touch_recent_at(&entry(Path::new("a.wav"), "A"), at(0))
            .unwrap();
        db.touch_recent_at(&entry(Path::new("b.wav"), "B"), at(1))
            .unwrap();
        let mut updated = entry(Path::new("a.wav"), "A (retitled)");
        updated.artist = Some("Somebody".into());
        db.touch_recent_at(&updated, at(2)).unwrap();

        let items = db.list_recents(10).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "A (retitled)");
        assert_eq!(items[0].artist.as_deref(), Some("Somebody"));
        assert_eq!(items[0].last_opened_at, at(2));
    }

    #[test]
    fn missing_files_are_flagged_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("present.wav");
        std::fs::write(&present, b"x").unwrap();
        let gone = dir.path().join("gone.wav");

        let db = Database::open_in_memory().unwrap();
        db.touch_recent_at(&entry(&present, "present"), at(0))
            .unwrap();
        db.touch_recent_at(&entry(&gone, "gone"), at(1)).unwrap();

        let items = db.list_recents(10).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!((items[0].title.as_str(), items[0].exists), ("gone", false));
        assert_eq!(
            (items[1].title.as_str(), items[1].exists),
            ("present", true)
        );
        assert_eq!(items[1].file_name, "present.wav");
    }

    #[test]
    fn removes_and_clears() {
        let db = Database::open_in_memory().unwrap();
        db.touch_recent_at(&entry(Path::new("a.wav"), "A"), at(0))
            .unwrap();
        db.touch_recent_at(&entry(Path::new("b.wav"), "B"), at(1))
            .unwrap();
        db.touch_recent_at(&entry(Path::new("c.wav"), "C"), at(2))
            .unwrap();

        let id = db.list_recents(10).unwrap()[0].id;
        assert_eq!(db.get_recent(id).unwrap().unwrap().title, "C");
        db.remove_recent(id).unwrap();
        assert!(db.get_recent(id).unwrap().is_none());
        db.remove_recent_by_path(Path::new("a.wav")).unwrap();
        assert_eq!(db.list_recents(10).unwrap().len(), 1);
        db.clear_recents().unwrap();
        assert!(db.list_recents(10).unwrap().is_empty());
    }

    #[test]
    fn the_table_is_capped() {
        let db = Database::open_in_memory().unwrap();
        for i in 0..(MAX_STORED_RECENTS + 20) {
            let name = format!("{i}.wav");
            let when = at(0) + chrono::Duration::seconds(i as i64);
            db.touch_recent_at(&entry(Path::new(&name), &name), when)
                .unwrap();
        }
        let items = db.list_recents(10_000).unwrap();
        assert_eq!(items.len() as u32, MAX_STORED_RECENTS);
        assert_eq!(items[0].title, format!("{}.wav", MAX_STORED_RECENTS + 19));
    }
}
