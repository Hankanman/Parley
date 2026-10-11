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

    let self_profile_id = profiles.iter().find(|p| p.is_self).map(|p| p.id.clone());
    let entries = profiles.into_iter().filter_map(|p| {
        bytes_to_floats(&p.embedding).map(|emb| (p.id, p.name, emb))
    });

    let matcher = SpeakerProfileMatcher::new(dim, entries, PROFILE_MATCH_THRESHOLD)?
        .with_self_profile(self_profile_id);
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

/// How many people the meeting's linked calendar event says were invited —
/// organizer plus attendees, de-duplicated, minus anyone who declined — for
/// use as a soft cap on speaker refinement. `None` when there's no linked
/// event or it lists fewer than two people (a one-person event is just a
/// calendar block, not a head count).
pub async fn expected_speaker_count(pool: &SqlitePool, meeting_id: &str) -> Option<usize> {
    match crate::calendar::repository::CalendarRepository::get_event_for_meeting(pool, meeting_id)
        .await
    {
        Ok(Some(event)) => head_count(&event),
        Ok(None) => None,
        Err(e) => {
            log::warn!("Failed to load calendar event for meeting {}: {}", meeting_id, e);
            None
        }
    }
}

fn head_count(event: &crate::calendar::models::CalendarEvent) -> Option<usize> {
    let key = |name: Option<&str>, email: Option<&str>| {
        email
            .or(name)
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
    };
    let mut people: Vec<String> = Vec::new();
    let mut add = |k: Option<String>| {
        if let Some(k) = k {
            if !people.contains(&k) {
                people.push(k);
            }
        }
    };
    // Shared calendars publish events with the calendar itself as organizer
    // (`…@group.calendar.google.com`); that's not a person in the room.
    let organizer = key(event.organizer_name.as_deref(), event.organizer_email.as_deref())
        .filter(|k| !k.ends_with("calendar.google.com"));
    add(organizer);
    for a in &event.attendees {
        let declined = a
            .status
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("DECLINED"));
        if !declined {
            add(key(a.name.as_deref(), a.email.as_deref()));
        }
    }
    (people.len() >= 2).then_some(people.len())
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
    let options = crate::speaker_diarization::RefineOptions::default()
        .with_max_speakers(expected_speaker_count(pool, meeting_id).await);
    let refined = {
        let diarizer = diarizer.clone();
        tokio::task::spawn_blocking(move || diarizer.refine_with(&options))
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
    // Naming a chip after someone who already has a profile must not mint a
    // second profile with the same name (that's how the user ended up with
    // two "Seb Burrell"s, #102): fold the cluster into the existing one.
    if let Some(existing) = VoiceProfilesRepository::find_by_name(pool, trimmed_name, None)
        .await
        .map_err(|e| format!("Failed to look up voice profiles: {}", e))?
    {
        log::info!(
            "'{}' already has a voice profile ({}) — merging {} into it instead of creating another",
            trimmed_name,
            existing.id,
            old_label,
        );
        if existing.email.is_none() && normalised_email.is_some() {
            VoiceProfilesRepository::update_profile(pool, &existing.id, &existing.name, normalised_email)
                .await
                .map_err(|e| format!("Failed to update voice profile: {}", e))?;
        }
        let (renamed, _) =
            merge_cluster_into_profile_core(pool, meeting_id, old_label, &existing.id).await?;
        return Ok((Some(existing.id), renamed));
    }

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

/// Shared core of the `merge_voice_profiles` command: fold `loser_id` into
/// `winner_id` — sample-count-weighted centroid, transcripts relinked,
/// `is_self` kept — and delete the loser, atomically (see
/// [`VoiceProfilesRepository::merge_into`]). Returns
/// `(relinked_count, centroid_updated)`.
pub async fn merge_voice_profiles_core(
    pool: &SqlitePool,
    winner_id: &str,
    loser_id: &str,
) -> Result<(u64, bool), String> {
    let outcome = VoiceProfilesRepository::merge_into(pool, winner_id, loser_id)
        .await
        .map_err(|e| format!("Failed to merge voice profiles: {}", e))?;
    if !outcome.centroid_updated {
        log::warn!(
            "Embedding-dim mismatch merging {} into {} — relinked transcripts only",
            loser_id,
            winner_id,
        );
    }
    log::info!(
        "Merged profile {} into {}: {} transcripts relinked, centroid_updated={}, samples={}, is_self={}",
        loser_id,
        winner_id,
        outcome.relinked_transcripts,
        outcome.centroid_updated,
        outcome.sample_count,
        outcome.is_self,
    );
    Ok((outcome.relinked_transcripts, outcome.centroid_updated))
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

#[cfg(test)]
mod head_count_tests {
    use super::head_count;
    use crate::calendar::models::{Attendee, CalendarEvent};

    fn event(organizer: Option<&str>, attendees: &[(&str, Option<&str>)]) -> CalendarEvent {
        CalendarEvent {
            id: "e".into(),
            source_id: "s".into(),
            ics_uid: "u".into(),
            recurrence_id: None,
            summary: None,
            description: None,
            location: None,
            organizer_name: None,
            organizer_email: organizer.map(str::to_string),
            start_at: chrono::Utc::now(),
            end_at: chrono::Utc::now(),
            is_all_day: false,
            attendees: attendees
                .iter()
                .map(|(email, status)| Attendee {
                    email: Some(email.to_string()),
                    status: status.map(str::to_string),
                    ..Default::default()
                })
                .collect(),
        }
    }

    #[test]
    fn counts_organizer_and_attendees_once_each() {
        let e = event(
            Some("Ann@x.com"),
            &[("ann@x.com", None), ("bob@x.com", None), ("cat@x.com", None)],
        );
        assert_eq!(head_count(&e), Some(3));
    }

    #[test]
    fn skips_declines_and_calendar_organizers() {
        let e = event(
            Some("team@group.calendar.google.com"),
            &[("ann@x.com", None), ("bob@x.com", Some("DECLINED")), ("cat@x.com", Some("ACCEPTED"))],
        );
        assert_eq!(head_count(&e), Some(2));
    }

    #[test]
    fn a_one_person_event_is_no_head_count() {
        assert_eq!(head_count(&event(Some("ann@x.com"), &[])), None);
    }
}

#[cfg(test)]
mod duplicate_profile_tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    async fn pool_with_meeting() -> SqlitePool {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at)
             VALUES ('m1', 'Meeting', '2026-10-01T00:00:00Z', '2026-10-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        for (id, speaker) in [("t1", "Speaker 3"), ("t2", "Speaker 3"), ("t3", "Speaker 4")] {
            sqlx::query(
                "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, speaker)
                 VALUES (?, 'm1', 'hi', '2026-10-01T00:00:00Z', ?)",
            )
            .bind(id)
            .bind(speaker)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    /// Naming a chip after an existing profile folds it into that profile
    /// instead of minting a second one with the same name.
    #[tokio::test]
    async fn promoting_to_an_existing_name_reuses_that_profile() {
        let pool = pool_with_meeting().await;
        let me = VoiceProfilesRepository::upsert_self(&pool, "Seb Burrell", &[1.0, 0.0], 4)
            .await
            .unwrap();

        let (profile_id, renamed) = promote_speaker_to_profile_core(
            &pool,
            "Speaker 3",
            "seb burrell",
            Some("seb@example.com"),
            "m1",
        )
        .await
        .unwrap();
        assert_eq!(profile_id.as_deref(), Some(me.as_str()));
        assert_eq!(renamed, 2);

        let profiles = VoiceProfilesRepository::list_all(&pool).await.unwrap();
        assert_eq!(profiles.len(), 1, "no duplicate profile was created");
        assert!(profiles[0].is_self);
        assert_eq!(profiles[0].email.as_deref(), Some("seb@example.com"));

        let rows: Vec<(String, Option<String>, Option<String>)> =
            sqlx::query_as("SELECT id, speaker, voice_profile_id FROM transcripts ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows[0], ("t1".into(), Some("Seb Burrell".into()), Some(me.clone())));
        assert_eq!(rows[1], ("t2".into(), Some("Seb Burrell".into()), Some(me.clone())));
        assert_eq!(rows[2], ("t3".into(), Some("Speaker 4".into()), None));
    }
}
