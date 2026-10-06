//! Reading and writing project files.

use std::io::Write;
use std::path::{Component, Path, PathBuf};

use chrono::Utc;

use super::{Project, PROJECT_FORMAT_VERSION};
use crate::error::{AppError, AppResult};

/// Writes `project` to `path` atomically (temporary file, then rename), so a
/// crash mid-save cannot destroy an existing project file.
pub fn save_project(project: &mut Project, path: &Path) -> AppResult<()> {
    project.project_format_version = PROJECT_FORMAT_VERSION;
    project.application_version = env!("CARGO_PKG_VERSION").to_string();
    project.modified_at = Utc::now();

    let project_dir = path.parent().unwrap_or_else(|| Path::new("."));
    for recording in &mut project.recordings {
        recording.source.relative_path = relative_path(&recording.source.path, project_dir);
    }

    let json = serde_json::to_string_pretty(project)
        .map_err(|err| AppError::Internal(format!("could not serialise the project: {err}")))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("vocalscope.tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(json.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Reads a project file. Source paths that no longer exist are repaired from
/// their project-relative path when the audio moved together with the project.
pub fn load_project(path: &Path) -> AppResult<Project> {
    let text = std::fs::read_to_string(path).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => AppError::FileNotFound(path.to_path_buf()),
        std::io::ErrorKind::InvalidData => AppError::ProjectFormat {
            path: path.to_path_buf(),
            details: "the file is not text".into(),
        },
        _ => AppError::Io(err),
    })?;
    let invalid = |details: String| AppError::ProjectFormat {
        path: path.to_path_buf(),
        details,
    };

    // Check the version before interpreting the rest, so a file from a newer
    // release yields a clear message instead of a confusing parse error.
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|err| invalid(err.to_string()))?;
    let found = value
        .get("project_format_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| invalid("missing project_format_version".into()))? as u32;
    if found > PROJECT_FORMAT_VERSION {
        return Err(AppError::ProjectVersion {
            found,
            supported: PROJECT_FORMAT_VERSION,
        });
    }
    if found == 0 {
        return Err(invalid("project_format_version must be at least 1".into()));
    }
    // Upgrades from older format versions will be applied to `value` here.

    let mut project: Project =
        serde_json::from_value(value).map_err(|err| invalid(err.to_string()))?;

    let project_dir = path.parent().unwrap_or_else(|| Path::new("."));
    for recording in &mut project.recordings {
        if recording.source.path.exists() {
            continue;
        }
        if let Some(relative) = &recording.source.relative_path {
            let candidate = resolve_relative(project_dir, relative);
            if candidate.exists() {
                recording.source.path = candidate;
            }
        }
    }
    Ok(project)
}

