//! The vocal-isolation models VocalScope knows about, choosing between them
//! and fetching them.
//!
//! No model ships with the application. One is downloaded only when the user
//! asks for it, after being shown its size and licence, and it is checked
//! against a known checksum before it is ever loaded.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::mdx::MdxParameters;
use crate::error::{AppError, AppResult};
use crate::hardware::HardwareProfile;

const DOWNLOAD_BASE: &str =
    "https://github.com/TRvlvr/model_repo/releases/download/all_public_uvr_models/";
/// The full-size models want this much memory and this many cores to run
/// at a reasonable speed alongside everything else.
const FULL_MODEL_MIN_MEMORY_BYTES: u64 = 7 * 1024 * 1024 * 1024;
const FULL_MODEL_MIN_CORES: u32 = 6;

/// Everything fixed about one model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub file_name: &'static str,
    pub size_bytes: u64,
    pub sha256: &'static str,
    /// The licence as stated by whoever published the weights, or a plain
    /// statement that none was given.
    pub license: &'static str,
    pub source: &'static str,
    pub parameters: MdxParameters,
    /// Full-size models are slower and better.
    pub full_size: bool,
}

/// Every model offered, best first.
pub const MODELS: [ModelSpec; 3] = [
    ModelSpec {
        id: "kim_vocal_2",
        name: "Kim Vocal 2",
        description: "Best quality. Slower, and needs about 2.5 GB of free memory.",
        file_name: "Kim_Vocal_2.onnx",
        size_bytes: 66_759_214,
        sha256: "ce74ef3b6a6024ce44211a07be9cf8bc6d87728cc852a68ab34eb8e58cde9c8b",
        license: "Not stated by the publisher",
        source: "Kimberley Jensen, distributed with Ultimate Vocal Remover",
        parameters: MdxParameters {
            n_fft: 7_680,
            dim_f: 3_072,
            compensation: 1.009,
        },
        full_size: true,
    },
    ModelSpec {
        id: "uvr_mdxnet_voc_ft",
        name: "UVR-MDX-NET Voc FT",
        description: "An alternative full-size model; sometimes cleaner on dense mixes.",
        file_name: "UVR-MDX-NET-Voc_FT.onnx",
        size_bytes: 66_762_490,
        sha256: "534b2070fcc7df514b13ef660dc8cbb328679c2374d04354a5c42bb14ecce111",
        license: "Not stated by the publisher",
        source: "Ultimate Vocal Remover",
        parameters: MdxParameters {
            n_fft: 7_680,
            dim_f: 3_072,
            compensation: 1.021,
        },
        full_size: true,
    },
    ModelSpec {
        id: "kuielab_b_vocals",
        name: "KUIELab MDX-Net B",
        description: "Smaller and faster. Good for older or lower-memory computers.",
        file_name: "kuielab_b_vocals.onnx",
        size_bytes: 29_703_204,
        sha256: "9b7dcb9d878acb0f3e64ff3fd27750faae96577013f6d50f5996875bf4250713",
        license: "MIT",
        source: "KUIELab, Korea University (MDX-Net)",
        parameters: MdxParameters {
            n_fft: 6_144,
            dim_f: 2_048,
            compensation: 1.035,
        },
        full_size: false,
    },
];

pub fn find_model(id: &str) -> AppResult<&'static ModelSpec> {
    MODELS
        .iter()
        .find(|model| model.id == id)
        .ok_or_else(|| AppError::ModelUnavailable(id.to_string()))
}

/// The model that suits this computer: the best one it can run comfortably.
pub fn recommended_model(hardware: &HardwareProfile) -> &'static ModelSpec {
    let capable = hardware.total_memory_bytes >= FULL_MODEL_MIN_MEMORY_BYTES
        && hardware.logical_cpu_count >= FULL_MODEL_MIN_CORES;
    MODELS
        .iter()
        .find(|model| model.full_size == capable)
        .unwrap_or(&MODELS[0])
}

/// CPU threads to give the model: the physical cores, leaving the interface
/// and playback something to run on.
pub fn inference_threads(hardware: &HardwareProfile) -> usize {
    let cores = hardware
        .physical_core_count
        .unwrap_or(hardware.logical_cpu_count)
        .max(1) as usize;
    cores.saturating_sub(1).clamp(1, 8)
}

/// A model as the UI lists it.
#[derive(Debug, Clone, Serialize, PartialEq, uniffi::Record)]
pub struct SeparationModel {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Size of the download.
    pub size_bytes: u64,
    pub license: String,
    pub source: String,
    /// Already downloaded.
    pub installed: bool,
    /// The one suggested for this computer.
    pub recommended: bool,
}

