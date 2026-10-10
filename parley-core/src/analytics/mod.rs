//! Talk-time and speaking analytics per meeting (issue #86).
//!
//! [`compute`] holds the pure statistics (shared verbatim with
//! `parley-mcp`); this module adds the loader that reads a meeting's
//! segments and speaker display names from SQLite.

pub mod compute;

pub use compute::{
    compute, format_clock, format_span, speaker_display_name, Channel, MeetingStats, Monologue, Segment,
    SpeakerStats,
};

use sqlx::{Row, SqlitePool};

/// A meeting's segments in recorded order, speaker names resolved, plus the
/// stored recording length (if any).
pub async fn load_meeting_segments(
    pool: &SqlitePool,
    meeting_id: &str,
) -> Result<(Vec<Segment>, Option<f64>), sqlx::Error> {
    let duration: Option<f64> = sqlx::query_scalar("SELECT duration_seconds FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_optional(pool)
        .await?
        .flatten();

    // Same order as the transcript view / MCP: sequence id (authoritative
    // for live recordings), then start time, then the wall-clock stamp.
    let rows = sqlx::query(
        r#"
        SELECT
            t.transcript, t.speaker, t.source,
            t.audio_start_time, t.audio_end_time, t.duration,
            vp.name AS profile_name, vp.is_self AS profile_is_self
        FROM transcripts t
        LEFT JOIN voice_profiles vp ON vp.id = t.voice_profile_id
        WHERE t.meeting_id = ?
        ORDER BY
            t.sequence_id IS NULL, t.sequence_id,
            t.audio_start_time IS NULL, t.audio_start_time,
            t.timestamp
        "#,
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await?;

    let segments = rows
        .into_iter()
        .map(|r| {
            let speaker: Option<String> = r.try_get("speaker")?;
            let source: Option<String> = r.try_get("source")?;
            let profile: Option<String> = r.try_get("profile_name")?;
            let is_self: Option<i64> = r.try_get("profile_is_self")?;
            let mut channel = Channel::from_source(source.as_deref());
            if channel == Channel::Unknown {
                // Older rows: no source column, but the enrolled self profile
                // or the legacy raw-channel label still tells us.
                channel = match (is_self, speaker.as_deref().map(str::trim)) {
                    (Some(1), _) | (_, Some("mic")) => Channel::Me,
                    (_, Some("system")) => Channel::Others,
                    _ => Channel::Unknown,
                };
            }
            Ok(Segment {
                speaker: Some(speaker_display_name(profile.as_deref(), speaker.as_deref(), source.as_deref())),
                channel,
                text: r.try_get("transcript")?,
                start: r.try_get("audio_start_time")?,
                end: r.try_get("audio_end_time")?,
                duration: r.try_get("duration")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    Ok((segments, duration))
}

/// Load and compute a meeting's stats in one go.
pub async fn meeting_stats(pool: &SqlitePool, meeting_id: &str) -> Result<MeetingStats, sqlx::Error> {
    let (segments, duration) = load_meeting_segments(pool, meeting_id).await?;
    Ok(compute(&segments, duration))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    async fn test_pool() -> SqlitePool {
        let options = SqliteConnectOptions::from_str("sqlite::memory:").unwrap().foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("in-memory sqlite pool");
        sqlx::migrate!("./migrations").run(&pool).await.expect("run migrations");
        pool
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert(
        pool: &SqlitePool,
        id: &str,
        seq: Option<i64>,
        text: &str,
        speaker: Option<&str>,
        source: Option<&str>,
        profile: Option<&str>,
        times: (Option<f64>, Option<f64>),
    ) {
        sqlx::query(
            "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, speaker, source, voice_profile_id, \
             audio_start_time, audio_end_time, sequence_id) VALUES (?, 'm1', ?, '2026-01-01T00:00:00Z', ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(text)
        .bind(speaker)
        .bind(source)
        .bind(profile)
        .bind(times.0)
        .bind(times.1)
        .bind(seq)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn loads_segments_with_resolved_names_and_channels() {
        let pool = test_pool().await;
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at, duration_seconds) \
             VALUES ('m1', 'Sync', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 60.0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO voice_profiles (id, name, embedding, embedding_dim, created_at, updated_at) \
             VALUES ('vp1', 'Alice', x'00000000', 1, '', '')",
        )
        .execute(&pool)
        .await
        .unwrap();

        insert(&pool, "t2", Some(2), "Sounds good?", Some("Speaker 1"), Some("system"), Some("vp1"), (Some(10.0), Some(20.0))).await;
        insert(&pool, "t1", Some(1), "Hi all", Some("Me"), Some("mic"), None, (Some(0.0), Some(10.0))).await;
        insert(&pool, "t3", None, "legacy row", Some("system"), None, None, (None, None)).await;

        let (segments, duration) = load_meeting_segments(&pool, "m1").await.unwrap();
        assert_eq!(duration, Some(60.0));
        let names: Vec<_> = segments.iter().map(|s| s.speaker.clone().unwrap()).collect();
        assert_eq!(names, vec!["Me", "Alice", "Others"]);
        assert_eq!(
            segments.iter().map(|s| s.channel).collect::<Vec<_>>(),
            vec![Channel::Me, Channel::Others, Channel::Others]
        );

        let stats = meeting_stats(&pool, "m1").await.unwrap();
        assert_eq!(stats.segment_count, 3);
        assert_eq!(stats.untimed_segments, 1);
        assert_eq!(stats.duration_secs, Some(60.0));
        assert_eq!(stats.me_share, Some(0.5));
        assert_eq!(stats.total_questions, 1);
    }

    #[tokio::test]
    async fn unknown_meeting_is_empty() {
        let pool = test_pool().await;
        let stats = meeting_stats(&pool, "nope").await.unwrap();
        assert!(stats.is_empty());
    }
}
