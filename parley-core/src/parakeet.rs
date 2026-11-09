//! Parakeet TDT v3 through the shared sherpa-onnx runtime. Sessions belong to
//! transcription tasks; the weak registry releases weights after the last task.
use crate::{
    events::{EventSinkExt, SharedEventSink},
    utils::download::{DownloadGuard, DownloadRequest, download_file},
    whisper_engine::{ModelInfo, ModelStatus},
};
use sha2::{Digest, Sha256};
mod native;
use native::Recognizer;
use ort::execution_providers::CUDAExecutionProvider;
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::sync::Mutex;

pub const PROVIDER: &str = "localParakeet";
pub const DEFAULT_MODEL: &str = "parakeet-tdt-0.6b-v3";
pub const CPU_MODEL: &str = "parakeet-tdt-0.6b-v3-int8";
pub const MAX_SAMPLES: usize = 24 * 16_000;
const CPU_SOURCE: &str = "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/resolve/2bda32ec70b097a55adaa07d9a7173915b43cc78";
const CPU_FILES: &[(&str, u64, &str)] = &[
    (
        "encoder.int8.onnx",
        652184281,
        "acfc2b4456377e15d04f0243af540b7fe7c992f8d898d751cf134c3a55fd2247",
    ),
    (
        "decoder.int8.onnx",
        11845275,
        "179e50c43d1a9de79c8a24149a2f9bac6eb5981823f2a2ed88d655b24248db4e",
    ),
    (
        "joiner.int8.onnx",
        6355277,
        "3164c13fc2821009440d20fcb5fdc78bff28b4db2f8d0f0b329101719c0948b3",
    ),
    (
        "tokens.txt",
        93939,
        "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
    ),
];
const GPU_SOURCE: &str = "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3/resolve/1a468a35cbba69418f126de829e75261dea4a4e4";
const GPU_FILES: &[(&str, u64, &str)] = &[
    (
        "encoder.onnx",
        41766257,
        "3eed7ce424bf8339ad09233533c687e2dbd07e74ccf5027b5e7344019ea373b0",
    ),
    (
        "encoder.weights",
        2435420160,
        "3af3f51af5f2d01dbbf5af47d42c7962a2c205f11004254bb4f2b979862f39a8",
    ),
    (
        "decoder.onnx",
        47233743,
        "d593cdb0e571f5a457ec2219af9968cbf6b0e8198e8f7839b40a8754593bf68c",
    ),
    (
        "joiner.onnx",
        25286330,
        "b9b0bcf88ac571902e69a6536223ed2d94885e981b85045410f1403d53121a63",
    ),
    (
        "tokens.txt",
        93939,
        "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
    ),
];
struct ModelSpec {
    name: &'static str,
    source: &'static str,
    files: &'static [(&'static str, u64, &'static str)],
    int8: bool,
}
const SPECS: &[ModelSpec] = &[
    ModelSpec {
        name: DEFAULT_MODEL,
        source: GPU_SOURCE,
        files: GPU_FILES,
        int8: false,
    },
    ModelSpec {
        name: CPU_MODEL,
        source: CPU_SOURCE,
        files: CPU_FILES,
        int8: true,
    },
];
fn spec(name: &str) -> Result<&'static ModelSpec, String> {
    SPECS
        .iter()
        .find(|m| m.name == name)
        .ok_or_else(|| format!("Unknown Parakeet model: {name}"))
}
static CURRENT: once_cell::sync::Lazy<Arc<Mutex<Weak<Engine>>>> =
    once_cell::sync::Lazy::new(|| Arc::new(Mutex::new(Weak::new())));
// Serialize installation, removal and loading, including while a cancelled
// download winds down. A cancellation request never releases this lock early.
static FILE_OP: once_cell::sync::Lazy<Arc<Mutex<()>>> =
    once_cell::sync::Lazy::new(|| Arc::new(Mutex::new(())));