/// Where downloaded models are kept.
#[derive(Debug, Clone)]
pub struct ModelStore {
    directory: PathBuf,
}

impl ModelStore {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub fn path(&self, model: &ModelSpec) -> PathBuf {
        self.directory.join(model.file_name)
    }

    /// A model counts as installed when its file is present at its full
    /// size; the checksum was verified when it was downloaded.
    pub fn is_installed(&self, model: &ModelSpec) -> bool {
        std::fs::metadata(self.path(model)).is_ok_and(|m| m.len() == model.size_bytes)
    }

    pub fn list(&self, hardware: &HardwareProfile) -> Vec<SeparationModel> {
        let recommended = recommended_model(hardware).id;
        MODELS
            .iter()
            .map(|model| SeparationModel {
                id: model.id.to_string(),
                name: model.name.to_string(),
                description: model.description.to_string(),
                size_bytes: model.size_bytes,
                license: model.license.to_string(),
                source: model.source.to_string(),
                installed: self.is_installed(model),
                recommended: model.id == recommended,
            })
            .collect()
    }

    pub fn remove(&self, model: &ModelSpec) -> AppResult<()> {
        match std::fs::remove_file(self.path(model)) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
            _ => Ok(()),
        }
    }

    /// Downloads a model from its published location.
    pub fn download(
        &self,
        model: &ModelSpec,
        cancel: &AtomicBool,
        on_progress: impl FnMut(f32),
    ) -> AppResult<PathBuf> {
        let url = format!("{DOWNLOAD_BASE}{}", model.file_name);
        self.download_from(model, &url, cancel, on_progress)
    }

    /// Downloads to a temporary file, verifies size and checksum, and only
    /// then moves it into place.
    pub fn download_from(
        &self,
        model: &ModelSpec,
        url: &str,
        cancel: &AtomicBool,
        mut on_progress: impl FnMut(f32),
    ) -> AppResult<PathBuf> {
        std::fs::create_dir_all(&self.directory)?;
        let target = self.path(model);
        let partial = target.with_extension("part");
        let result = fetch(url, &partial, model, cancel, &mut on_progress);
        match result {
            Ok(()) => {
                std::fs::rename(&partial, &target)?;
                Ok(target)
            }
            Err(err) => {
                let _ = std::fs::remove_file(&partial);
                Err(err)
            }
        }
    }
}

