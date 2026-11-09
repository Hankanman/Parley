//! End-to-end Parakeet check using public speech and isolated XDG directories.
use anyhow::{Result, ensure};
use parley_core::{
    audio::{
        self,
        transcription::{self, engine::TranscriptionEngine},
    },
    database::{manager::DatabaseManager, repositories::setting::SettingsRepository},
    events::{EventSink, SharedEventSink},
    parakeet,
};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
#[derive(Default)]
struct Sink {
    events: Mutex<Vec<(String, serde_json::Value)>>,
    cancel_download: AtomicBool,
    model: String,
}
impl EventSink for Sink {
    fn emit_value(&self, name: &str, value: serde_json::Value) -> Result<(), String> {
        self.events.lock().unwrap().push((name.into(), value));
        if name == "model-download-progress" && self.cancel_download.swap(false, Ordering::SeqCst) {
            let model = self.model.clone();
            tokio::spawn(async move {
                parakeet::cancel(&model).await.unwrap();
            });
        }
        Ok(())
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    let data = PathBuf::from(std::env::var("XDG_DATA_HOME")?);
    ensure!(
        data.to_string_lossy().contains("/target/parakeet-e2e/"),
        "Use isolated target/parakeet-e2e XDG directories"
    );
    ensure!(
        audio::recording_preferences::get_default_recordings_folder()
            .starts_with(data.parent().unwrap()),
        "Recordings folder must be isolated"
    );
    env_logger::init();
    parley_core::bootstrap::init_paths();
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        (2..=3).contains(&args.len()),
        "usage: parakeet_pipeline_check <public-speech.wav> [model-id]"
    );
    let model = args
        .get(2)
        .map(String::as_str)
        .unwrap_or(parakeet::CPU_MODEL);
    ensure!(parakeet::is_model(model), "Unknown model");
    let sink = Arc::new(Sink {
        model: model.into(),
        ..Default::default()
    });
    let events: SharedEventSink = sink.clone();
    sink.cancel_download.store(true, Ordering::SeqCst);
    let cancelled = parakeet::download(events.clone(), model.into()).await;
    ensure!(cancelled.is_err(), "Download did not cancel");
    ensure!(
        parakeet::validate(model).is_err(),
        "Incomplete download published"
    );
    parakeet::download(events.clone(), model.into())
        .await
        .map_err(anyhow::Error::msg)?;
    ensure!(
        sink.events
            .lock()
            .unwrap()
            .iter()
            .any(|(n, _)| n == "model-download-complete"),
        "Missing completion event"
    );
    println!("download/cancel/retry/hash verification: OK");
    let db = DatabaseManager::new_default().await?;
    let pool = db.pool().clone();
    SettingsRepository::save_transcript_config(&pool, parakeet::PROVIDER, model).await?;
    ensure!(
        SettingsRepository::save_transcript_config(&pool, parakeet::PROVIDER, "unknown")
            .await
            .is_err(),
        "Accepted unknown model"
    );
    let engine = transcription::engine::get_or_init_transcription_engine(Some(&pool))
        .await
        .map_err(anyhow::Error::msg)?;
    let weak = match &engine {
        TranscriptionEngine::Parakeet(e) => Arc::downgrade(e),
        _ => unreachable!(),
    };
    if std::env::var("PARLEY_PARAKEET_DEVICE").as_deref() == Ok("cuda")
        && model != parakeet::CPU_MODEL
    {
        ensure!(
            weak.upgrade().unwrap().device == "CUDA",
            "GPU validation fell back to CPU"
        );
        let out = std::process::Command::new("nvidia-smi")
            .args([
                "--query-compute-apps=pid,used_gpu_memory",
                "--format=csv,noheader,nounits",
            ])
            .output()?;
        println!(
            "GPU allocations while model is loaded: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
    let reused = parakeet::acquire(model).await.map_err(anyhow::Error::msg)?;
    ensure!(
        Arc::ptr_eq(&reused, &weak.upgrade().unwrap()),
        "Recognizer not shared"
    );
    ensure!(
        parakeet::delete(model).await.is_err(),
        "Deleted active model"
    );
    ensure!(
        parakeet::download(events.clone(), model.into())
            .await
            .is_err(),
        "Downloaded over active model"
    );
    ensure!(
        SettingsRepository::save_transcript_config(&pool, "localWhisper", "small")
            .await
            .is_err(),
        "Switched active provider"
    );
    let pcm =
        audio::decoder::decode_audio_file(std::path::Path::new(&args[1]))?.to_whisper_format();
    let start = std::time::Instant::now();
    let (text, confidence, _) = engine
        .transcribe(pcm.clone(), None, None, false)
        .await
        .map_err(anyhow::Error::msg)?;
    ensure!(
        !text.is_empty() && confidence.is_none(),
        "Invalid final result"
    );
    println!(
        "final: {text} ({:.2}s for {:.2}s audio)",
        start.elapsed().as_secs_f64(),
        pcm.len() as f64 / 16000.0
    );
    let (preview, confidence, partial) = engine
        .transcribe(pcm[..pcm.len().min(6 * 16000)].to_vec(), None, None, true)
        .await
        .map_err(anyhow::Error::msg)?;
    ensure!(
        !preview.is_empty() && confidence.is_none() && partial,
        "Invalid preview"
    );
    ensure!(
        engine
            .transcribe(vec![f32::NAN], None, None, false)
            .await
            .is_err(),
        "Accepted NaN"
    );
    let mut long = Vec::new();
    for _ in 0..4 {
        long.extend_from_slice(&pcm);
    }
    let (text, _, _) = engine
        .transcribe(long, None, None, false)
        .await
        .map_err(anyhow::Error::msg)?;
    ensure!(!text.is_empty(), "Long input lost");
    drop(reused);
    drop(engine);
    ensure!(
        weak.upgrade().is_none() && !parakeet::is_busy(),
        "Recognizer leaked"
    );
    println!("preview/long input/reuse/busy guard/release: OK");
    let cancel_engine = parakeet::acquire(model).await.map_err(anyhow::Error::msg)?;
    let cancel_weak = Arc::downgrade(&cancel_engine);
    let cancel_pcm = pcm.repeat(4);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _ = started_tx.send(());
        cancel_engine.transcribe(cancel_pcm, false).await
    });
    started_rx.await?;
    task.abort();
    let _ = task.await;
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while cancel_weak.upgrade().is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await?;
    ensure!(!parakeet::is_busy(), "Cancelled inference leaked model");
    println!("cancelled async inference releases model after native decoding: OK");

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let final_task =
        transcription::start_transcription_task(events.clone(), Some(pool.clone()), rx);
    let (ptx, prx) = tokio::sync::mpsc::unbounded_channel();
    transcription::start_partial_decode_task(events.clone(), Some(pool.clone()), prx);
    ptx.send(audio::recording_state::PartialAudioChunk {
        samples: pcm[..pcm.len().min(6 * 16000)].to_vec(),
        source: audio::recording_state::DeviceType::Microphone,
        utterance_id: 1,
    })?;
    tx.send(audio::recording_state::AudioChunk {
        data: pcm,
        sample_rate: 16000,
        timestamp: 0.,
        chunk_id: 1,
        device_type: audio::recording_state::DeviceType::Microphone,
    })?;
    drop(tx);
    drop(ptx);
    final_task.await?;
    if let Some(task) = transcription::take_partial_task_handle() {
        task.await?;
    }
    ensure!(
        sink.events
            .lock()
            .unwrap()
            .iter()
            .any(|(n, v)| n == "transcript-update"
                && v["text"].as_str().is_some_and(|s| !s.is_empty())
                && v["confidence"].is_null()),
        "No live transcript"
    );
    ensure!(!parakeet::is_busy(), "Live tasks leaked model");
    println!("live final/preview tasks: OK");
    struct CancelImport;
    impl EventSink for CancelImport {
        fn emit_value(&self, n: &str, _: serde_json::Value) -> Result<(), String> {
            if n == "import-progress" {
                audio::import::cancel_import();
            }
            Ok(())
        }
    }
    let cancelled = audio::import::start_import_with(
        Arc::new(CancelImport),
        Some(pool.clone()),
        args[1].clone(),
        "Parakeet cancellation check".into(),
        None,
        None,
        None,
        0,
    )
    .await;
    ensure!(
        cancelled.is_err() && !audio::import::is_import_in_progress() && !parakeet::is_busy(),
        "Import cancellation leaked state"
    );
    let imported = audio::import::start_import_with(
        events.clone(),
        Some(pool.clone()),
        args[1].clone(),
        "Parakeet import check".into(),
        None,
        None,
        None,
        0,
    )
    .await?;
    ensure!(
        imported.segments_count > 0 && !parakeet::is_busy(),
        "Import failed/leaked model"
    );
    let row: (String,) = sqlx::query_as("SELECT folder_path FROM meetings WHERE id = ?")
        .bind(&imported.meeting_id)
        .fetch_one(&pool)
        .await?;
    let redo = audio::retranscription::start_retranscription_with(
        events,
        Some(pool.clone()),
        imported.meeting_id,
        row.0,
        None,
        Some(model.into()),
        Some(parakeet::PROVIDER.into()),
    )
    .await?;
    ensure!(
        redo.segments_count > 0 && !parakeet::is_busy(),
        "Retranscription failed/leaked model"
    );
    println!("import/cancellation/retranscription: OK");
    SettingsRepository::save_transcript_config(&pool, "localWhisper", "small").await?;
    ensure!(
        transcription::engine::resolve_selection(Some(&pool), None, None)
            .await
            .map_err(anyhow::Error::msg)?
            .0
            == "localWhisper",
        "Cannot switch back to Whisper"
    );
    // Retain a verified fixture for an optional local installation after this
    // check, while still exercising removal from the app's model catalog.
    let fixture = data.parent().unwrap().join(format!("verified-{model}"));
    std::fs::create_dir_all(&fixture)?;
    for entry in std::fs::read_dir(
        parakeet::models_dir()
            .map_err(anyhow::Error::msg)?
            .join(model),
    )? {
        let entry = entry?;
        std::fs::copy(entry.path(), fixture.join(entry.file_name()))?;
    }
    parakeet::delete(model).await.map_err(anyhow::Error::msg)?;
    ensure!(parakeet::validate(model).is_err(), "Model deletion failed");
    println!("switch back/delete: OK");
    db.cleanup().await?;
    Ok(())
}
