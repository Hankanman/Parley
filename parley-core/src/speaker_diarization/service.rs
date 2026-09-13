//! Tauri-free core of the speaker diarization layer: model download,
//! diarizer construction/lifecycle, and post-recording refinement. See
//! `commands.rs` for the thin `#[tauri::command]` wrappers around these.

use crate::database::repositories::transcript::TranscriptsRepository;
use crate::database::repositories::voice_profile::{bytes_to_floats, VoiceProfilesRepository};
use crate::events::{EventSinkExt, SharedEventSink};
use crate::speaker_diarization::embedding_math::{average_and_normalize, merge_centroids};
use crate::speaker_diarization::{
    current_diarizer, default_model_path, model::model_is_ready, model_download_url,
    set_current_diarizer, Diarizer, SpeakerEmbedder, SpeakerProfileMatcher,
    DEFAULT_CLUSTER_THRESHOLD, PROFILE_MATCH_THRESHOLD,
};
use crate::speaker_diarization::model::{
    pyannote_segmentation_download_url, pyannote_segmentation_path,
};
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

/// Tauri-free core of the `speaker_model_download` command.
pub async fn download_speaker_model(sink: SharedEventSink) -> Result<(), String> {
    let path = default_model_path()
        .ok_or_else(|| "Speaker models directory not initialized".to_string())?;

    if model_is_ready(&path) {
        let _ = sink.emit_event(
            "speaker-model-download-complete",
            &serde_json::json!({ "alreadyPresent": true }),
        );
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(format!("Failed to create speaker models dir: {}", e));
        }
    }

    let url = model_download_url();
    log::info!("Downloading speaker model from {} -> {}", url, path.display());

    if let Err(e) = stream_download(&sink, &url, &path).await {
        let _ = std::fs::remove_file(&path); // partial-file cleanup
        let msg = e.to_string();
        let _ = sink.emit_event(
            "speaker-model-download-error",
            &serde_json::json!({ "error": &msg }),
        );
        return Err(msg);
    }

    let _ = sink.emit_event(
        "speaker-model-download-complete",
        &serde_json::json!({ "alreadyPresent": false }),
    );
    Ok(())
}

/// Tauri-free core of the `ensure_pyannote_segmentation_model` command.
pub async fn ensure_pyannote_segmentation_model() -> Result<String, String> {
    let path = pyannote_segmentation_path()
        .ok_or_else(|| "Speaker models directory not initialized".to_string())?;

    if model_is_ready(&path) {
        return Ok(path.to_string_lossy().into_owned());
    }

    let url = pyannote_segmentation_download_url();
    log::info!(
        "Pyannote segmentation model missing — fetching {} -> {}",
        url,
        path.display()
    );
    crate::utils::download_file_to(url, &path)
        .await
        .map_err(|e| e.to_string())?;

    Ok(path.to_string_lossy().into_owned())
}

/// Build a [`Diarizer`] from the on-disk model + stored voice profiles.
/// Returns `None` if the speaker model isn't downloaded — callers should
/// degrade gracefully (no `speaker` label rather than failing).
///
/// Used by both the live recording path (via [`try_init_for_recording`])
/// and batch jobs (import / retranscription) that want their own
/// short-lived diarizer instance with fresh cluster IDs.
pub async fn build_diarizer(pool: Option<&SqlitePool>) -> Result<Option<Arc<Diarizer>>> {
    let Some(path) = default_model_path() else {
        log::warn!("Speaker models dir not configured; skipping diarizer build");
        return Ok(None);
    };
    if !model_is_ready(&path) {
        log::info!(
            "Speaker model not present at {}; speaker labels will be empty",
            path.display()
        );
        return Ok(None);
    }

    let embedder = SpeakerEmbedder::from_path(&path, 1)?;
    let dim = embedder.dim();

    // Load all stored voice profiles whose embedding dim matches the model.
    // A mismatched-dim profile (e.g., from an older model) is skipped by the
    // matcher rather than failing the build.
    let matcher = match build_profile_matcher(pool, dim).await {
        Ok(m) => m,
        Err(e) => {
            log::warn!("Failed to load voice profiles: {} (continuing without)", e);
            None
        }
    };

    let diarizer = Diarizer::new(Arc::new(embedder), DEFAULT_CLUSTER_THRESHOLD, matcher);
    Ok(Some(Arc::new(diarizer)))
}

