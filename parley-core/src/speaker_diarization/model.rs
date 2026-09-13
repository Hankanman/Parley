//! Speaker model file resolution and download coordination.
//!
//! Models live under `<app_data_dir>/speaker_models/`. The default model
//! ([`DEFAULT_MODEL_FILENAME`]) is the 3D-Speaker CAM++ English checkpoint —
//! 28MB, 192-dim output, runs in <50ms on CPU per segment. Different model
//! files don't change the public API; the embedding dimension is read from
//! the loaded ONNX at runtime.

use once_cell::sync::Lazy;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Default speaker-embedding model. English-trained (VoxCeleb), 192-dim,
/// fast on CPU. The 3D-Speaker family ships its preprocessing inside the
/// ONNX graph, so we feed raw 16 kHz mono audio directly.
pub const DEFAULT_MODEL_FILENAME: &str =
    "3dspeaker_speech_campplus_sv_en_voxceleb_16k.onnx";

/// Silero VAD ONNX model used by sherpa-onnx's `VoiceActivityDetector`.
/// Lives alongside the speaker embedding model since both are sherpa-onnx
/// runtime artefacts. Tiny (~2.3 MB).
pub const SILERO_VAD_FILENAME: &str = "silero_vad.onnx";

/// Pyannote segmentation model (ONNX) used by sherpa-onnx's *offline* speaker
/// diarization. Unlike the online cosine clusterer, offline diarization does
/// proper global clustering (and can be told the exact number of speakers),
/// which is far more accurate. ~5.7 MB. Only needed for Import / re-diarize.
pub const PYANNOTE_SEG_FILENAME: &str = "pyannote_segmentation_3_0.onnx";

/// Direct ONNX download (HuggingFace), matching the single-file downloader.
const PYANNOTE_SEG_URL: &str =
    "https://huggingface.co/csukuangfj/sherpa-onnx-pyannote-segmentation-3-0/resolve/main/model.onnx";

const MODEL_BASE_URL: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models";

/// Sherpa's silero-vad model lives in a different release tag than the
/// speaker recognition models.
const SILERO_VAD_URL: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx";

static MODELS_DIR: Lazy<Mutex<Option<PathBuf>>> = Lazy::new(|| Mutex::new(None));

/// Called once at app startup (from `lib.rs::setup`). Resolves
/// `<app_data_dir>/speaker_models/`, creates it if missing, and caches the
/// path so command handlers can look it up without an `AppHandle`.
pub fn set_models_dir() {
    let Ok(app_data_dir) = crate::paths::app_data_dir() else {
        log::warn!("Failed to resolve app_data_dir for speaker models");
        return;
    };
    let dir = app_data_dir.join("speaker_models");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::error!("Failed to create speaker_models dir: {}", e);
        return;
    }
    log::info!("Speaker models directory: {}", dir.display());
    if let Ok(mut guard) = MODELS_DIR.lock() {
        *guard = Some(dir);
    }
}

/// Configured models directory, or `None` if `set_models_dir` hasn't run yet.
pub fn models_dir() -> Option<PathBuf> {
    MODELS_DIR.lock().ok().and_then(|g| g.clone())
}

/// Filename of the active speaker embedding model.
pub fn model_filename() -> &'static str {
    DEFAULT_MODEL_FILENAME
}

/// Full path to the speaker embedding model. `None` if `set_models_dir`
/// hasn't run.
pub fn default_model_path() -> Option<PathBuf> {
    models_dir().map(|d| d.join(DEFAULT_MODEL_FILENAME))
}

/// Download URL for the speaker embedding model.
pub fn model_download_url() -> String {
    format!("{}/{}", MODEL_BASE_URL, DEFAULT_MODEL_FILENAME)
}

/// Path to the silero-vad ONNX model used by sherpa-onnx's VAD.
pub fn silero_vad_path() -> Option<PathBuf> {
    models_dir().map(|d| d.join(SILERO_VAD_FILENAME))
}

/// Download URL for the silero-vad model.
pub fn silero_vad_download_url() -> &'static str {
    SILERO_VAD_URL
}

/// Path to the pyannote segmentation model (offline diarization).
pub fn pyannote_segmentation_path() -> Option<PathBuf> {
    models_dir().map(|d| d.join(PYANNOTE_SEG_FILENAME))
}

/// Download URL for the pyannote segmentation model.
pub fn pyannote_segmentation_download_url() -> &'static str {
    PYANNOTE_SEG_URL
}

/// True when the model file exists on disk and is non-empty.
pub fn model_is_ready(path: &Path) -> bool {
    path.metadata().map(|m| m.len() > 0).unwrap_or(false)
}
