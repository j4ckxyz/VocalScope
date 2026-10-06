//! Well-known locations on disk and cache housekeeping.

use std::path::{Path, PathBuf};

/// Total size the waveform cache is trimmed back to at startup.
pub const WAVEFORM_CACHE_LIMIT_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub log_dir: PathBuf,
}

impl AppPaths {
    pub fn database_file(&self) -> PathBuf {
        self.data_dir.join("vocalscope.db")
    }

    pub fn waveform_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("waveforms")
    }

    /// Cache file for a source's waveform summary. The name is derived from
    /// the file's path, size and modification time, so an edited or replaced
    /// file never reuses a stale summary. `None` if the file cannot be read.
    pub fn waveform_cache_file(&self, source: &Path) -> Option<PathBuf> {
        let metadata = std::fs::metadata(source).ok()?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        let key = format!(
            "{}\n{}\n{}",
            source.to_string_lossy(),
            metadata.len(),
            modified
        );
        Some(
            self.waveform_cache_dir()
                .join(format!("{:016x}.vspk", fnv1a64(key.as_bytes()))),
        )
    }
}

/// FNV-1a. Used only to name cache files; stable across builds and platforms,
/// unlike the standard library's hasher.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Deletes the least recently modified files in `dir` until its total size is
/// at most `max_bytes`. Returns the number of files removed.
pub fn prune_cache_dir(dir: &Path, max_bytes: u64) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| {
                let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
                (modified, metadata.len(), entry.path())
            })
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
    files.sort_by_key(|(modified, _, _)| *modified);

    let mut removed = 0;
    for (_, size, path) in files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &Path) -> AppPaths {
        AppPaths {
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            log_dir: root.join("logs"),
        }
    }

    #[test]
    fn fnv1a_matches_reference_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn cache_key_changes_when_the_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let source = dir.path().join("a.wav");
        assert_eq!(paths.waveform_cache_file(&source), None);

        std::fs::write(&source, b"one").unwrap();
        let first = paths.waveform_cache_file(&source).unwrap();
        assert_eq!(paths.waveform_cache_file(&source).unwrap(), first);
        assert!(first.starts_with(paths.waveform_cache_dir()));

        std::fs::write(&source, b"different length").unwrap();
        assert_ne!(paths.waveform_cache_file(&source).unwrap(), first);
    }

    #[test]
    fn pruning_removes_oldest_files_first() {
        let dir = tempfile::tempdir().unwrap();
        let now = std::time::SystemTime::now();
        for (i, name) in ["old", "middle", "new"].iter().enumerate() {
            let path = dir.path().join(name);
            std::fs::write(&path, vec![0u8; 1_000]).unwrap();
            let age = std::time::Duration::from_secs(3_600 * (3 - i as u64));
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(now - age)
                .unwrap();
        }
        assert_eq!(prune_cache_dir(dir.path(), 5_000), 0);
        assert_eq!(prune_cache_dir(dir.path(), 1_500), 2);
        assert!(!dir.path().join("old").exists());
        assert!(!dir.path().join("middle").exists());
        assert!(dir.path().join("new").exists());
        assert_eq!(prune_cache_dir(&dir.path().join("absent"), 0), 0);
    }
}