/// Build a diarizer and install it into the process-wide slot for the live
/// recording path. Silent no-op (returns `Ok(false)`) if the model isn't on
/// disk — recording proceeds with the "Speaker" placeholder.
pub async fn try_init_for_recording(pool: Option<&SqlitePool>) -> Result<bool> {
    match build_diarizer(pool).await? {
        Some(diarizer) => {
            set_current_diarizer(Some(diarizer));
            log::info!("Speaker diarizer initialized for recording session");
            Ok(true)
        }
        None => {
            set_current_diarizer(None);
            Ok(false)
        }
    }
}

/// At recording stop we *don't* clear the diarizer — its embedding history
/// is still needed for `promote_speaker_to_profile` and 2-pass refinement.
/// The next `try_init_for_recording` call replaces it with a fresh instance.
pub fn shutdown_for_recording() {
    log::info!("Speaker diarizer retained post-stop for promote / refine actions");
}

async fn build_profile_matcher(
    pool: Option<&SqlitePool>,
    dim: usize,
) -> Result<Option<Arc<SpeakerProfileMatcher>>> {
    let pool = pool.ok_or_else(|| anyhow!("DB pool unavailable; cannot load voice profiles"))?;

    let profiles = VoiceProfilesRepository::list_all(pool)
        .await
        .map_err(|e| anyhow!("DB error listing voice profiles: {}", e))?;

    if profiles.is_empty() {
        return Ok(None);
    }

    let entries = profiles.into_iter().filter_map(|p| {
        bytes_to_floats(&p.embedding).map(|emb| (p.id, p.name, emb))
    });

    let matcher = SpeakerProfileMatcher::new(dim, entries, PROFILE_MATCH_THRESHOLD)?;
    if matcher.num_profiles() == 0 {
        Ok(None)
    } else {
        Ok(Some(Arc::new(matcher)))
    }
}

async fn stream_download(
    sink: &SharedEventSink,
    url: &str,
    dest: &std::path::Path,
) -> Result<()> {
    let response = reqwest::get(url)
        .await
        .map_err(|e| anyhow!("HTTP error: {}", e))?;
    if !response.status().is_success() {
        return Err(anyhow!("HTTP {} fetching {}", response.status(), url));
    }

    let total = response.content_length().unwrap_or(0);
    let mut downloaded: u64 = 0;
    let mut last_pct: u8 = 0;

    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| anyhow!("Cannot create {}: {}", dest.display(), e))?;

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow!("Download stream error: {}", e))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| anyhow!("Write error: {}", e))?;
        downloaded += chunk.len() as u64;

        if total > 0 {
            let pct = ((downloaded as u128 * 100) / total as u128) as u8;
            // Emit only on 1% steps to avoid event flood for a 28MB download.
            if pct != last_pct {
                last_pct = pct;
                let _ = sink.emit_event(
                    "speaker-model-download-progress",
                    &serde_json::json!({ "progress": pct }),
                );
            }
        }
    }

    file.flush()
        .await
        .map_err(|e| anyhow!("Flush error: {}", e))?;
    Ok(())
}

/// Re-cluster the just-finished recording offline and write the improved
/// speaker labels into `meeting_id`'s saved transcripts.
///
/// Called right after the meeting is saved (see
/// `audio::recording_commands::trigger_post_meeting_refine`). Emits
/// `speakers-refined` (`{ meeting_id, changed_count }`) on success so the
/// meeting-details view can reload the transcript.
///
/// Every skip path is a no-op rather than an error to the user: this runs
/// unattended and the live labels are already saved and usable, so the worst
/// realistic outcome is "labels stay as they were".
///
/// ## Diarizer lifetime
///
/// This depends on [`current_diarizer`] still holding the diarizer that
/// produced the meeting. That holds at the call site:
/// `stop_recording` deliberately does *not* clear the slot (see
/// [`shutdown_for_recording`]) precisely so promote/refine can still reach
/// the history, and only the next `try_init_for_recording` replaces it. If
/// the slot is empty anyway (speaker model never downloaded, so no diarizer
/// was ever built) we log and skip.
pub async fn refine_and_persist(
    sink: &SharedEventSink,
    pool: &SqlitePool,
    meeting_id: &str,
) -> Result<usize> {
    let Some(diarizer) = current_diarizer() else {
        log::info!(
            "No diarizer available for meeting {} — skipping speaker refinement",
            meeting_id
        );
        return Ok(0);
    };

    // Clustering is pure CPU work over the whole session's embeddings; keep
    // it off the async runtime so it can't stall other tasks (same reasoning
    // as whisper/embedding inference elsewhere).
    let refined = {
        let diarizer = diarizer.clone();
        tokio::task::spawn_blocking(move || diarizer.refine())
            .await
            .map_err(|e| anyhow!("Speaker refinement task panicked: {}", e))?
    };
    if refined.is_empty() {
        log::info!(
            "Diarizer history empty for meeting {} — nothing to refine",
            meeting_id
        );
        return Ok(0);
    }

    let updates: Vec<(u64, String, String, Option<String>)> = refined
        .iter()
        .filter(|r| r.changed)
        .map(|r| {
            (
                r.sequence_id,
                r.previous_speaker.clone(),
                r.speaker.clone(),
                r.voice_profile_id.clone(),
            )
        })
        .collect();

    if updates.is_empty() {
        log::info!(
            "Speaker refinement for meeting {}: {} segments, live labels already optimal",
            meeting_id,
            refined.len()
        );
        return Ok(0);
    }

    let changed_count =
        TranscriptsRepository::update_speakers_by_sequence(pool, meeting_id, &updates)
            .await
            .map_err(|e| anyhow!("DB error applying speaker refinement: {}", e))?;

    // Keep the in-memory history's labels in sync with what the DB (and so
    // the UI) now shows, so a later promote/merge — which looks embeddings up
    // *by label* — resolves the refined labels the user actually sees.
    diarizer.apply_refinement(&refined);

    log::info!(
        "Speaker refinement for meeting {}: {} segments re-clustered, {} rows relabeled",
        meeting_id,
        refined.len(),
        changed_count
    );

    let _ = sink.emit_event(
        "speakers-refined",
        &serde_json::json!({
            "meeting_id": meeting_id,
            "changed_count": changed_count,
        }),
    );

    Ok(changed_count as usize)
}

