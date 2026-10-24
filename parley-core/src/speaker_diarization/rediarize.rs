//! Re-diarize an existing meeting from its stored audio (#115).
//!
//! #114 cut speaker over-fragmentation for new recordings and
//! retranscriptions, but meetings recorded before it keep their old labels
//! (often 14–28 "Speaker N"s). This re-runs speaker embedding and the
//! offline refinement over the meeting's saved audio, using the transcript's
//! existing segment boundaries — Whisper isn't involved, so the text is
//! untouched and it takes seconds rather than minutes.
//!
//! Lines the user has named are left alone: anything linked to a voice
//! profile, or carrying a name other than a generic placeholder ("Speaker
//! N", "Me", "Speaker"). Only the generic lines are relabelled, and their
//! embeddings still take part in the clustering.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, Result};
use serde::Serialize;
use sqlx::{Row, SqlitePool};

use super::{service, RefineOptions, SegmentSource};
use crate::audio::common::{has_split_capture_channels, ChannelAudio};
use crate::audio::decoder::decode_audio_file;
use crate::events::{EventSinkExt, SharedEventSink};

/// What a re-diarization changed.
#[derive(Debug, Clone, Serialize)]
pub struct RediarizeReport {
    pub meeting_id: String,
    /// Distinct speaker labels before and after.
    pub speakers_before: usize,
    pub speakers_after: usize,
    /// Transcript lines relabelled.
    pub changed: usize,
    /// Lines left alone because the user named them.
    pub kept: usize,
}

static RUNNING: AtomicBool = AtomicBool::new(false);

/// Whether a re-diarization is running.
pub fn is_running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

/// A placeholder label the diarizer assigned, as opposed to a name the user
/// gave.
pub fn is_generic_label(label: Option<&str>) -> bool {
    let Some(label) = label.map(str::trim).filter(|l| !l.is_empty()) else {
        return true;
    };
    matches!(label, "Speaker" | "Me" | "mic" | "system" | "Unknown")
        || label
            .strip_prefix("Speaker ")
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

struct Row_ {
    id: String,
    start: f64,
    end: f64,
    speaker: Option<String>,
    voice_profile_id: Option<String>,
}

impl Row_ {
    fn named(&self) -> bool {
        self.voice_profile_id.is_some() || !is_generic_label(self.speaker.as_deref())
    }
}

/// Re-diarize one meeting. Errors when the meeting has no saved audio,
/// is being recorded, or another re-diarization is running.
pub async fn rediarize_meeting(
    sink: &SharedEventSink,
    pool: &SqlitePool,
    meeting_id: &str,
) -> Result<RediarizeReport> {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err(anyhow!("Speakers are already being re-detected for another meeting."));
    }
    let result = run(sink, pool, meeting_id).await;
    RUNNING.store(false, Ordering::SeqCst);
    result
}

