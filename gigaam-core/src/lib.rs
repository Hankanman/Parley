//! In-process GigaAM v3 inference. Exported bundles include the exact training
//! window/filter bank, avoiding a second, subtly different mel implementation.
use anyhow::{bail, Context, Result};
use ort::{execution_providers::CUDAExecutionProvider, session::Session, value::Tensor};
use rustfft::{num_complex::Complex, FftPlanner};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const MODELS: [&str; 4] = ["v3_e2e_rnnt", "v3_e2e_ctc", "v3_rnnt", "v3_ctc"];
pub const MAX_SAMPLES: usize = 24 * 16_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FileInfo {
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Manifest {
    pub version: u32,
    pub name: String,
    pub encoder: String,
    pub decoder: Option<String>,
    pub joint: Option<String>,
    pub tokenizer: Option<String>,
    pub vocab: Vec<String>,
    pub hidden: usize,
    pub layers: usize,
    pub max_symbols: usize,
    pub n_fft: usize,
    pub hop_length: usize,
    pub center: bool,
    pub window: Vec<f32>,
    /// Frequency-major, as torchaudio.transforms.MelScale.fb.
    pub mel_filters: Vec<Vec<f32>>,
    pub files: BTreeMap<String, FileInfo>,
}

impl Manifest {
    pub fn read(dir: &Path) -> Result<Self> {
        let m: Self = serde_json::from_slice(&std::fs::read(dir.join("manifest.json"))?)?;
        if m.version != 1 || !MODELS.contains(&m.name.as_str()) {
            bail!("Unsupported GigaAM bundle");
        }
        if m.n_fft < 2
            || m.hop_length == 0
            || m.window.len() != m.n_fft
            || m.mel_filters.len() != m.n_fft / 2 + 1
            || m.vocab.is_empty()
            || m.mel_filters
                .iter()
                .any(|r| r.len() != m.mel_filters[0].len())
            || m.mel_filters[0].is_empty()
        {
            bail!("Invalid GigaAM feature configuration");
        }
        for file in [
            &m.encoder,
            m.decoder.as_ref().unwrap_or(&m.encoder),
            m.joint.as_ref().unwrap_or(&m.encoder),
            m.tokenizer.as_ref().unwrap_or(&m.encoder),
        ] {
            if !m.files.contains_key(file) {
                bail!("Missing bundle file metadata: {file}");
            }
        }
        if m.name.contains("rnnt")
            && (m.decoder.is_none()
                || m.joint.is_none()
                || m.hidden == 0
                || m.layers == 0
                || !(1..=10).contains(&m.max_symbols))
        {
            bail!("Invalid GigaAM RNNT configuration");
        }
        for (name, info) in &m.files {
            if Path::new(name).components().count() != 1 || name.starts_with('.') {
                bail!("Invalid bundle filename");
            }
            if std::fs::metadata(dir.join(name))
                .with_context(|| format!("Missing GigaAM file: {name}"))?
                .len()
                != info.bytes
            {
                bail!("Incomplete GigaAM file: {name}");
            }
        }
        Ok(m)
    }
    pub fn verify(&self, dir: &Path) -> Result<()> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        for (name, info) in &self.files {
            let mut hash = Sha256::new();
            let mut f = std::fs::File::open(dir.join(name))?;
            let mut buffer = vec![0; 1024 * 1024];
            loop {
                let n = f.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
            if format!("{:x}", hash.finalize()) != info.sha256 {
                bail!("Corrupted GigaAM file: {name}");
            }
        }
        Ok(())
    }
}

/// Matches torch.stft(center=true, pad_mode=reflect, normalized=false,
/// onesided=true) -> power -> exported HTK mel filters -> log(clamp(1e-9)).
pub fn features(m: &Manifest, pcm: &[f32]) -> Result<(Vec<f32>, usize)> {
    if pcm.len() < m.n_fft || pcm.len() > MAX_SAMPLES || pcm.iter().any(|x| !x.is_finite()) {
        bail!("GigaAM expects finite 16 kHz mono audio between 20 ms and 24 seconds");
    }
    let frames = if m.center {
        pcm.len() / m.hop_length + 1
    } else {
        (pcm.len() - m.n_fft) / m.hop_length + 1
    };
    let bins = m.n_fft / 2 + 1;
    let n_mels = m.mel_filters[0].len();
    let mut result = vec![0.0; n_mels * frames];
    let mut frame = vec![Complex::new(0.0, 0.0); m.n_fft];
    let fft = FftPlanner::<f32>::new().plan_fft_forward(m.n_fft);
    for t in 0..frames {
        for (i, value) in frame.iter_mut().enumerate() {
            let pos =
                (t * m.hop_length + i) as isize - if m.center { (m.n_fft / 2) as isize } else { 0 };
            let reflected = if pos < 0 {
                -pos
            } else if pos >= pcm.len() as isize {
                2 * pcm.len() as isize - pos - 2
            } else {
                pos
            } as usize;
            *value = Complex::new(pcm[reflected] * m.window[i], 0.0);
        }
        fft.process(&mut frame);
        for mel in 0..n_mels {
            let power: f32 = (0..bins)
                .map(|k| frame[k].norm_sqr() * m.mel_filters[k][mel])
                .sum();
            result[mel * frames + t] = power.clamp(1e-9, 1e9).ln();
        }
    }
    Ok((result, frames))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub start: f64,
    pub end: f64,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Transcript {
    pub text: String,
    pub words: Vec<Word>,
}

pub struct Model {
    pub manifest: Manifest,
    pub device: &'static str,
    encoder: Session,
    decoder: Option<Session>,
    joint: Option<Session>,
    tokenizer: Option<sentencepiece_rs::SentencePieceProcessor>,
}

fn session(path: PathBuf, cuda: bool) -> Result<Session> {
    let mut builder = Session::builder()?.with_intra_threads(4)?;
    if cuda {
        builder = builder.with_execution_providers([CUDAExecutionProvider::default()
            .with_memory_limit(4 * 1024 * 1024 * 1024)
            .with_conv_max_workspace(false)
            .with_tf32(false)
            .build()
            .error_on_failure()])?;
    }
    Ok(builder.commit_from_file(path)?)
}

impl Model {
    pub fn load(dir: &Path, cuda: bool) -> Result<Self> {
        let m = Manifest::read(dir)?;
        m.verify(dir)?;
        let encoder = session(dir.join(&m.encoder), cuda)?;
        // RNNT predictor/joint are small, serial kernels. Keep them on CPU to
        // avoid hundreds of host/device round-trips for every utterance.
        let decoder = m
            .decoder
            .as_ref()
            .map(|p| session(dir.join(p), false))
            .transpose()?;
        let joint = m
            .joint
            .as_ref()
            .map(|p| session(dir.join(p), false))
            .transpose()?;
        let tokenizer = m
            .tokenizer
            .as_ref()
            .map(|p| sentencepiece_rs::SentencePieceProcessor::open(dir.join(p)))
            .transpose()?;
        Ok(Self {
            manifest: m,
            device: if cuda { "CUDA" } else { "CPU" },
            encoder,
            decoder,
            joint,
            tokenizer,
        })
    }
    pub fn transcribe(&mut self, pcm: &[f32]) -> Result<Transcript> {
        let (input, frames) = features(&self.manifest, pcm)?;
        let n_mels = self.manifest.mel_filters[0].len();
        let result = self.encoder.run(ort::inputs![
            Tensor::from_array(([1, n_mels, frames], input))?,
            Tensor::from_array(([1], vec![frames as i64]))?
        ])?;
        let (shape, encoded) = result[0].try_extract_tensor::<f32>()?;
        let length = if let Ok((_, values)) = result[1].try_extract_tensor::<i64>() {
            values[0]
        } else {
            result[1].try_extract_tensor::<i32>()?.1[0] as i64
        };
        let valid = usize::try_from(length).context("Negative encoder length")?;
        let blank = self.manifest.vocab.len();
        let mut ids = Vec::new();
        let mut token_frames = Vec::new();
        if let (Some(decoder), Some(joint)) = (&mut self.decoder, &mut self.joint) {
            if shape.len() != 3 || shape[0] != 1 || valid > shape[2] as usize {
                bail!("Invalid RNNT encoder output");
            }
            let dim = shape[1] as usize;
            let steps = shape[2] as usize;
            let layers = self.manifest.layers;
            let hidden = self.manifest.hidden;
            let mut h = vec![0f32; layers * hidden];
            let mut c = h.clone();
            let mut label = blank as i64;
            let mut prediction: Option<(Vec<f32>, Vec<f32>, Vec<f32>)> = None;
            for t in 0..valid {
                let f: Vec<f32> = (0..dim).map(|i| encoded[i * steps + t]).collect();
                for _ in 0..self.manifest.max_symbols {
                    if prediction.is_none() {
                        let out = decoder.run(ort::inputs![
                            Tensor::from_array(([1, 1], vec![label]))?,
                            Tensor::from_array(([layers, 1, hidden], h.clone()))?,
                            Tensor::from_array(([layers, 1, hidden], c.clone()))?
                        ])?;
                        prediction = Some((
                            out[0].try_extract_tensor::<f32>()?.1.to_vec(),
                            out[1].try_extract_tensor::<f32>()?.1.to_vec(),
                            out[2].try_extract_tensor::<f32>()?.1.to_vec(),
                        ));
                    }
                    let (g, next_h, next_c) = prediction.as_ref().unwrap();
                    let out = joint.run(ort::inputs![
                        Tensor::from_array(([1, dim, 1], f.clone()))?,
                        Tensor::from_array(([1, hidden, 1], g.clone()))?
                    ])?;
                    let scores = out[0].try_extract_tensor::<f32>()?.1;
                    if scores.len() != blank + 1 {
                        bail!("Invalid RNNT vocabulary size");
                    }
                    let id = argmax(scores);
                    if id == blank {
                        break;
                    }
                    ids.push(id);
                    token_frames.push(t);
                    h = next_h.clone();
                    c = next_c.clone();
                    label = id as i64;
                    prediction = None;
                }
            }
        } else {
            if shape.len() != 3
                || shape[0] != 1
                || shape[2] as usize != blank + 1
                || valid > shape[1] as usize
            {
                bail!("Invalid CTC encoder output");
            }
            let mut previous = blank;
            for (t, scores) in encoded.chunks_exact(blank + 1).take(valid).enumerate() {
                let id = argmax(scores);
                if id != blank && id != previous {
                    ids.push(id);
                    token_frames.push(t);
                }
                previous = id;
            }
        }
        let text = if let Some(tokenizer) = &self.tokenizer {
            tokenizer.decode_ids(&ids)?
        } else {
            ids.iter()
                .map(|&i| self.manifest.vocab[i].as_str())
                .collect::<String>()
        };
        // Align word boundaries to emitted token frames. SentencePiece word
        // markers and character spaces are retained in the exported vocabulary.
        let seconds_per_frame = pcm.len() as f64 / 16000.0 / valid.max(1) as f64;
        let mut words: Vec<Word> = Vec::new();
        let mut current = String::new();
        let mut start = 0.0;
        let mut end = 0.0;
        for (&id, &frame) in ids.iter().zip(&token_frames) {
            let piece = &self.manifest.vocab[id];
            let boundary = if self.tokenizer.is_some() {
                piece.starts_with('▁')
            } else {
                piece.chars().all(char::is_whitespace)
            };
            if boundary && !current.is_empty() {
                words.push(Word {
                    text: std::mem::take(&mut current),
                    start,
                    end,
                });
            }
            let piece = piece.trim_start_matches('▁');
            if !piece.trim().is_empty() {
                if current.is_empty() {
                    start = frame as f64 * seconds_per_frame;
                }
                current.push_str(piece);
                end = ((frame + 1) as f64 * seconds_per_frame).min(pcm.len() as f64 / 16000.0);
            }
        }
        if !current.is_empty() {
            words.push(Word {
                text: current,
                start,
                end,
            });
        }
        Ok(Transcript {
            text: text.trim().to_string(),
            words,
        })
    }
}

fn argmax(values: &[f32]) -> usize {
    values
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1).then_with(|| b.0.cmp(&a.0)))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// ONNX Runtime is shared with sherpa-onnx. Always load the same packaged
/// library; a second independently-versioned runtime in this process is unsafe.
pub fn init_runtime(path: &Path) -> Result<()> {
    // sherpa may have loaded ORT from AppDir/usr/lib while the caller is
    // AppDir/usr/bin. Resolve the existing ELF symbol to its owning library
    // rather than opening a second copy from the executable's directory.
    let loaded_path = loaded_runtime_path();
    let path = loaded_path.as_deref().unwrap_or(path);
    ort::init_from(path.display().to_string())
        .with_name("parley-gigaam")
        .commit()?;
    Ok(())
}

#[cfg(unix)]
fn loaded_runtime_path() -> Option<PathBuf> {
    // SAFETY: dladdr only inspects a symbol address owned by an already
    // loaded library. The function is never called, and its path is copied
    // before the temporary dlopen handle is released.
    unsafe {
        let process = libloading::os::unix::Library::this();
        let symbol = process
            .get::<unsafe extern "C" fn() -> *const libc::c_void>(b"OrtGetApiBase\0")
            .ok()?;
        let mut info: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(*symbol as *const () as *const libc::c_void, &mut info) == 0
            || info.dli_fname.is_null()
        {
            return None;
        }
        Some(PathBuf::from(
            std::ffi::CStr::from_ptr(info.dli_fname)
                .to_string_lossy()
                .into_owned(),
        ))
    }
}
#[cfg(not(unix))]
fn loaded_runtime_path() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn argmax_uses_first_tie_like_torch() {
        assert_eq!(argmax(&[1., 2., 2.]), 1);
    }
    #[test]
    fn bundle_names_are_asr_only() {
        assert_eq!(MODELS.len(), 4);
        assert!(MODELS.iter().all(|m| !m.contains("ssl")));
    }
}