/// `path` expressed relative to `base`, or `None` when they share no common
/// root (for example different drives) or either is not absolute.
pub fn relative_path(path: &Path, base: &Path) -> Option<PathBuf> {
    if !path.is_absolute() || !base.is_absolute() {
        return None;
    }
    let path: Vec<Component> = path.components().collect();
    let base: Vec<Component> = base.components().collect();
    let common = path.iter().zip(&base).take_while(|(a, b)| a == b).count();
    // Only a shared root (or nothing) in common is not worth a relative path.
    if common <= 1 {
        return None;
    }
    // Always written with forward slashes, so a project saved on Windows
    // opens on macOS and the other way round.
    let mut parts: Vec<String> = vec!["..".to_string(); base.len() - common];
    parts.extend(
        path[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    Some(PathBuf::from(parts.join("/")))
}

/// Joins a stored project-relative path onto the project's folder, accepting
/// either slash direction whatever platform wrote the file.
fn resolve_relative(project_dir: &Path, relative: &Path) -> PathBuf {
    let mut out = project_dir.to_path_buf();
    for part in relative.to_string_lossy().split(['/', '\\']) {
        if !part.is_empty() {
            out.push(part);
        }
    }
    normalize(&out)
}

/// Resolves `.` and `..` lexically, without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::test_support::audio_info;
    use super::super::{Recording, RecordingLabel, SourceKind};
    use super::*;

    fn sample_project(audio_path: &Path) -> Project {
        let mut project = Project::new("Comparison");
        let mut recording = Recording::new(audio_path, audio_info("take.wav"));
        recording.source.kind = SourceKind::VocalStem;
        recording.label = RecordingLabel {
            recording_name: Some("Lead vocal".into()),
            version: Some("Original release".into()),
            release_year: Some(1976),
            notes: Some("From the multitrack.".into()),
        };
        project.recordings.push(recording);
        project
    }

    #[test]
    fn saves_and_loads_a_project() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("audio").join("take.wav");
        std::fs::create_dir_all(audio.parent().unwrap()).unwrap();
        std::fs::write(&audio, b"fake").unwrap();
        let file = dir.path().join("session.vocalscope");

        let mut project = sample_project(&audio);
        save_project(&mut project, &file).unwrap();
        let loaded = load_project(&file).unwrap();

        assert_eq!(loaded, project);
        assert_eq!(loaded.project_format_version, PROJECT_FORMAT_VERSION);
        assert_eq!(loaded.application_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            loaded.recordings[0].source.relative_path.as_deref(),
            Some(Path::new("audio/take.wav"))
        );
        assert!(!dir.path().join("session.vocalscope.tmp").exists());
    }

    #[test]
    fn saved_file_uses_explicit_field_names() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("p.vocalscope");
        let mut project = sample_project(&dir.path().join("take.wav"));
        save_project(&mut project, &file).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(json["project_format_version"], 1);
        let recording = &json["recordings"][0];
        assert_eq!(recording["source"]["kind"], "vocal_stem");
        assert_eq!(recording["audio"]["sample_rate_hz"], 44_100);
        assert_eq!(recording["label"]["release_year"], 1976);
    }

    #[test]
    fn relocates_audio_that_moved_with_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        std::fs::create_dir_all(original.join("audio")).unwrap();
        let audio = original.join("audio").join("take.wav");
        std::fs::write(&audio, b"fake").unwrap();
        let mut project = sample_project(&audio);
        save_project(&mut project, &original.join("session.vocalscope")).unwrap();

        // Move the whole folder; the absolute path is now stale.
        let moved = dir.path().join("moved");
        std::fs::rename(&original, &moved).unwrap();

        let loaded = load_project(&moved.join("session.vocalscope")).unwrap();
        let source = &loaded.recordings[0].source;
        assert_eq!(source.path, moved.join("audio").join("take.wav"));
        assert!(source.path.exists());
    }

    #[test]
    fn a_missing_source_does_not_prevent_loading() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("take.wav");
        std::fs::write(&audio, b"fake").unwrap();
        let file = dir.path().join("session.vocalscope");
        let mut project = sample_project(&audio);
        save_project(&mut project, &file).unwrap();
        std::fs::remove_file(&audio).unwrap();

        let loaded = load_project(&file).unwrap();
        assert_eq!(loaded.recordings.len(), 1);
        assert_eq!(loaded.recordings[0].source.path, audio);
        assert!(!loaded.recordings[0].source.path.exists());
        assert_eq!(
            loaded.recordings[0].label.recording_name.as_deref(),
            Some("Lead vocal")
        );
    }

    #[test]
    fn rejects_newer_and_malformed_files_with_specific_errors() {
        let dir = tempfile::tempdir().unwrap();

        let newer = dir.path().join("newer.vocalscope");
        std::fs::write(
            &newer,
            r#"{"project_format_version": 99, "shape": "unknown"}"#,
        )
        .unwrap();
        assert!(matches!(
            load_project(&newer).unwrap_err(),
            AppError::ProjectVersion {
                found: 99,
                supported: PROJECT_FORMAT_VERSION
            }
        ));

        let garbage = dir.path().join("garbage.vocalscope");
        std::fs::write(&garbage, "{ not json").unwrap();
        assert!(matches!(
            load_project(&garbage).unwrap_err(),
            AppError::ProjectFormat { .. }
        ));

        let unversioned = dir.path().join("unversioned.vocalscope");
        std::fs::write(&unversioned, r#"{"name": "x"}"#).unwrap();
        assert!(matches!(
            load_project(&unversioned).unwrap_err(),
            AppError::ProjectFormat { .. }
        ));

        let incomplete = dir.path().join("incomplete.vocalscope");
        std::fs::write(&incomplete, r#"{"project_format_version": 1}"#).unwrap();
        assert!(matches!(
            load_project(&incomplete).unwrap_err(),
            AppError::ProjectFormat { .. }
        ));

        let missing = dir.path().join("missing.vocalscope");
        assert!(matches!(
            load_project(&missing).unwrap_err(),
            AppError::FileNotFound(_)
        ));
    }

    #[test]
    fn tolerates_unknown_fields_from_later_minor_changes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("p.vocalscope");
        let mut project = sample_project(&dir.path().join("take.wav"));
        save_project(&mut project, &file).unwrap();

        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        json["future_field"] = serde_json::json!({"anything": true});
        json["recordings"][0]["future_field"] = serde_json::json!(1);
        std::fs::write(&file, json.to_string()).unwrap();

        assert_eq!(load_project(&file).unwrap(), project);
    }

    #[test]
    fn relative_paths_resolve_with_either_slash() {
        let dir = Path::new(if cfg!(windows) {
            "C:\\projects\\study"
        } else {
            "/projects/study"
        });
        let expected = dir.join("audio").join("take.wav");
        assert_eq!(resolve_relative(dir, Path::new("audio/take.wav")), expected);
        assert_eq!(
            resolve_relative(dir, Path::new("audio\\take.wav")),
            expected
        );
        assert_eq!(
            resolve_relative(dir, Path::new("../study/audio/take.wav")),
            expected
        );
    }

    // Uses POSIX-style absolute paths, which are not absolute on Windows.
    #[cfg(unix)]
    #[test]
    fn computes_relative_paths() {
        let rel = |path: &str, base: &str| relative_path(Path::new(path), Path::new(base));
        assert_eq!(rel("/a/b/c.wav", "/a/b"), Some(PathBuf::from("c.wav")));
        assert_eq!(
            rel("/a/b/audio/c.wav", "/a/b"),
            Some(PathBuf::from("audio/c.wav"))
        );
        assert_eq!(rel("/a/x/c.wav", "/a/b"), Some(PathBuf::from("../x/c.wav")));
        // Nothing but the root in common: keep only the absolute path.
        assert_eq!(rel("/elsewhere/c.wav", "/a/b"), None);
        assert_eq!(rel("c.wav", "/a/b"), None);
    }
}