static DOWNLOAD: once_cell::sync::Lazy<DownloadGuard> =
    once_cell::sync::Lazy::new(DownloadGuard::new);

pub struct Engine {
    pub name: String,
    pub device: String,
    session: std::sync::Mutex<Recognizer>,
}
pub fn is_model(name: &str) -> bool {
    SPECS.iter().any(|m| m.name == name)
}
pub fn models_dir() -> Result<PathBuf, String> {
    Ok(crate::paths::app_data_dir()?.join("models/parakeet"))
}
pub fn is_busy() -> bool {
    CURRENT
        .try_lock()
        .map(|r| r.strong_count() != 0)
        .unwrap_or(true)
}
fn check_name(name: &str) -> Result<(), String> {
    if is_model(name) {
        Ok(())
    } else {
        Err(format!("Unknown Parakeet model: {name}"))
    }
}
fn verify_file(path: &Path, size: u64, hash: &str) -> Result<(), String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if file.metadata().map_err(|e| e.to_string())?.len() != size {
        return Err(format!("Invalid size: {}", path.display()));
    }
    let mut digest = Sha256::new();
    let mut buf = [0; 65536];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    if format!("{:x}", digest.finalize()) != hash {
        return Err(format!("Checksum mismatch: {}", path.display()));
    }
    Ok(())
}
fn verify_bundle(dir: &Path, model: &ModelSpec) -> Result<(), String> {
    for &(file, size, hash) in model.files {
        verify_file(&dir.join(file), size, hash)?;
    }
    Ok(())
}
pub fn validate(name: &str) -> Result<(), String> {
    check_name(name)?;
    verify_bundle(&models_dir()?.join(name), spec(name)?)
        .map_err(|e| format!("Parakeet is missing or damaged; download it in Settings: {e}"))
}
pub fn catalog() -> Vec<ModelInfo> {
    SPECS
        .iter()
        .map(|model| {
            let dir = models_dir().unwrap_or_default().join(model.name);
            let complete = model
                .files
                .iter()
                .all(|(f, size, _)| dir.join(f).metadata().is_ok_and(|m| m.len() == *size));
            let bytes: u64 = model.files.iter().map(|f| f.1).sum();
            let device = if model.int8 {
                "CPU INT8"
            } else {
                "CUDA / CPU FP32"
            };
            ModelInfo {
                name: model.name.into(),
                description: format!("Parakeet TDT v3 · 25 languages including Russian · {device}"),
                size_mb: (bytes / 1024 / 1024) as u32,
                accuracy: "Multilingual".into(),
                speed: device.into(),
                status: if complete {
                    ModelStatus::Available
                } else if dir.exists() {
                    ModelStatus::Corrupted {
                        file_size: model
                            .files
                            .iter()
                            .filter_map(|(f, _, _)| dir.join(f).metadata().ok())
                            .map(|m| m.len())
                            .sum(),
                        expected_min_size: bytes,
                    }
                } else {
                    ModelStatus::Missing
                },
                path: dir,
            }
        })
        .collect()
}
fn cuda_ready() -> Result<(), String> {
    let path = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or("No executable directory")?
        .join("libonnxruntime.so");
    gigaam_core::init_runtime(&path).map_err(|e| e.to_string())?;
    // Unlike an availability query, registration also loads CUDA/cuDNN and
    // returns an error when their shared libraries cannot be initialized.
    let _builder = ort::session::Session::builder()
        .map_err(|e| e.to_string())?
        .with_execution_providers([CUDAExecutionProvider::default().build().error_on_failure()])
        .map_err(|e| e.to_string())?;
    Ok(())
}
fn is_out_of_memory(error: &str) -> bool {
    let error = error.to_lowercase();
    error.contains("out of memory")
        || error.contains("failed to allocate")
        || error.contains("cudaerrormemoryallocation")
}
pub async fn acquire(name: &str) -> Result<Arc<Engine>, String> {
    let model = spec(name)?;
    let mut registry = CURRENT.clone().lock_owned().await;
    if let Some(e) = registry.upgrade() {
        if e.name != name {
            return Err(format!(
                "Parakeet {} is in use; finish the current operation before changing models",
                e.name
            ));
        }
        return Ok(e);
    }
    let files = FILE_OP.clone().lock_owned().await;
    let dir = models_dir()?.join(name);
    tokio::task::spawn_blocking(move || {
        let _files = files;
        verify_bundle(&dir, model)?;
        let requested = std::env::var("PARLEY_PARAKEET_DEVICE").unwrap_or_else(|_| "auto".into());
        if !matches!(requested.as_str(), "auto" | "cpu" | "cuda") {
            return Err("PARLEY_PARAKEET_DEVICE must be auto, cpu or cuda".into());
        }
        let profile = std::env::var_os("PARLEY_PARAKEET_PROFILE").map(PathBuf::from);
        let mut device = "CPU";
        let use_cuda = !model.int8 && requested != "cpu";
        let ready = if use_cuda { cuda_ready() } else { Ok(()) };
        let recognizer = if use_cuda && ready.is_ok() {
            match Recognizer::load(&dir, model.int8, "cuda", profile.as_deref()) {
                Ok(r) => {
                    device = "CUDA";
                    r
                }
                Err(e) if is_out_of_memory(&e) || requested == "cuda" => {
                    return Err(format!("Parakeet CUDA failed: {e}"));
                }
                Err(e) => {
                    log::warn!("Parakeet CUDA unavailable ({e}); loading on CPU");
                    Recognizer::load(&dir, model.int8, "cpu", profile.as_deref())?
                }
            }
        } else {
            if let Err(e) = ready {
                if requested == "cuda" {
                    return Err(format!("Parakeet CUDA unavailable: {e}"));
                }
                log::warn!("Parakeet CUDA unavailable ({e}); loading on CPU");
            }
            Recognizer::load(&dir, model.int8, "cpu", profile.as_deref())?
        };
        let engine = Arc::new(Engine {
            name: model.name.into(),
            device: device.into(),
            session: std::sync::Mutex::new(recognizer),
        });
        *registry = Arc::downgrade(&engine);
        log::info!("Parakeet {} loaded on {}", engine.name, engine.device);
        Ok(engine)
    })
    .await
    .map_err(|e| e.to_string())?
}
impl Engine {
    pub async fn transcribe(
        self: &Arc<Self>,
        samples: Vec<f32>,
        partial: bool,
    ) -> Result<(String, Option<f32>, bool), String> {
        if samples.is_empty() {
            return Ok((String::new(), None, partial));
        }
        if partial && crate::audio::transcription::queue::queue_depth() > 0 {
            return Err("Final transcription has priority".into());
        }
        if samples.iter().any(|x| !x.is_finite()) {
            return Err("Invalid audio samples".into());
        }
        let engine = self.clone();
        let text = tokio::task::spawn_blocking(move || {
            let recognizer = if partial {
                engine
                    .session
                    .try_lock()
                    .map_err(|_| "Parakeet is busy; skipping preview")?
            } else {
                engine
                    .session
                    .lock()
                    .map_err(|_| "Parakeet session lock poisoned")?
            };
            let mut texts = Vec::new();
            for part in samples.chunks(MAX_SAMPLES) {
                texts.push(recognizer.transcribe(part)?.trim().to_string());
            }
            Ok::<_, String>(texts.join(" "))
        })
        .await
        .map_err(|e| e.to_string())??;
        Ok((text, None, partial))
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        log::info!("Parakeet: last task finished, releasing model resources");
    }
}
pub fn runtime_status() -> String {
    match CURRENT.try_lock() {
        Ok(registry) => match registry.upgrade() {
            Some(e) => format!("{} · {} · unloads after processing", e.name, e.device),
            None => "CUDA or CPU · loads on demand · unloads after processing".into(),
        },
        Err(_) => "Loading model…".into(),
    }
}