async fn run(sink: &SharedEventSink, pool: &SqlitePool, meeting_id: &str) -> Result<RediarizeReport> {
    if crate::audio::recording_service::current_meeting_id().as_deref() == Some(meeting_id) {
        return Err(anyhow!("This meeting is still being recorded."));
    }
    let folder: Option<String> = sqlx::query_scalar("SELECT folder_path FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| anyhow!("That meeting no longer exists."))?;
    let folder = folder
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("This meeting has no saved audio."))?;
    let audio_path = crate::audio::retranscription::find_audio_file(&folder)
        .map_err(|_| anyhow!("This meeting has no saved audio."))?;

    let rows: Vec<Row_> = sqlx::query(
        "SELECT id, audio_start_time, audio_end_time, speaker, voice_profile_id
         FROM transcripts
         WHERE meeting_id = ? AND audio_start_time IS NOT NULL AND audio_end_time IS NOT NULL
           AND audio_end_time > audio_start_time
         ORDER BY audio_start_time, sequence_id",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| Row_ {
        id: r.get("id"),
        start: r.get("audio_start_time"),
        end: r.get("audio_end_time"),
        speaker: r.get("speaker"),
        voice_profile_id: r.get("voice_profile_id"),
    })
    .collect();
    if rows.is_empty() {
        return Err(anyhow!("This meeting has no timed transcript lines to work from."));
    }
    let speakers_before = distinct(rows.iter().map(|r| r.speaker.clone()));

    let diarizer = service::build_diarizer(Some(pool))
        .await?
        .ok_or_else(|| anyhow!("The speaker model isn't downloaded yet."))?;
    service::apply_meeting_attendees(pool, &diarizer, meeting_id).await;
    let options = RefineOptions::default()
        .with_max_speakers(service::expected_speaker_count(pool, meeting_id).await);

    let split = has_split_capture_channels(&folder);
    let starts: Vec<(f64, f64)> = rows.iter().map(|r| (r.start, r.end)).collect();
    let d = diarizer.clone();
    let refined = tokio::task::spawn_blocking(move || -> Result<_> {
        let decoded = decode_audio_file(&audio_path)?;
        let mono = decoded.to_whisper_format();
        let channels = if split {
            decoded
                .split_channels_to_whisper_format(None)?
                .map(|(mic, system)| ChannelAudio { mic, system })
        } else {
            None
        };
        drop(decoded);
        for (i, (start, end)) in starts.iter().enumerate() {
            let (start_ms, end_ms) = (start * 1000.0, end * 1000.0);
            let (source, channel_samples) = match channels.as_ref() {
                Some(c) => c.for_segment(start_ms, end_ms),
                None => (SegmentSource::Unknown, None),
            };
            let samples = match channel_samples.filter(|s| !s.is_empty()) {
                Some(s) => s,
                None => {
                    let a = ((start * 16_000.0) as usize).min(mono.len());
                    let b = ((end * 16_000.0) as usize).min(mono.len());
                    &mono[a..b.max(a)]
                }
            };
            if let Err(e) = d.process_with_hint(i as u64, samples, None, None, source) {
                log::debug!("re-diarize: segment {i} not embedded: {e}");
            }
        }
        let refined = d.refine_with(&options);
        d.apply_refinement(&refined);
        Ok(refined)
    })
    .await
    .map_err(|e| anyhow!("Speaker detection task failed: {e}"))??;

    // New label per row: the refined assignment for its sequence id, or the
    // live pass's label when refinement had nothing to say.
    let mut new_labels: Vec<(Option<String>, Option<String>)> =
        rows.iter().map(|r| (r.speaker.clone(), r.voice_profile_id.clone())).collect();
    for a in &refined {
        if let Some(slot) = new_labels.get_mut(a.sequence_id as usize) {
            *slot = (Some(a.speaker.clone()), a.voice_profile_id.clone());
        }
    }

    let mut kept = 0;
    let mut updates = Vec::new();
    for (row, (speaker, profile)) in rows.iter().zip(&new_labels) {
        if row.named() {
            kept += 1;
            continue;
        }
        if speaker != &row.speaker || profile != &row.voice_profile_id {
            updates.push((row, speaker.clone(), profile.clone()));
        }
    }

    let mut tx = pool.begin().await?;
    let mut changed = 0usize;
    for (row, speaker, profile) in &updates {
        // Only rows still showing what we read: a rename since is kept.
        let r = sqlx::query(
            "UPDATE transcripts SET speaker = ?, voice_profile_id = ?
             WHERE id = ? AND speaker IS ? AND voice_profile_id IS ?",
        )
        .bind(speaker)
        .bind(profile)
        .bind(&row.id)
        .bind(&row.speaker)
        .bind(&row.voice_profile_id)
        .execute(&mut *tx)
        .await?;
        changed += r.rows_affected() as usize;
    }
    tx.commit().await?;

    let after: Vec<Option<String>> = sqlx::query_scalar("SELECT speaker FROM transcripts WHERE meeting_id = ?")
        .bind(meeting_id)
        .fetch_all(pool)
        .await?;
    let speakers_after = distinct(after);

    // Let "name this speaker" learn from this session's embeddings, as after
    // a retranscription — unless a live recording owns the slot.
    if !crate::audio::recording_service::is_recording().await {
        super::set_current_diarizer(Some(diarizer));
    }

    log::info!(
        "Re-diarized meeting {meeting_id}: {speakers_before} → {speakers_after} speakers, \
         {changed} lines relabelled, {kept} named lines kept"
    );
    let _ = sink.emit_event(
        "speakers-refined",
        &serde_json::json!({ "meeting_id": meeting_id, "changed_count": changed }),
    );
    Ok(RediarizeReport {
        meeting_id: meeting_id.to_string(),
        speakers_before,
        speakers_after,
        changed,
        kept,
    })
}

