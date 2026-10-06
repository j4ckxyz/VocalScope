//! Local SQLite store for settings and the recent-files list.
//!
//! Audio never goes in here — only paths and small pieces of metadata. The
//! schema is versioned with `PRAGMA user_version` and migrated forward on
//! open; it is deliberately independent of how the UI presents the data.

pub mod recents;
pub mod settings;

use std::path::Path;

use rusqlite::Connection;

use crate::error::AppResult;

/// Ordered list of migrations. Entry `n` upgrades schema version `n` to
/// `n + 1`. Never edit an entry that has shipped; append a new one.
const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    "CREATE TABLE settings (
         key   TEXT PRIMARY KEY,
         value TEXT NOT NULL
     );
     CREATE TABLE recent_items (
         id               INTEGER PRIMARY KEY,
         kind             TEXT NOT NULL CHECK (kind IN ('audio', 'project')),
         path             TEXT NOT NULL UNIQUE,
         title            TEXT NOT NULL,
         artist           TEXT,
         detail           TEXT,
         duration_seconds REAL,
         last_opened_at   TEXT NOT NULL
     );
     CREATE INDEX recent_items_last_opened ON recent_items (last_opened_at DESC);",
];

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &Path) -> AppResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> AppResult<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> AppResult<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn schema_version(&self) -> AppResult<u32> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    fn migrate(&mut self) -> AppResult<()> {
        let current = self.schema_version()? as usize;
        for (index, sql) in MIGRATIONS.iter().enumerate().skip(current) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (index + 1) as u32)?;
            tx.commit()?;
        }
        Ok(())
    }

    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_a_fresh_database_to_the_latest_version() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap() as usize, MIGRATIONS.len());
    }

    #[test]
    fn reopening_keeps_data_and_does_not_rerun_migrations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("vocalscope.db");
        {
            let db = Database::open(&path).unwrap();
            db.conn()
                .execute(
                    "INSERT INTO settings (key, value) VALUES ('probe', '1')",
                    [],
                )
                .unwrap();
        }
        let db = Database::open(&path).unwrap();
        let value: String = db
            .conn()
            .query_row(
                "SELECT value FROM settings WHERE key = 'probe'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(value, "1");
        assert_eq!(db.schema_version().unwrap() as usize, MIGRATIONS.len());
    }
}
