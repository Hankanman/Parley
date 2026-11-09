//! Compare native inference and mel features with independently exported
//! official Python fixtures. Run explicitly; no downloads occur in tests.
use anyhow::{Context, Result};
use std::path::Path;
fn floats(path: &Path) -> Result<Vec<f32>> {
    let bytes = std::fs::read(path)?;
    anyhow::ensure!(bytes.len() % 4 == 0, "Invalid fixture");
    Ok(bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect())
}
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    anyhow::ensure!(
        args.len() >= 4,
        "usage: validate <runtime.so> <bundles> <cpu|cuda> [model]"
    );
    gigaam_core::init_runtime(Path::new(&args[1]))?;
    for name in gigaam_core::MODELS {
        if args.get(4).is_some_and(|m| m != name) {
            continue;
        }
        let dir = Path::new(&args[2]).join(name);
        let manifest = gigaam_core::Manifest::read(&dir)?;
        let pcm = floats(&dir.join("reference.pcm"))?;
        let expected = floats(&dir.join("reference.features"))?;
        let (features, _) = gigaam_core::features(&manifest, &pcm)?;
        anyhow::ensure!(features.len() == expected.len(), "Feature shape mismatch");
        let max_error = features
            .iter()
            .zip(&expected)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        let rms = (features
            .iter()
            .zip(&expected)
            .map(|(a, b)| (a - b).powi(2))
            .sum::<f32>()
            / features.len() as f32)
            .sqrt();
        anyhow::ensure!(
            max_error < 0.02 && rms < 0.001,
            "{name}: preprocessing drift max={max_error} rms={rms}"
        );
        let expected: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("reference.json"))?)?;
        let mut model = gigaam_core::Model::load(&dir, args[3] == "cuda")
            .with_context(|| format!("Loading {name}"))?;
        let started = std::time::Instant::now();
        let result = model.transcribe(&pcm)?;
        println!(
            "{name}: device={} seconds={:.3} rtf={:.3} mel_max={max_error:.6} mel_rms={rms:.6}\n{}",
            model.device,
            started.elapsed().as_secs_f64(),
            started.elapsed().as_secs_f64() / (pcm.len() as f64 / 16000.0),
            result.text
        );
        anyhow::ensure!(
            result.text == expected["text"].as_str().unwrap_or_default(),
            "{name}: text differs: expected {}",
            expected["text"]
        );
        anyhow::ensure!(
            result.words.windows(2).all(|w| w[0].start <= w[1].start),
            "Non-monotonic word times"
        );
        anyhow::ensure!(
            result
                .words
                .iter()
                .all(|w| w.start >= 0.0 && w.end >= w.start && w.end <= pcm.len() as f64 / 16000.0),
            "Invalid word times"
        );
        // Explicitly destroy all sessions between models (also exercised by
        // Parley's weak registry when the last operation finishes).
        drop(model);
    }
    Ok(())
}
