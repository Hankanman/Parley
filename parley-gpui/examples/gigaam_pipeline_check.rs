//! Explicit end-to-end check using isolated XDG directories and public audio.
//! Refuses to run against the user's real app data.
use anyhow::{ensure, Result};
use parley_core::{
    audio::{
        self,
        transcription::{self, engine::TranscriptionEngine},
    },
    database::{manager::DatabaseManager, repositories::setting::SettingsRepository},
    events::{EventSink, SharedEventSink},
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Sink(Mutex<Vec<(String, serde_json::Value)>>);
impl EventSink for Sink {
    fn emit_value(&self, name: &str, value: serde_json::Value) -> Result<(), String> {
        self.0.lock().unwrap().push((name.to_string(), value));
        Ok(())
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    let data = PathBuf::from(std::env::var("XDG_DATA_HOME")?);
    ensure!(
        data.to_string_lossy().contains("/target/gigaam-e2e/"),
        "Use isolated target/gigaam-e2e XDG directories"
    );
    ensure!(
        audio::recording_preferences::get_default_recordings_folder()
            .starts_with(data.parent().unwrap()),
        "Test recordings folder must be isolated"
    );
    let args: Vec<String> = std::env::args().collect();
    ensure!(
        args.len() == 3,
        "usage: gigaam_pipeline_check <public-example.wav> <long-example.wav>"
    );
    env_logger::init();
    parley_core::bootstrap::init_paths();
    let db = DatabaseManager::new_default().await?;
    let pool = db.pool().clone();
    let sink = Arc::new(Sink::default());
    let events: SharedEventSink = sink.clone();
    let pcm_path = parley_core::gigaam::models_dir()
        .map_err(anyhow::Error::msg)?
        .join("v3_e2e_rnnt/reference.pcm");
    let bytes = std::fs::read(pcm_path)?;
    let pcm: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    for name in gigaam_models() {
        SettingsRepository::save_transcript_config(&pool, "localGigaAM", name).await?;
        let engine = transcription::engine::get_or_init_transcription_engine(Some(&pool))
            .await
            .map_err(anyhow::Error::msg)?;
        let weak = match &engine {
            TranscriptionEngine::GigaAm(e) => Arc::downgrade(e),
            _ => unreachable!(),
        };
        let reused = parley_core::gigaam::acquire(name)
            .await
            .map_err(anyhow::Error::msg)?;
        ensure!(
            Arc::ptr_eq(&weak.upgrade().unwrap(), &reused),
            "Did not reuse model"
        );
        let other = if name == "v3_ctc" {
            "v3_rnnt"
        } else {
            "v3_ctc"
        };
        ensure!(
            parley_core::gigaam::acquire(other).await.is_err(),
            "Model switched while in use"
        );
        ensure!(
            SettingsRepository::save_transcript_config(&pool, "localWhisper", "small")
                .await
                .is_err(),
            "Allowed active selection change"
        );
        let (preview, confidence, _) = engine
            .transcribe(pcm[..6 * 16000].to_vec(), None, None, true)
            .await
            .map_err(anyhow::Error::msg)?;
        let runtimes: std::collections::BTreeSet<_> = std::fs::read_to_string("/proc/self/maps")?
            .lines()
            .filter(|line| line.contains("/libonnxruntime.so"))
            .filter_map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                Some((fields.get(3)?.to_string(), fields.get(4)?.to_string()))
            })
            .collect();
        ensure!(
            runtimes.len() == 1,
            "Multiple ONNX Runtime libraries loaded: {runtimes:?}"
        );
        let gpu_loaded = gpu_memory_mb();
        if std::env::var("PARLEY_GIGAAM_DEVICE").as_deref() != Ok("cpu") {
            ensure!(
                reused.device == "CUDA",
                "GPU validation unexpectedly fell back to CPU"
            );
        }
        ensure!(
            !preview.is_empty() && confidence.is_none(),
            "Invalid GigaAM preview/confidence"
        );
        drop(reused);
        drop(engine);
        ensure!(
            weak.upgrade().is_none() && !parley_core::gigaam::is_busy(),
            "Model not released"
        );
        let gpu_released = gpu_memory_mb();
        if let (Some(loaded), Some(released)) = (gpu_loaded, gpu_released) {
            println!("GPU model lifecycle {name}: loaded={loaded}MiB after_drop={released}MiB");
            if std::env::var("PARLEY_GIGAAM_DEVICE").as_deref() != Ok("cpu") && loaded > 500 {
                ensure!(
                    released + 200 < loaded,
                    "Model GPU allocations did not release"
                );
            }
        }
        println!("lifetime/preview/busy guard: {name} OK");
    }
    SettingsRepository::save_transcript_config(&pool, "localGigaAM", "v3_e2e_rnnt").await?;
    // Exercise both real live tasks with PCM supplied directly, without
    // opening the user's microphone or system audio capture.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let final_task =
        transcription::start_transcription_task(events.clone(), Some(pool.clone()), rx);
    let (partial_tx, partial_rx) = tokio::sync::mpsc::unbounded_channel();
    transcription::start_partial_decode_task(events.clone(), Some(pool.clone()), partial_rx);
    partial_tx.send(audio::recording_state::PartialAudioChunk {
        samples: pcm[..6 * 16000].to_vec(),
        source: audio::recording_state::DeviceType::Microphone,
        utterance_id: 1,
    })?;
    tx.send(audio::recording_state::AudioChunk {
        data: pcm.clone(),
        sample_rate: 16000,
        timestamp: 0.,
        chunk_id: 1,
        device_type: audio::recording_state::DeviceType::Microphone,
    })?;
    drop(tx);
    drop(partial_tx);
    final_task.await?;
    if let Some(task) = transcription::take_partial_task_handle() {
        task.await?;
    }
    ensure!(
        sink.0
            .lock()
            .unwrap()
            .iter()
            .any(|(n, v)| n == "transcript-update"
                && v["text"].as_str().is_some_and(|s| !s.is_empty())
                && v["confidence"].is_null()),
        "No live GigaAM transcript"
    );
    ensure!(!parley_core::gigaam::is_busy(), "Live tasks leaked model");
    println!("live final/partial tasks and release: OK");

    ensure!(
        parley_core::gigaam::acquire("v3_ssl").await.is_err(),
        "Accepted unsupported model"
    );
    // Cancel a real import at its first progress event, then verify cleanup.
    struct CancelSink;
    impl EventSink for CancelSink {
        fn emit_value(&self, name: &str, _: serde_json::Value) -> Result<(), String> {
            if name == "import-progress" {
                audio::import::cancel_import();
            }
            Ok(())
        }
    }
    let cancelled = audio::import::start_import_with(
        Arc::new(CancelSink),
        Some(pool.clone()),
        args[1].clone(),
        "GigaAM cancel check".into(),
        None,
        Some("v3_ctc".into()),
        Some("localGigaAM".into()),
        0,
    )
    .await;
    ensure!(
        cancelled.is_err()
            && !audio::import::is_import_in_progress()
            && !parley_core::gigaam::is_busy(),
        "Cancellation leaked operation/model"
    );
    println!("cancel import / invalid model: OK");
    let mut first = None;
    for name in gigaam_models() {
        let result = audio::import::start_import_with(
            events.clone(),
            Some(pool.clone()),
            args[1].clone(),
            format!("GigaAM check {name}"),
            Some("ru".into()),
            Some(name.into()),
            Some("localGigaAM".into()),
            0,
        )
        .await?;
        ensure!(
            result.segments_count > 0,
            "Import produced no segments: {name}"
        );
        ensure!(
            !parley_core::gigaam::is_busy(),
            "Import retained model: {name}"
        );
        println!("import: {name} segments={} OK", result.segments_count);
        if first.is_none() {
            first = Some(result);
        }
    }
    let meeting = first.unwrap();
    let row: (String,) = sqlx::query_as("SELECT folder_path FROM meetings WHERE id = ?")
        .bind(&meeting.meeting_id)
        .fetch_one(&pool)
        .await?;
    let redo = audio::retranscription::start_retranscription_with(
        events.clone(),
        Some(pool.clone()),
        meeting.meeting_id,
        row.0,
        Some("ru".into()),
        Some("v3_ctc".into()),
        Some("localGigaAM".into()),
    )
    .await?;
    ensure!(
        redo.segments_count > 0 && !parley_core::gigaam::is_busy(),
        "Retranscription failed/released late"
    );
    println!("retranscription with changed provider model: OK");
    let long = audio::import::start_import_with(
        events.clone(),
        Some(pool.clone()),
        args[2].clone(),
        "GigaAM long check".into(),
        None,
        Some("v3_e2e_rnnt".into()),
        Some("localGigaAM".into()),
        0,
    )
    .await?;
    ensure!(
        long.segments_count > 0 && !parley_core::gigaam::is_busy(),
        "Long import failed"
    );
    println!("long import (>25 s): OK");
    SettingsRepository::save_transcript_config(&pool, "localWhisper", "large-v3-turbo").await?;
    ensure!(
        transcription::engine::resolve_selection(Some(&pool), None, None)
            .await
            .map_err(anyhow::Error::msg)?
            .0
            == "localWhisper",
        "Whisper selection did not persist"
    );
    let whisper = transcription::engine::get_or_init_transcription_engine(Some(&pool))
        .await
        .map_err(anyhow::Error::msg)?;
    ensure!(whisper.is_whisper(), "Whisper was not selected");
    let (text, confidence, _) = whisper
        .transcribe(pcm, Some("ru".into()), None, false)
        .await
        .map_err(anyhow::Error::msg)?;
    ensure!(
        !text.is_empty() && confidence.is_some(),
        "Whisper regression"
    );
    println!("switch back to Whisper and decode Russian speech: OK");
    db.cleanup().await?;
    Ok(())
}
fn gigaam_models() -> [&'static str; 4] {
    ["v3_e2e_rnnt", "v3_e2e_ctc", "v3_rnnt", "v3_ctc"]
}

fn gpu_memory_mb() -> Option<u64> {
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-compute-apps=pid,used_gpu_memory",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let pid = std::process::id().to_string();
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let (p, used) = line.split_once(',')?;
                if p.trim() == pid {
                    used.trim().parse::<u64>().ok()
                } else {
                    None
                }
            })
            .sum(),
    )
}
