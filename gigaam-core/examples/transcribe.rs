use anyhow::Result;
use std::path::Path;
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    anyhow::ensure!(
        args.len() == 5,
        "usage: transcribe <runtime.so> <bundle> <f32le-pcm> <cpu|cuda>"
    );
    gigaam_core::init_runtime(Path::new(&args[1]))?;
    let mut model = gigaam_core::Model::load(Path::new(&args[2]), args[4] == "cuda")?;
    let bytes = std::fs::read(&args[3])?;
    let pcm: Vec<_> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    let t = std::time::Instant::now();
    let result = model.transcribe(&pcm)?;
    eprintln!(
        "device={} decode_ms={} duration_ms={}",
        model.device,
        t.elapsed().as_millis(),
        pcm.len() / 16
    );
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}
