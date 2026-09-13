use crate::audio::common::TranscriptSegment;
use crate::database::models::TranscriptSearchResult;
use chrono::Utc;
use sqlx::{Connection, Error as SqlxError, SqlitePool};
use tracing::{error, info};
use uuid::Uuid;

pub struct TranscriptsRepository;

impl TranscriptsRepository {
    /// Saves a new meeting and its associated transcript segments.
    /// This function uses a transaction to ensure that either both the meeting
    /// and all its transcripts are saved, or none of them are.
    pub async fn save_transcript(
        pool: &SqlitePool,
        meeting_title: &str,
        transcripts: &[TranscriptSegment],
        folder_path: Option<String>,
    ) -> Result<String, SqlxError> {
        let meeting_id = format!("meeting-{}", Uuid::new_v4());

        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        let now = Utc::now();

        // 1. Create the new meeting
        let result = sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at, folder_path) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&meeting_id)
        .bind(meeting_title)
        .bind(now)
        .bind(now)
        .bind(&folder_path)
        .execute(&mut *transaction)
        .await;

        if let Err(e) = result {
            error!("Failed to create meeting '{}': {}", meeting_title, e);
            transaction.rollback().await?;
            return Err(e);
        }

        info!("Successfully created meeting with id: {}", meeting_id);

        // 2. Save each transcript segment with audio timing + speaker fields
        //
        // `sequence_id` is persisted (rather than dropped in favour of the
        // generated uuid key) so post-meeting speaker refinement can match a
        // diarizer embedding back to the row it produced. The live-recording
        // path always carries one; batch paths leave it `None`.
        for segment in transcripts {
            let transcript_id = format!("transcript-{}", Uuid::new_v4());
            let result = sqlx::query(
                "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time, audio_end_time, duration, speaker, voice_profile_id, sequence_id, source, confidence)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
            )
            .bind(&transcript_id)
            .bind(&meeting_id)
            .bind(&segment.text)
            .bind(
                segment
                    .timestamp
                    .clone()
                    .unwrap_or_else(|| Utc::now().to_rfc3339()),
            )
            .bind(segment.audio_start_time)
            .bind(segment.audio_end_time)
            .bind(segment.duration)
            .bind(&segment.speaker)
            .bind(&segment.voice_profile_id)
            .bind(segment.sequence_id.map(|s| s as i64))
            .bind(&segment.source)
            .bind(segment.confidence)
            .execute(&mut *transaction)
            .await;

            if let Err(e) = result {
                error!(
                    "Failed to save transcript segment for meeting {}: {}",
                    meeting_id, e
                );
                transaction.rollback().await?;
                return Err(e);
            }
        }

        info!(
            "Successfully saved {} transcript segments for meeting {}",
            transcripts.len(),
            meeting_id
        );

        // Commit the transaction
        transaction.commit().await?;

        Ok(meeting_id)
    }

    /// Upsert a batch of live-recording transcript segments, keyed by
    /// `(meeting_id, sequence_id)` (issue #57 slice 2): a segment whose
    /// `(meeting_id, sequence_id)` already exists is updated in place
    /// (covers the transcription worker revising a partial into its final
    /// text under the same sequence id); anything new is inserted. One
    /// transaction per batch — called by `transcript_db_writer`'s periodic
    /// flush, so a batch is typically a handful of segments from one
    /// in-progress recording.
    ///
    /// A segment with `sequence_id: None` is always inserted as a new row
    /// (SQLite's UNIQUE index treats every NULL as distinct) — the
    /// live-recording path this writer serves always sets it, so that's
    /// only reachable from a caller passing raw segments in directly.
    ///
    /// Returns `Err` (rolling the whole batch back) on the first failing
    /// segment rather than skipping it, so a batch is never partially
    /// applied — the caller logs and drops the batch on error; the
    /// transcripts.ndjson journal on disk remains the source of truth if a
    /// batch is ever lost this way.
    pub async fn upsert_transcript_segments(
        pool: &SqlitePool,
        items: &[(String, TranscriptSegment)],
    ) -> Result<u64, SqlxError> {
        if items.is_empty() {
            return Ok(0);
        }

        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        let mut affected = 0u64;
        for (meeting_id, segment) in items {
            let transcript_id = format!("transcript-{}", Uuid::new_v4());
            let result = sqlx::query(
                "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time, audio_end_time, duration, speaker, voice_profile_id, sequence_id, source, confidence)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(meeting_id, sequence_id) DO UPDATE SET
                     transcript = excluded.transcript,
                     timestamp = excluded.timestamp,
                     audio_start_time = excluded.audio_start_time,
                     audio_end_time = excluded.audio_end_time,
                     duration = excluded.duration,
                     speaker = excluded.speaker,
                     voice_profile_id = excluded.voice_profile_id,
                     source = excluded.source,
                     confidence = excluded.confidence",
            )
            .bind(&transcript_id)
            .bind(meeting_id)
            .bind(&segment.text)
            .bind(
                segment
                    .timestamp
                    .clone()
                    .unwrap_or_else(|| Utc::now().to_rfc3339()),
            )
            .bind(segment.audio_start_time)
            .bind(segment.audio_end_time)
            .bind(segment.duration)
            .bind(&segment.speaker)
            .bind(&segment.voice_profile_id)
            .bind(segment.sequence_id.map(|s| s as i64))
            .bind(&segment.source)
            .bind(segment.confidence)
            .execute(&mut *transaction)
            .await;

            match result {
                Ok(r) => affected += r.rows_affected(),
                Err(e) => {
                    error!(
                        "Failed to upsert transcript segment (meeting {}, sequence {:?}): {}",
                        meeting_id, segment.sequence_id, e
                    );
                    transaction.rollback().await?;
                    return Err(e);
                }
            }
        }

        transaction.commit().await?;
        Ok(affected)
    }

    /// Replace every transcript row for `meeting_id` with `segments`, in one
    /// transaction — a full re-save (retranscription / auto-refine), not the
    /// incremental `upsert_transcript_segments` the live-recording path uses.
    pub async fn replace_transcripts_for_meeting(
        pool: &SqlitePool,
        meeting_id: &str,
        segments: &[TranscriptSegment],
    ) -> Result<(), SqlxError> {
        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        sqlx::query("DELETE FROM transcripts WHERE meeting_id = ?")
            .bind(meeting_id)
            .execute(&mut *transaction)
            .await?;

        for segment in segments {
            let result = sqlx::query(
                "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time, audio_end_time, duration, speaker, voice_profile_id)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&segment.id)
            .bind(meeting_id)
            .bind(&segment.text)
            .bind(
                segment
                    .timestamp
                    .clone()
                    .unwrap_or_else(|| Utc::now().to_rfc3339()),
            )
            .bind(segment.audio_start_time)
            .bind(segment.audio_end_time)
            .bind(segment.duration)
            .bind(&segment.speaker)
            .bind(&segment.voice_profile_id)
            .execute(&mut *transaction)
            .await;

            if let Err(e) = result {
                error!(
                    "Failed to insert transcript segment for meeting {}: {}",
                    meeting_id, e
                );
                transaction.rollback().await?;
                return Err(e);
            }
        }

        transaction.commit().await?;
        Ok(())
    }

    /// Re-point every transcript that currently references `from_profile_id`
    /// to `to_profile_id`, also rewriting the displayed `speaker` text to
    /// `to_name`. Used when merging two stored voice profiles into one.
    /// Returns the number of rows updated.
    pub async fn relink_transcripts(
        pool: &SqlitePool,
        from_profile_id: &str,
        to_profile_id: &str,
        to_name: &str,
    ) -> Result<u64, SqlxError> {
        let res = sqlx::query(
            "UPDATE transcripts
             SET voice_profile_id = ?, speaker = ?
             WHERE voice_profile_id = ?",
        )
        .bind(to_profile_id)
        .bind(to_name)
        .bind(from_profile_id)
        .execute(pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Replace every transcript's `speaker` label that currently equals
    /// `old_label` within `meeting_id` with `new_label`, and set
    /// `voice_profile_id` to `profile_id` (which may be `None` for the
    /// rename-only fallback). Returns the number of rows updated.
    ///
    /// Scoped to a single meeting because cluster numbering is per-meeting:
    /// "Speaker 1" in one recording is a different person than "Speaker 1"
    /// in another, so a global rename would mis-attribute speech.
    pub async fn rename_speaker_in_meeting(
        pool: &SqlitePool,
        meeting_id: &str,
        old_label: &str,
        new_label: &str,
        profile_id: Option<&str>,
    ) -> Result<u64, SqlxError> {
        let res = sqlx::query(
            "UPDATE transcripts
             SET speaker = ?, voice_profile_id = ?
             WHERE meeting_id = ? AND speaker = ?",
        )
        .bind(new_label)
        .bind(profile_id)
        .bind(meeting_id)
        .bind(old_label)
        .execute(pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Apply post-meeting speaker refinement to `meeting_id`'s transcripts.
    ///
    /// `updates` is `(sequence_id, expected_current_speaker, new_speaker,
    /// new_voice_profile_id)` — the profile id is `Some` when refinement
    /// folded the row's cluster into a stored voice profile, linking the row
    /// so its chip routes through profile editing from then on.
    /// Returns the number of rows actually rewritten.
    ///
    /// Three guards keep this from ever damaging a deliberate label, since
    /// refinement runs unattended in the background:
    ///
    /// 1. `voice_profile_id IS NULL` — a row matched to an enrolled voice (or
    ///    named by the user via promote/merge) is authoritative and is never
    ///    re-clustered. This mirrors the pinning in `refinement::refine` and
    ///    holds even if the in-memory diarizer history disagrees with the DB.
    /// 2. `speaker = expected_current_speaker` — makes each update a no-op
    ///    unless the row still carries the exact label the live pass gave it,
    ///    so a rename that landed between save and refinement wins, and a
    ///    re-run of refinement changes nothing.
    /// 3. `sequence_id = ?` — only ever matches rows that carry a sequence;
    ///    NULL-sequence rows (from data predating sequence tracking) are
    ///    silently left alone.
    ///
    /// All updates share one transaction: refinement is a single logical
    /// re-labelling of the meeting, so a failure partway through must not
    /// leave half the meeting re-clustered against the other half.
    pub async fn update_speakers_by_sequence(
        pool: &SqlitePool,
        meeting_id: &str,
        updates: &[(u64, String, String, Option<String>)],
    ) -> Result<u64, SqlxError> {
        if updates.is_empty() {
            return Ok(0);
        }

        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        let mut changed = 0u64;
        for (sequence_id, expected_speaker, new_speaker, new_profile_id) in updates {
            let result = sqlx::query(
                "UPDATE transcripts
                 SET speaker = ?, voice_profile_id = ?
                 WHERE meeting_id = ?
                   AND sequence_id = ?
                   AND speaker = ?
                   AND voice_profile_id IS NULL",
            )
            .bind(new_speaker)
            .bind(new_profile_id)
            .bind(meeting_id)
            .bind(*sequence_id as i64)
            .bind(expected_speaker)
            .execute(&mut *transaction)
            .await;

            match result {
                Ok(r) => changed += r.rows_affected(),
                Err(e) => {
                    error!(
                        "Failed to refine speaker for meeting {} sequence {}: {}",
                        meeting_id, sequence_id, e
                    );
                    transaction.rollback().await?;
                    return Err(e);
                }
            }
        }

        transaction.commit().await?;
        info!(
            "Speaker refinement rewrote {} of {} candidate rows in meeting {}",
            changed,
            updates.len(),
            meeting_id
        );
        Ok(changed)
    }

    /// Searches for a query string within the transcripts.
    /// It returns a list of matching transcripts with context.
    pub async fn search_transcripts(
        pool: &SqlitePool,
        query: &str,
    ) -> Result<Vec<TranscriptSearchResult>, SqlxError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let search_query = format!("%{}%", query.to_lowercase());

        let rows = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT m.id, m.title, t.transcript, t.timestamp
             FROM meetings m
             JOIN transcripts t ON m.id = t.meeting_id
             WHERE LOWER(t.transcript) LIKE ?",
        )
        .bind(&search_query)
        .fetch_all(pool)
        .await?;

        let results = rows
            .into_iter()
            .map(|(id, title, transcript, timestamp)| {
                let match_context = Self::get_match_context(&transcript, query);
                TranscriptSearchResult {
                    id,
                    title,
                    match_context,
                    timestamp,
                }
            })
            .collect();

        Ok(results)
    }

    /// Helper function to extract a snippet of text around the first match of a query.
    fn get_match_context(transcript: &str, query: &str) -> String {
        let transcript_lower = transcript.to_lowercase();
        let query_lower = query.to_lowercase();

        match transcript_lower.find(&query_lower) {
            Some(match_index) => {
                let start_index = match_index.saturating_sub(100);
                let end_index = (match_index + query.len() + 100).min(transcript.len());

                let mut context = String::new();
                if start_index > 0 {
                    context.push_str("...");
                }
                context.push_str(&transcript[start_index..end_index]);
                if end_index < transcript.len() {
                    context.push_str("...");
                }
                context
            }
            None => transcript.chars().take(200).collect(), // Fallback to the start of the transcript
        }
    }
}