fn distinct(labels: impl IntoIterator<Item = Option<String>>) -> usize {
    labels
        .into_iter()
        .flatten()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect::<HashSet<_>>()
        .len()
}

/// Meetings with saved audio and at least one generic speaker label, newest
/// first — the candidates for "re-detect speakers in all meetings".
pub async fn meetings_to_rediarize(pool: &SqlitePool) -> Result<Vec<String>> {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT DISTINCT m.id, m.folder_path FROM meetings m
         JOIN transcripts t ON t.meeting_id = m.id
         WHERE COALESCE(m.status, 'completed') != 'recording'
           AND t.voice_profile_id IS NULL
           AND (t.speaker IS NULL OR t.speaker = 'Me' OR t.speaker LIKE 'Speaker%')
         ORDER BY m.created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|(_, folder)| {
            folder
                .as_deref()
                .is_some_and(|f| crate::audio::retranscription::find_audio_file(std::path::Path::new(f)).is_ok())
        })
        .map(|(id, _)| id)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_labels_are_placeholders_only() {
        for generic in [None, Some(""), Some("Speaker"), Some("Speaker 3"), Some("Speaker 12"), Some("Me"), Some("mic")] {
            assert!(is_generic_label(generic), "{generic:?}");
        }
        for named in [Some("Priya Shah"), Some("Speaker A"), Some("Speakerphone"), Some("Meg")] {
            assert!(!is_generic_label(named), "{named:?}");
        }
    }

    #[test]
    fn distinct_ignores_blank_labels() {
        let labels = [Some("A".to_string()), Some(" A ".to_string()), None, Some(String::new()), Some("B".into())];
        assert_eq!(distinct(labels), 2);
    }
}

/// Manual check against a scratch copy of a real data directory (never the
/// live one): re-diarizes each listed meeting and prints the report.
///
/// ```text
/// XDG_DATA_HOME=/scratch/share PARLEY_REDIARIZE_MEETINGS=<id>,<id> \
/// cargo test -p parley-core --release rediarize_scratch_copy -- --ignored --nocapture
/// ```
#[cfg(test)]
mod manual {
    #[tokio::test]
    #[ignore]
    async fn rediarize_scratch_copy() {
        crate::bootstrap::init_paths();
        let dir = crate::paths::app_data_dir().unwrap();
        let live = dirs::home_dir().unwrap().join(".local/share/io.github.hankanman.Parley");
        assert!(
            std::env::var_os("XDG_DATA_HOME").is_some() && dir != live,
            "point XDG_DATA_HOME at a scratch copy"
        );
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", dir.join("meeting_minutes.sqlite").display()))
            .await
            .unwrap();
        let sink: crate::events::SharedEventSink = std::sync::Arc::new(crate::events::NullSink);
        let ids = std::env::var("PARLEY_REDIARIZE_MEETINGS").unwrap_or_default();
        let ids: Vec<String> = if ids.is_empty() {
            super::meetings_to_rediarize(&pool).await.unwrap()
        } else {
            ids.split(',').map(str::to_string).collect()
        };
        for id in ids {
            let started = std::time::Instant::now();
            match super::rediarize_meeting(&sink, &pool, &id).await {
                Ok(r) => eprintln!(
                    "{id}: {} -> {} speakers, {} changed, {} kept ({:.1}s)",
                    r.speakers_before,
                    r.speakers_after,
                    r.changed,
                    r.kept,
                    started.elapsed().as_secs_f32()
                ),
                Err(e) => eprintln!("{id}: error: {e}"),
            }
        }
    }
}