/// Shared core of the `promote_speaker_to_profile` command: pull the
/// diarizer's embeddings for `old_label`, average+save as a voice profile,
/// and rewrite the label across `meeting_id`'s transcripts. Returns
/// `(profile_id, renamed_count)`.
pub async fn promote_speaker_to_profile_core(
    pool: &SqlitePool,
    old_label: &str,
    trimmed_name: &str,
    normalised_email: Option<&str>,
    meeting_id: &str,
) -> Result<(Option<String>, u64), String> {
    // Try to grab the embeddings. They're reachable only when the diarizer
    // that produced this meeting is still the in-memory singleton (a live
    // recording or its just-finished retranscription). Older meetings
    // degrade to rename-only.
    let embeddings = current_diarizer()
        .map(|d| d.embeddings_for_label(old_label))
        .unwrap_or_default();

    let profile_id = if embeddings.is_empty() {
        log::warn!(
            "No embeddings reachable for {} in meeting {} — renaming transcripts but skipping voice-profile creation",
            old_label,
            meeting_id,
        );
        None
    } else {
        let centroid = average_and_normalize(&embeddings);
        let id = VoiceProfilesRepository::create(
            pool,
            trimmed_name,
            normalised_email,
            &centroid,
            embeddings.len() as i64,
        )
        .await
        .map_err(|e| format!("Failed to create voice profile: {}", e))?;
        log::info!(
            "Promoted {} to profile '{}' (email={:?}, id={}, samples={})",
            old_label,
            trimmed_name,
            normalised_email,
            id,
            embeddings.len()
        );
        Some(id)
    };

    // Rewrite the displayed speaker label in this meeting's transcripts so
    // every row that previously showed "Speaker N" now shows the new name.
    // This runs in both the success and the rename-only fallback case.
    let renamed_count = TranscriptsRepository::rename_speaker_in_meeting(
        pool,
        meeting_id,
        old_label,
        trimmed_name,
        profile_id.as_deref(),
    )
    .await
    .map_err(|e| format!("Failed to rename transcripts: {}", e))?;

    log::info!(
        "Renamed {} -> '{}' across {} transcript rows in meeting {}",
        old_label,
        trimmed_name,
        renamed_count,
        meeting_id,
    );

    Ok((profile_id, renamed_count))
}