pub async fn download(sink: SharedEventSink, name: String) -> Result<(), String> {
    let result = async {
        check_name(&name)?;
        let _operation = FILE_OP
            .try_lock()
            .map_err(|_| "Parakeet model files are in use")?;
        if is_busy() {
            return Err("Finish Parakeet transcription before downloading".into());
        }
        DOWNLOAD.begin(&name).await.map_err(|e| e.to_string())?;
        let result = download_inner(&sink, &name).await;
        DOWNLOAD.finish(&name).await;
        result
    }
    .await;
    match &result {
        Ok(()) => {
            let _ = sink.emit_event(
                "model-download-complete",
                &serde_json::json!({"modelName": name}),
            );
        }
        Err(e) => {
            let _ = sink.emit_event(
                "model-download-error",
                &serde_json::json!({"modelName": name, "error": e}),
            );
        }
    }
    result
}
async fn download_inner(sink: &SharedEventSink, name: &str) -> Result<(), String> {
    let model = spec(name)?;
    let root = models_dir()?;
    let staging = root.join(format!(".{name}.download"));
    tokio::fs::create_dir_all(&staging)
        .await
        .map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let total: u64 = model.files.iter().map(|f| f.1).sum();
    let mut done = 0;
    for &(file, size, hash) in model.files {
        if DOWNLOAD.is_cancel_requested(name).await {
            return Err("Download cancelled".into());
        }
        let dest = staging.join(file);
        let valid = {
            let p = dest.clone();
            tokio::task::spawn_blocking(move || verify_file(&p, size, hash))
                .await
                .map_err(|e| e.to_string())?
                .is_ok()
        };
        if !valid {
            download_file(
                DownloadRequest {
                    client: &client,
                    url: &format!("{}/{file}", model.source),
                    dest_path: &dest,
                    resume: false,
                    chunk_timeout: Some(Duration::from_secs(60)),
                },
                &DOWNLOAD,
                name,
                |bytes, _, _| {
                    let progress = ((done + bytes).min(total) * 100 / total) as u8;
                    let _ = sink.emit_event(
                        "model-download-progress",
                        &serde_json::json!({"modelName": name, "progress": progress}),
                    );
                },
            )
            .await
            .map_err(|e| e.to_string())?;
            tokio::task::spawn_blocking(move || verify_file(&dest, size, hash))
                .await
                .map_err(|e| e.to_string())??;
        }
        done += size;
    }
    if DOWNLOAD.is_cancel_requested(name).await {
        return Err("Download cancelled".into());
    }
    let installed = root.join(name);
    // All hashes have passed. Keep any previous installation until this point.
    if installed.exists() {
        tokio::fs::remove_dir_all(&installed)
            .await
            .map_err(|e| e.to_string())?;
    }
    tokio::fs::rename(staging, installed)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}
pub async fn cancel(name: &str) -> Result<(), String> {
    check_name(name)?;
    DOWNLOAD.request_cancel(name).await;
    Ok(())
}
pub async fn delete(name: &str) -> Result<(), String> {
    check_name(name)?;
    let _files = FILE_OP
        .try_lock()
        .map_err(|_| "Parakeet model files are in use")?;
    if is_busy() {
        return Err("Finish Parakeet transcription before deleting".into());
    }
    let dir = models_dir()?.join(name);
    if dir.exists() {
        tokio::fs::remove_dir_all(dir)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn integrity_rejects_truncation_and_same_size_corruption() {
        let temp = tempfile::tempdir().unwrap();
        let f = temp.path().join("weights");
        std::fs::write(&f, b"abc").unwrap();
        let hash = format!("{:x}", Sha256::digest(b"abc"));
        assert!(verify_file(&f, 3, &hash).is_ok());
        assert!(verify_file(&f, 4, &hash).is_err());
        std::fs::write(&f, b"abd").unwrap();
        assert!(verify_file(&f, 3, &hash).is_err());
        assert!(verify_bundle(temp.path(), spec(CPU_MODEL).unwrap()).is_err());
    }
}
