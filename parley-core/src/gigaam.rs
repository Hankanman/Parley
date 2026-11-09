//! Task-owned GigaAM sessions. The registry is deliberately weak: the final
//! task releasing its Arc destroys the sessions and frees their model buffers.
use crate::whisper_engine::{ModelInfo, ModelStatus};
use gigaam_core::Manifest;
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock, Weak},
};
use tokio::sync::Mutex;

pub const PROVIDER: &str = "localGigaAM";
pub const DEFAULT_MODEL: &str = "v3_e2e_rnnt";
static CURRENT: once_cell::sync::Lazy<Arc<Mutex<Weak<Engine>>>> =
    once_cell::sync::Lazy::new(|| Arc::new(Mutex::new(Weak::new())));
static RUNTIME: OnceLock<Result<(), String>> = OnceLock::new();

pub struct Engine {
    pub name: String,
    pub device: String,
    session: Arc<std::sync::Mutex<gigaam_core::Model>>,
}
impl Drop for Engine {
    fn drop(&mut self) {
        log::info!(
            "GigaAM {}: last task finished, releasing {} model resources",
            self.name,
            self.device
        );
    }
}

pub fn is_model(name: &str) -> bool {
    gigaam_core::MODELS.contains(&name)
}
pub fn models_dir() -> Result<PathBuf, String> {
    Ok(crate::paths::app_data_dir()?.join("models/gigaam"))
}
pub fn is_busy() -> bool {
    CURRENT
        .try_lock()
        .map(|w| w.strong_count() != 0)
        .unwrap_or(true)
}
pub async fn active() -> Option<Arc<Engine>> {
    CURRENT.lock().await.upgrade()
}
pub fn validate(name: &str) -> Result<(), String> {
    if !is_model(name) {
        return Err(format!("Unknown GigaAM model: {name}"));
    }
    let m = Manifest::read(&models_dir()?.join(name))
        .map_err(|e| format!("GigaAM {name} is not installed or is damaged: {e}"))?;
    if m.name != name {
        return Err("GigaAM bundle name mismatch".into());
    }
    Ok(())
}
pub fn catalog() -> Vec<ModelInfo> {
    gigaam_core::MODELS
        .iter()
        .map(|&name| {
            let dir = models_dir().unwrap_or_default().join(name);
            let manifest = Manifest::read(&dir);
            ModelInfo {
                name: name.to_string(),
                description: format!(
                    "GigaAM v3 · Russian · {}",
                    if name.contains("e2e") {
                        "punctuation and normalization"
                    } else {
                        "plain text"
                    }
                ),
                size_mb: manifest
                    .as_ref()
                    .map(|m| m.files.values().map(|f| f.bytes).sum::<u64>() / 1024 / 1024)
                    .unwrap_or(0) as u32,
                accuracy: if name.contains("e2e") { "E2E" } else { "ASR" }.to_string(),
                speed: "Native ONNX".to_string(),
                status: if manifest.is_ok() {
                    ModelStatus::Available
                } else {
                    ModelStatus::Missing
                },
                path: dir,
            }
        })
        .collect()
}

pub async fn acquire(name: &str) -> Result<Arc<Engine>, String> {
    validate(name)?;
    let mut registry = CURRENT.clone().lock_owned().await;
    if let Some(engine) = registry.upgrade() {
        if engine.name != name {
            return Err(format!(
                "GigaAM {} is in use; finish the current operation before changing models",
                engine.name
            ));
        }
        return Ok(engine);
    }
    let dir = models_dir()?.join(name);
    let name = name.to_string();
    let engine = tokio::task::spawn_blocking(move || -> Result<Arc<Engine>, String> {
        RUNTIME
            .get_or_init(|| {
                let path = std::env::current_exe()
                    .map_err(|e| e.to_string())?
                    .parent()
                    .ok_or("No executable directory")?
                    .join("libonnxruntime.so");
                gigaam_core::init_runtime(&path)
                    .map_err(|e| format!("Cannot initialize packaged ONNX Runtime: {e}"))
            })
            .clone()?;
        let use_cuda = std::env::var("PARLEY_GIGAAM_DEVICE").as_deref() != Ok("cpu");
        let model = if use_cuda {
            match gigaam_core::Model::load(&dir, true) {
                Ok(m) => m,
                Err(e) => {
                    let detail = format!("{e:#}");
                    if detail.to_lowercase().contains("out of memory")
                        || detail.contains("Failed to allocate")
                    {
                        return Err(format!(
                            "Insufficient GPU memory for GigaAM {name}: {detail}"
                        ));
                    }
                    // CUDA initialization may be unavailable on a CPU host. A
                    // damaged bundle or invalid graph must still fail on CPU.
                    log::warn!("GigaAM CUDA unavailable ({e:#}); loading on CPU");
                    gigaam_core::Model::load(&dir, false).map_err(|e| e.to_string())?
                }
            }
        } else {
            gigaam_core::Model::load(&dir, false).map_err(|e| e.to_string())?
        };
        let device = model.device.to_string();
        log::info!("GigaAM {name} loaded on {device}");
        let engine = Arc::new(Engine {
            name,
            device,
            session: Arc::new(std::sync::Mutex::new(model)),
        });
        *registry = Arc::downgrade(&engine);
        Ok(engine)
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(engine)
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
        let engine = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut model = if partial {
                engine
                    .session
                    .try_lock()
                    .map_err(|_| "GigaAM is busy; skipping preview".to_string())?
            } else {
                engine
                    .session
                    .lock()
                    .map_err(|_| "GigaAM session lock poisoned".to_string())?
            };
            // Normally the pipeline supplies <=24 s segments. Keep this guard
            // for callers outside that pipeline, preserving every input sample.
            let mut texts = Vec::new();
            for part in samples.chunks(gigaam_core::MAX_SAMPLES) {
                if part.len() < 400 {
                    continue;
                }
                texts.push(model.transcribe(part).map_err(|e| e.to_string())?.text);
            }
            Ok::<_, String>(texts.join(" "))
        })
        .await
        .map_err(|e| e.to_string())??;
        Ok((result, None, partial))
    }
}

pub fn runtime_status() -> String {
    match CURRENT.try_lock() {
        Ok(registry) => match registry.upgrade() {
            Some(e) => format!("{} · {} · unloads after processing", e.name, e.device),
            None => "Not loaded · loads on demand · unloads after processing".into(),
        },
        Err(_) => "Loading model…".into(),
    }
}