/// Shared core of the `merge_voice_profiles` command: fold `loser_id`'s
/// centroid into `winner_id`'s (weighted by sample count), relink every
/// transcript referencing the loser, then delete the loser. Returns
/// `(renamed_count, centroid_updated)`.
pub async fn merge_voice_profiles_core(
    pool: &SqlitePool,
    winner_id: &str,
    loser_id: &str,
) -> Result<(u64, bool), String> {
    let winner = VoiceProfilesRepository::get_by_id(pool, winner_id)
        .await
        .map_err(|e| format!("Failed to load winner profile: {}", e))?
        .ok_or_else(|| format!("Profile not found: {}", winner_id))?;
    let loser = VoiceProfilesRepository::get_by_id(pool, loser_id)
        .await
        .map_err(|e| format!("Failed to load loser profile: {}", e))?
        .ok_or_else(|| format!("Profile not found: {}", loser_id))?;

    let centroid_updated = if winner.embedding_dim == loser.embedding_dim {
        let winner_centroid = bytes_to_floats(&winner.embedding)
            .ok_or_else(|| "Winner profile has corrupt embedding".to_string())?;
        let loser_centroid = bytes_to_floats(&loser.embedding)
            .ok_or_else(|| "Loser profile has corrupt embedding".to_string())?;
        let merged = merge_centroids(
            &winner_centroid,
            winner.sample_count.max(0) as usize,
            &loser_centroid,
            loser.sample_count.max(0) as usize,
        );
        let new_count = winner.sample_count + loser.sample_count;
        VoiceProfilesRepository::update_centroid(pool, &winner.id, &merged, new_count)
            .await
            .map_err(|e| format!("Failed to update winner centroid: {}", e))?;
        true
    } else {
        log::warn!(
            "Embedding-dim mismatch ({} vs {}) merging {} into {} — relinking transcripts only",
            winner.embedding_dim,
            loser.embedding_dim,
            loser.id,
            winner.id,
        );
        false
    };

    let renamed_count =
        TranscriptsRepository::relink_transcripts(pool, &loser.id, &winner.id, &winner.name)
            .await
            .map_err(|e| format!("Failed to relink transcripts: {}", e))?;

    VoiceProfilesRepository::delete(pool, &loser.id)
        .await
        .map_err(|e| format!("Failed to delete loser profile: {}", e))?;

    log::info!(
        "Merged profile '{}' ({}) into '{}' ({}): {} transcripts relinked, centroid_updated={}",
        loser.name,
        loser.id,
        winner.name,
        winner.id,
        renamed_count,
        centroid_updated
    );

    Ok((renamed_count, centroid_updated))
}

/// Shared core of the `merge_cluster_into_profile` command: fold the
/// diarizer's embeddings for `old_label` (if reachable) into `profile_id`'s
/// centroid, then rewrite the label across `meeting_id`'s transcripts.
/// Returns `(renamed_count, centroid_updated)`.
pub async fn merge_cluster_into_profile_core(
    pool: &SqlitePool,
    meeting_id: &str,
    old_label: &str,
    profile_id: &str,
) -> Result<(u64, bool), String> {
    let profile = VoiceProfilesRepository::get_by_id(pool, profile_id)
        .await
        .map_err(|e| format!("Failed to load profile: {}", e))?
        .ok_or_else(|| format!("Profile not found: {}", profile_id))?;

    // Pull this label's embeddings if the diarizer is still reachable.
    let cluster_embeddings = current_diarizer()
        .map(|d| d.embeddings_for_label(old_label))
        .unwrap_or_default();

    let centroid_updated = if !cluster_embeddings.is_empty()
        && cluster_embeddings[0].len() as i64 == profile.embedding_dim
    {
        let cluster_centroid = average_and_normalize(&cluster_embeddings);
        let profile_centroid = bytes_to_floats(&profile.embedding)
            .ok_or_else(|| "Profile has corrupt embedding".to_string())?;
        let merged = merge_centroids(
            &profile_centroid,
            profile.sample_count.max(0) as usize,
            &cluster_centroid,
            cluster_embeddings.len(),
        );
        let new_count = profile.sample_count + cluster_embeddings.len() as i64;
        VoiceProfilesRepository::update_centroid(pool, &profile.id, &merged, new_count)
            .await
            .map_err(|e| format!("Failed to update profile centroid: {}", e))?;
        true
    } else {
        if !cluster_embeddings.is_empty() {
            log::warn!(
                "Embedding-dim mismatch for {} ({} vs profile {}) — skipping centroid update",
                old_label,
                cluster_embeddings[0].len(),
                profile.embedding_dim,
            );
        } else {
            log::info!(
                "No embeddings reachable for {} in meeting {} — relabel only",
                old_label,
                meeting_id,
            );
        }
        false
    };

    let renamed_count = TranscriptsRepository::rename_speaker_in_meeting(
        pool,
        meeting_id,
        old_label,
        &profile.name,
        Some(&profile.id),
    )
    .await
    .map_err(|e| format!("Failed to rename transcripts: {}", e))?;

    log::info!(
        "Merged {} (meeting {}) into profile '{}' ({}): {} transcripts relabeled, centroid_updated={}",
        old_label,
        meeting_id,
        profile.name,
        profile.id,
        renamed_count,
        centroid_updated
    );

    Ok((renamed_count, centroid_updated))
}
