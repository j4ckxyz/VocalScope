//! Local log file.
//!
//! Records versions, the hardware profile, operation timings and errors.
//! It never records audio, and file *names* only — not full paths, which
//! would contain the user's account name.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use log::{Level, LevelFilter, Log, Metadata, Record};

const LOG_FILE: &str = "vocalscope.log";
const PREVIOUS_LOG_FILE: &str = "vocalscope.previous.log";
/// The log is rotated once it reaches this size; one previous file is kept.
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

struct FileLogger {
    file: Mutex<Option<File>>,
    echo_to_stderr: bool,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        // Our own messages at info and above; dependencies only when
        // something is wrong (decoders are chatty at info level).
        let ours = metadata.target().starts_with("vocalscope");
        metadata.level() <= if ours { Level::Info } else { Level::Warn }
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} {:5} [{}] {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            record.level(),
            record.target(),
            record.args()
        );
        if self.echo_to_stderr {
            eprint!("{line}");
        }
        if let Ok(mut guard) = self.file.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = file.write_all(line.as_bytes());
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = file.flush();
            }
        }
    }
}

/// Moves an oversized log aside so the active file stays small.
fn rotate(dir: &Path) {
    let current = dir.join(LOG_FILE);
    let too_big = std::fs::metadata(&current).is_ok_and(|m| m.len() >= MAX_LOG_BYTES);
    if too_big {
        let _ = std::fs::rename(&current, dir.join(PREVIOUS_LOG_FILE));
    }
}

fn open(dir: &Path) -> Option<File> {
    std::fs::create_dir_all(dir).ok()?;
    rotate(dir);
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOG_FILE))
        .ok()
}

/// Starts logging to `dir`. Safe to call more than once; only the first call
/// installs the logger. If the directory cannot be written, logging quietly
/// falls back to standard error only.
pub fn init(dir: &Path) -> PathBuf {
    let logger = FileLogger {
        file: Mutex::new(open(dir)),
        echo_to_stderr: cfg!(debug_assertions),
    };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
    dir.join(LOG_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_only_oversized_logs() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join(LOG_FILE);
        let previous = dir.path().join(PREVIOUS_LOG_FILE);

        std::fs::write(&current, b"small").unwrap();
        rotate(dir.path());
        assert!(current.exists() && !previous.exists());

        std::fs::write(&current, vec![b'x'; MAX_LOG_BYTES as usize]).unwrap();
        rotate(dir.path());
        assert!(!current.exists() && previous.exists());

        // Opening creates a fresh file and appends to it.
        let mut file = open(dir.path()).unwrap();
        file.write_all(b"hello\n").unwrap();
        assert_eq!(std::fs::read_to_string(&current).unwrap(), "hello\n");
    }

    #[test]
    fn filters_dependency_chatter() {
        let logger = FileLogger {
            file: Mutex::new(None),
            echo_to_stderr: false,
        };
        let meta =
            |target: &'static str, level| Metadata::builder().target(target).level(level).build();
        assert!(logger.enabled(&meta("vocalscope_core::app", Level::Info)));
        assert!(!logger.enabled(&meta("vocalscope_core::app", Level::Debug)));
        assert!(!logger.enabled(&meta("symphonia_bundle_mp3::demuxer", Level::Info)));
        assert!(logger.enabled(&meta("symphonia_bundle_mp3::demuxer", Level::Warn)));
    }
}