fn fetch(
    url: &str,
    partial: &Path,
    model: &ModelSpec,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(f32),
) -> AppResult<()> {
    let network = |err: &dyn std::fmt::Display| AppError::Network(err.to_string());
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_body(Some(Duration::from_secs(60 * 30)))
        .build()
        .into();
    let mut response = agent.get(url).call().map_err(|err| network(&err))?;
    let mut body = response.body_mut().as_reader();

    let mut file = std::fs::File::create(partial)?;
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut block = vec![0u8; 256 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(AppError::Cancelled);
        }
        let count = body.read(&mut block).map_err(|err| network(&err))?;
        if count == 0 {
            break;
        }
        received += count as u64;
        // A server sending more than the model's size is not sending the model.
        if received > model.size_bytes {
            return Err(AppError::ModelCorrupt(format!(
                "more than the expected {} bytes arrived",
                model.size_bytes
            )));
        }
        hasher.update(&block[..count]);
        file.write_all(&block[..count])?;
        on_progress(received as f32 / model.size_bytes as f32);
    }
    file.sync_all()?;
    if received != model.size_bytes {
        return Err(AppError::Network(format!(
            "the connection closed after {received} of {} bytes",
            model.size_bytes
        )));
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if digest != model.sha256 {
        return Err(AppError::ModelCorrupt(format!(
            "expected {}, got {digest}",
            model.sha256
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    use super::*;

    fn hardware(memory_gb: u64, cores: u32) -> HardwareProfile {
        HardwareProfile {
            os_name: "Test".into(),
            os_version: "1".into(),
            kernel_version: None,
            cpu_model: "Test CPU".into(),
            cpu_architecture: "arm64".into(),
            logical_cpu_count: cores,
            physical_core_count: Some(cores),
            is_apple_silicon: true,
            total_memory_bytes: memory_gb * 1024 * 1024 * 1024,
            available_memory_bytes: memory_gb * 512 * 1024 * 1024,
            memory_used_fraction: 0.5,
            cpu_usage_percent: None,
            memory_pressure: None,
        }
    }

    /// Serves `body` once over HTTP on a local port and returns its URL.
    fn serve(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/model.onnx", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                line.clear();
            }
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(&body);
        });
        url
    }

    fn tiny_model(body: &[u8]) -> ModelSpec {
        let digest: String = Sha256::digest(body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        ModelSpec {
            file_name: "tiny.onnx",
            size_bytes: body.len() as u64,
            // Leaked so the test can build a spec with a computed checksum.
            sha256: Box::leak(digest.into_boxed_str()),
            ..MODELS[2]
        }
    }

    #[test]
    fn the_registry_is_complete_and_consistent() {
        for model in &MODELS {
            assert_eq!(model.sha256.len(), 64, "{}", model.id);
            assert!(model.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(model.file_name.ends_with(".onnx"));
            assert!(model.size_bytes > 1_000_000);
            assert!(!model.license.is_empty() && !model.source.is_empty());
            // The kept bins must exist in the transform.
            assert!(model.parameters.dim_f <= model.parameters.n_fft / 2 + 1);
            assert!((1.0..1.1).contains(&model.parameters.compensation));
            assert_eq!(find_model(model.id).unwrap(), model);
        }
        assert!(matches!(
            find_model("nope").unwrap_err(),
            AppError::ModelUnavailable(_)
        ));
    }

    #[test]
    fn the_recommendation_follows_the_hardware() {
        assert_eq!(recommended_model(&hardware(16, 10)).id, "kim_vocal_2");
        assert_eq!(recommended_model(&hardware(8, 6)).id, "kim_vocal_2");
        assert_eq!(recommended_model(&hardware(4, 8)).id, "kuielab_b_vocals");
        assert_eq!(recommended_model(&hardware(16, 4)).id, "kuielab_b_vocals");
        assert_eq!(inference_threads(&hardware(16, 10)), 8);
        assert_eq!(inference_threads(&hardware(8, 4)), 3);
        assert_eq!(inference_threads(&hardware(8, 1)), 1);
    }

    #[test]
    fn a_download_is_verified_then_installed() {
        let dir = tempfile::tempdir().unwrap();
        let store = ModelStore::new(dir.path().join("models"));
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let model = tiny_model(&body);
        assert!(!store.is_installed(&model));

        let mut reports = Vec::new();
        let path = store
            .download_from(&model, &serve(body.clone()), &AtomicBool::new(false), |f| {
                reports.push(f)
            })
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), body);
        assert!(store.is_installed(&model));
        assert_eq!(*reports.last().unwrap(), 1.0);
        assert!(reports.windows(2).all(|w| w[0] <= w[1]));
        assert!(!path.with_extension("part").exists());

        store.remove(&model).unwrap();
        assert!(!store.is_installed(&model));
        store.remove(&model).unwrap();
    }

    #[test]
    fn a_wrong_or_short_download_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let store = ModelStore::new(dir.path().to_path_buf());
        let body = vec![7u8; 50_000];
        let model = tiny_model(&body);

        let mut tampered = body.clone();
        tampered[100] ^= 1;
        let err = store
            .download_from(&model, &serve(tampered), &AtomicBool::new(false), |_| {})
            .unwrap_err();
        assert!(matches!(err, AppError::ModelCorrupt(_)), "{err:?}");

        let err = store
            .download_from(
                &model,
                &serve(body[..10_000].to_vec()),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap_err();
        assert!(matches!(err, AppError::Network(_)), "{err:?}");

        let mut longer = body.clone();
        longer.extend([0u8; 10]);
        let err = store
            .download_from(&model, &serve(longer), &AtomicBool::new(false), |_| {})
            .unwrap_err();
        assert!(matches!(err, AppError::ModelCorrupt(_)), "{err:?}");

        let err = store
            .download_from(&model, &serve(body), &AtomicBool::new(true), |_| {})
            .unwrap_err();
        assert!(matches!(err, AppError::Cancelled));

        assert!(!store.is_installed(&model));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn an_unreachable_server_is_a_network_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = ModelStore::new(dir.path().to_path_buf());
        // A port nothing is listening on.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = store
            .download_from(
                &MODELS[2],
                &format!("http://127.0.0.1:{port}/x"),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap_err();
        assert!(matches!(err, AppError::Network(_)), "{err:?}");
    }

    #[test]
    fn the_listing_marks_installed_and_recommended_models() {
        let dir = tempfile::tempdir().unwrap();
        let store = ModelStore::new(dir.path().to_path_buf());
        let listed = store.list(&hardware(4, 4));
        assert_eq!(listed.len(), MODELS.len());
        assert!(listed.iter().all(|m| !m.installed));
        let recommended: Vec<_> = listed.iter().filter(|m| m.recommended).collect();
        assert_eq!(recommended.len(), 1);
        assert_eq!(recommended[0].id, "kuielab_b_vocals");
        assert_eq!(recommended[0].license, "MIT");
    }
}
