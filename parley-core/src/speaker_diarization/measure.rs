//! Offline measurement of speaker fragmentation on real recordings (#102).
//!
//! Not a unit test: it reads a *copy* of a real database plus the recordings
//! it points at, re-embeds every transcript segment with the CAM++ model, and
//! prints how many distinct speakers each stage of the pipeline produces.
//! Run with:
//!
//! ```text
//! PARLEY_MEASURE_DB=/path/to/copy/meeting_minutes.sqlite \
//! PARLEY_MEASURE_MEETINGS=<meeting-id>,<meeting-id> \
//! PARLEY_MEASURE_MODEL=/path/to/3dspeaker_speech_campplus_sv_en_voxceleb_16k.onnx \
//! PARLEY_MEASURE_CACHE=/some/scratch/dir \
//! cargo test -p parley-core --release measure_fragmentation -- --ignored --nocapture
//! ```
//!
//! The database is opened read-only and nothing is written next to the
//! recordings; extracted features are cached under `PARLEY_MEASURE_CACHE`
//! so tuning runs skip the (slow) decode + embed step.
//!
//! Segments come from the transcript rows already in the DB (the same
//! VAD-derived boundaries the batch path produced), so this measures the
//! diarization stages in isolation from Whisper.

use super::clusterer::OnlineSpeakerClusterer;
use super::embedding_math::l2_normalize;
use super::refinement::{refine, RefineOptions};
use super::{
    EmbeddingRecord, SegmentSource, SpeakerEmbedder, SpeakerProfileMatcher,
    DEFAULT_CLUSTER_THRESHOLD, PROFILE_MATCH_THRESHOLD,
};
use crate::audio::decoder::decode_audio_file;
use crate::database::repositories::voice_profile::bytes_to_floats;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Serialize, Deserialize, Clone)]
struct SegmentFeatures {
    start: f64,
    end: f64,
    /// Embedding of the mono downmix (what the batch path embeds today).
    embedding: Option<Vec<f32>>,
    /// Embeddings of the mic (left) / system (right) channel alone.
    mic_embedding: Option<Vec<f32>>,
    sys_embedding: Option<Vec<f32>>,
    /// Mean-square energy of the left (mic) and right (system) channel over
    /// the segment, on the raw decoded audio.
    mic_energy: f64,
    sys_energy: f64,
    db_speaker: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct MeetingFeatures {
    title: String,
    segments: Vec<SegmentFeatures>,
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

async fn extract(
    pool: &sqlx::SqlitePool,
    meeting_id: &str,
    embedder: &SpeakerEmbedder,
) -> anyhow::Result<MeetingFeatures> {
    let meeting = sqlx::query("SELECT title, audio_path, folder_path FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_one(pool)
        .await?;
    let title: String = meeting.get("title");
    let audio_path: Option<String> = meeting.get("audio_path");
    let folder: Option<String> = meeting.get("folder_path");
    let audio = audio_path
        .map(PathBuf::from)
        .or_else(|| folder.map(|f| Path::new(&f).join("audio.mp4")))
        .ok_or_else(|| anyhow::anyhow!("meeting {meeting_id} has no audio"))?;

    let rows = sqlx::query(
        "SELECT audio_start_time, audio_end_time, speaker FROM transcripts
         WHERE meeting_id = ? AND audio_start_time IS NOT NULL AND audio_end_time IS NOT NULL
         ORDER BY audio_start_time",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await?;

    eprintln!("[{title}] decoding {}", audio.display());
    let decoded = decode_audio_file(&audio)?;
    let mono_16k = decoded.to_whisper_format();
    let channels = decoded.channels as usize;
    let sr = decoded.sample_rate as f64;
    let channel_16k = |c: usize| -> Option<Vec<f32>> {
        (channels >= 2).then(|| {
            crate::audio::decoder::DecodedAudio {
                samples: decoded.samples.iter().skip(c).step_by(channels).copied().collect(),
                sample_rate: decoded.sample_rate,
                channels: 1,
                duration_seconds: decoded.duration_seconds,
            }
            .to_whisper_format()
        })
    };
    let mic_16k = channel_16k(0);
    let sys_16k = channel_16k(1);
    let embed = |audio: &[f32], start: f64, end: f64| -> Option<Vec<f32>> {
        let a = ((start * 16_000.0) as usize).min(audio.len());
        let b = ((end * 16_000.0) as usize).min(audio.len());
        (b > a).then(|| embedder.embed(&audio[a..b]).ok()).flatten().map(|mut e| {
            l2_normalize(&mut e);
            e
        })
    };

    let mut segments = Vec::with_capacity(rows.len());
    for row in rows {
        let start: f64 = row.get("audio_start_time");
        let end: f64 = row.get("audio_end_time");
        let embedding = embed(&mono_16k, start, end);
        let mic_embedding = mic_16k.as_deref().and_then(|x| embed(x, start, end));
        let sys_embedding = sys_16k.as_deref().and_then(|x| embed(x, start, end));
        let (mut mic, mut sys, mut n) = (0.0f64, 0.0f64, 0usize);
        if channels >= 2 {
            let fa = (start * sr) as usize;
            let fb = ((end * sr) as usize).min(decoded.samples.len() / channels);
            for f in fa..fb {
                let l = decoded.samples[f * channels] as f64;
                let r = decoded.samples[f * channels + 1] as f64;
                mic += l * l;
                sys += r * r;
                n += 1;
            }
        }
        let n = n.max(1) as f64;
        segments.push(SegmentFeatures {
            start,
            end,
            embedding,
            mic_embedding,
            sys_embedding,
            mic_energy: mic / n,
            sys_energy: sys / n,
            db_speaker: row.get("speaker"),
        });
    }
    Ok(MeetingFeatures { title, segments })
}

fn distinct(labels: impl IntoIterator<Item = String>) -> usize {
    labels.into_iter().collect::<HashSet<_>>().len()
}

/// Labels holding at least `min_secs` of speech — "people you'd actually
/// see in the talk-share chart".
fn substantial(labels: &[String], segs: &[SegmentFeatures], min_secs: f64) -> usize {
    let mut totals: std::collections::HashMap<&str, f64> = Default::default();
    for (l, s) in labels.iter().zip(segs) {
        *totals.entry(l.as_str()).or_default() += s.end - s.start;
    }
    totals.values().filter(|&&t| t >= min_secs).count()
}

#[tokio::test]
#[ignore = "offline measurement on real recordings; see module docs"]
async fn measure_fragmentation() {
    let (Some(db), Some(model), Some(cache)) = (
        env_path("PARLEY_MEASURE_DB"),
        env_path("PARLEY_MEASURE_MODEL"),
        env_path("PARLEY_MEASURE_CACHE"),
    ) else {
        eprintln!("set PARLEY_MEASURE_DB / PARLEY_MEASURE_MODEL / PARLEY_MEASURE_CACHE");
        return;
    };
    let meetings: Vec<String> = std::env::var("PARLEY_MEASURE_MEETINGS")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    std::fs::create_dir_all(&cache).unwrap();

    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db.display()))
        .unwrap()
        .read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();

    let embedder = SpeakerEmbedder::from_path(&model, 4).expect("speaker model");
    let dim = embedder.dim();

    let profiles = sqlx::query("SELECT id, name, embedding, is_self FROM voice_profiles")
        .fetch_all(&pool)
        .await
        .unwrap();
    let mut self_id = None;
    let entries: Vec<(String, String, Vec<f32>)> = profiles
        .iter()
        .filter_map(|r| {
            let id: String = r.get("id");
            let is_self: bool = r.get("is_self");
            if is_self {
                self_id = Some(id.clone());
            }
            bytes_to_floats(&r.get::<Vec<u8>, _>("embedding")).map(|e| (id, r.get("name"), e))
        })
        .collect();
    let matcher = SpeakerProfileMatcher::new(dim, entries, PROFILE_MATCH_THRESHOLD)
        .unwrap()
        .with_self_profile(self_id);

    let min_secs: f64 = env_f32("PARLEY_MEASURE_MIN_SECS").unwrap_or(30.0) as f64;
    let mut options = RefineOptions::default();
    if let Some(v) = env_f32("PARLEY_MEASURE_THRESHOLD") {
        options.threshold = v;
    }
    if let Some(v) = env_f32("PARLEY_MEASURE_FOLD") {
        options.fold_similarity = v;
    }
    if let Some(v) = env_f32("PARLEY_MEASURE_MINOR_SECS") {
        options.min_cluster_secs = v;
    }
    if let Some(v) = env_f32("PARLEY_MEASURE_MERGE") {
        options.merge_similarity = v;
    }
    if let Some(v) = env_f32("PARLEY_MEASURE_MIC_MERGE") {
        options.mic_merge_similarity = v;
    }
    if let Some(v) = env_f32("PARLEY_MEASURE_SHORT") {
        options.short_segment_secs = v;
    }
    eprintln!("new = {options:?}");
    eprintln!(
        "{:<36} {:>4} | {:>4} | mixdown: {:>4} {:>4} {:>4} | per-channel: {:>4} {:>4} | substantial (>= {min_secs}s) db/live/new-ch",
        "meeting", "segs", "db", "live", "old", "new", "live", "new"
    );

    for meeting_id in &meetings {
        let cache_file = cache.join(format!("{meeting_id}.json"));
        let features: MeetingFeatures = match std::fs::read(&cache_file) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap(),
            Err(_) => {
                let f = extract(&pool, meeting_id, &embedder).await.unwrap();
                std::fs::write(&cache_file, serde_json::to_vec(&f).unwrap()).unwrap();
                f
            }
        };
        let segs: Vec<SegmentFeatures> = features
            .segments
            .iter()
            .filter(|s| s.embedding.is_some())
            .cloned()
            .collect();

        let mix = replay(&segs, &matcher, false);
        let per_channel = replay(&segs, &matcher, true);
        let labels = |a: Vec<super::RefinedAssignment>| -> Vec<String> {
            a.into_iter().map(|a| a.speaker).collect()
        };
        let live_mix: Vec<String> = mix.iter().map(|r| r.label.clone()).collect();
        let old_mix = labels(refine(&mix, &RefineOptions::legacy(), Some(&matcher)));
        let new_mix = labels(refine(&mix, &options, Some(&matcher)));
        let live_ch: Vec<String> = per_channel.iter().map(|r| r.label.clone()).collect();
        let new_ch = labels(refine(&per_channel, &options, Some(&matcher)));
        let db: Vec<String> = segs
            .iter()
            .map(|s| s.db_speaker.clone().unwrap_or_default())
            .collect();
        // Calendar attendee count, when the meeting is linked to an event.
        let attendees: Option<i64> = sqlx::query_scalar(
            "SELECT json_array_length(e.attendees_json) FROM meetings m
             JOIN calendar_events e ON e.id = m.calendar_event_id WHERE m.id = ?",
        )
        .bind(meeting_id)
        .fetch_optional(&pool)
        .await
        .unwrap_or_else(|e| {
            eprintln!("  attendee lookup failed: {e}");
            None
        });
        // A mono import: same mixdown embeddings, no channel information.
        let mono: Vec<EmbeddingRecord> = mix
            .iter()
            .cloned()
            .map(|mut r| {
                r.source = SegmentSource::Unknown;
                r
            })
            .collect();
        let new_mono = labels(refine(&mono, &options, Some(&matcher)));
        eprintln!(
            "  (mono import: live {} -> new {} speakers)",
            distinct(live_mix.clone()),
            distinct(new_mono)
        );
        let capped = attendees.map(|n| {
            let options = options.clone().with_max_speakers(Some(n as usize));
            distinct(labels(refine(&per_channel, &options, Some(&matcher))))
        });

        let title: String = features.title.chars().take(36).collect();
        eprintln!(
            "{:<36} {:>4} | {:>4} |          {:>4} {:>4} {:>4} |              {:>4} {:>4} | {}/{}/{}{}",
            title,
            segs.len(),
            distinct(db.clone()),
            distinct(live_mix.clone()),
            distinct(old_mix.clone()),
            distinct(new_mix.clone()),
            distinct(live_ch.clone()),
            distinct(new_ch.clone()),
            substantial(&db, &segs, min_secs),
            substantial(&live_mix, &segs, min_secs),
            substantial(&new_ch, &segs, min_secs),
            match (attendees, capped) {
                (Some(a), Some(c)) => format!(" | {a} attendees -> {c} with cap"),
                _ => String::new(),
            },
        );
        if std::env::var_os("PARLEY_MEASURE_SIMS").is_some() {
            centroid_similarities(&new_ch, &per_channel, min_secs as f32);
        }
        if let Ok(stage) = std::env::var("PARLEY_MEASURE_VERBOSE") {
            match stage.as_str() {
                "live" => breakdown(&live_mix, &mix, &segs),
                "old" => breakdown(&old_mix, &mix, &segs),
                "new" => breakdown(&new_mix, &mix, &segs),
                "live-ch" => breakdown(&live_ch, &per_channel, &segs),
                _ => breakdown(&new_ch, &per_channel, &segs),
            }
        }
    }
}

fn env_f32(name: &str) -> Option<f32> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}

/// Replay the batch path's per-segment diarization: online greedy
/// clustering with a per-segment profile match on top. `per_channel` embeds
/// the dominant channel's audio instead of the mono downmix.
fn replay(
    segs: &[SegmentFeatures],
    matcher: &SpeakerProfileMatcher,
    per_channel: bool,
) -> Vec<EmbeddingRecord> {
    let mut clusterer = OnlineSpeakerClusterer::new(DEFAULT_CLUSTER_THRESHOLD);
    segs.iter()
        .enumerate()
        .map(|(i, s)| {
            let source = SegmentSource::from_channel_energy(s.mic_energy, s.sys_energy);
            let channel = match source {
                SegmentSource::Microphone => s.mic_embedding.clone(),
                SegmentSource::System => s.sys_embedding.clone(),
                SegmentSource::Unknown => None,
            };
            let e = if per_channel { channel } else { None }
                .or_else(|| s.embedding.clone())
                .unwrap();
            let id = clusterer.assign(e.clone());
            let m = matcher.search(&e);
            EmbeddingRecord {
                sequence_id: i as u64,
                label: m
                    .as_ref()
                    .map(|m| m.name.clone())
                    .unwrap_or_else(|| clusterer.label_for(id)),
                voice_profile_id: m.as_ref().map(|m| m.profile_id.clone()),
                profile_score: m.map(|m| m.score),
                embedding: e,
                duration_secs: (s.end - s.start) as f32,
                source,
            }
        })
        .collect()
}


/// Per-label talk time and channel mix, for eyeballing one stage's output.
fn breakdown(labels: &[String], history: &[EmbeddingRecord], segs: &[SegmentFeatures]) {
    #[derive(Default)]
    struct Totals {
        secs: f64,
        n: usize,
        mic: usize,
        sys: usize,
        pinned: usize,
    }
    let mut totals: std::collections::BTreeMap<&str, Totals> = Default::default();
    for ((l, r), s) in labels.iter().zip(history).zip(segs) {
        let t = totals.entry(l.as_str()).or_default();
        t.secs += s.end - s.start;
        t.n += 1;
        match r.source {
            SegmentSource::Microphone => t.mic += 1,
            SegmentSource::System => t.sys += 1,
            SegmentSource::Unknown => {}
        }
        if r.voice_profile_id.is_some() {
            t.pinned += 1;
        }
    }
    for (l, t) in totals {
        eprintln!(
            "    {l:<22} {:>7.1}s {:>4} segs  mic {:>3} sys {:>3} unk {:>3}  live-pinned {:>3}",
            t.secs,
            t.n,
            t.mic,
            t.sys,
            t.n - t.mic - t.sys,
            t.pinned
        );
    }
}

/// Pairwise centroid similarity between the substantial speakers of one
/// stage's output — how alike two voices that the pipeline kept apart are,
/// which bounds how low a fold threshold can safely go.
fn centroid_similarities(labels: &[String], history: &[EmbeddingRecord], min_secs: f32) {
    use super::embedding_math::{average_and_normalize, dot};
    let mut by_label: std::collections::BTreeMap<&str, (Vec<Vec<f32>>, f32, f32)> =
        Default::default();
    for (l, r) in labels.iter().zip(history) {
        let e = by_label.entry(l.as_str()).or_default();
        e.0.push(r.embedding.clone());
        e.1 += r.duration_secs;
        if r.source == SegmentSource::Microphone {
            e.2 += r.duration_secs;
        }
    }
    let big: Vec<(&str, Vec<f32>, bool)> = by_label
        .into_iter()
        .filter(|(_, (_, secs, _))| *secs >= min_secs)
        .map(|(l, (es, secs, mic))| (l, average_and_normalize(&es), mic > secs / 2.0))
        .collect();
    for i in 0..big.len() {
        for j in (i + 1)..big.len() {
            if big[i].2 == big[j].2 {
                eprintln!(
                    "    sim {:<14} ~ {:<14} {:.2}{}",
                    big[i].0,
                    big[j].0,
                    dot(&big[i].1, &big[j].1),
                    if big[i].2 { " (mic)" } else { "" }
                );
            }
        }
    }
}

/// End-to-end check of the batch path: retranscribe one meeting of a
/// scratch copy (its DB *and* its recording folder — retranscription writes
/// transcripts.json/metadata.json next to the audio) and report speakers
/// and channel tags before and after. Run with `XDG_DATA_HOME` pointing at a
/// scratch copy of the data dir:
///
/// ```text
/// XDG_DATA_HOME=/scratch/xdg PARLEY_E2E_MEETING=<id> PARLEY_E2E_FOLDER=/scratch/rec \
/// PARLEY_E2E_MODEL=small \
/// cargo test -p parley-core --release retranscribe_scratch_copy -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "end-to-end on a scratch copy of real data; see doc comment"]
async fn retranscribe_scratch_copy() {
    let (Ok(meeting_id), Some(folder)) = (
        std::env::var("PARLEY_E2E_MEETING"),
        env_path("PARLEY_E2E_FOLDER"),
    ) else {
        eprintln!("set PARLEY_E2E_MEETING / PARLEY_E2E_FOLDER");
        return;
    };
    let data_dir = crate::paths::app_data_dir().unwrap();
    let real = dirs::home_dir()
        .unwrap()
        .join(".local/share/io.github.hankanman.Parley");
    assert_ne!(data_dir, real, "point XDG_DATA_HOME at a scratch copy");

    crate::speaker_diarization::model::set_models_dir();
    crate::whisper_engine::models::set_models_directory();
    crate::whisper_engine::models::whisper_init().await.unwrap();

    let opts = SqliteConnectOptions::from_str(&format!(
        "sqlite://{}",
        data_dir.join("meeting_minutes.sqlite").display()
    ))
    .unwrap();
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .unwrap();

    let summary = |pool: sqlx::SqlitePool, id: String| async move {
        let rows: Vec<(Option<String>, Option<String>, f64)> = sqlx::query_as(
            "SELECT speaker, source, COALESCE(audio_end_time - audio_start_time, 0)
             FROM transcripts WHERE meeting_id = ?",
        )
        .bind(id)
        .fetch_all(&pool)
        .await
        .unwrap();
        let mut by: std::collections::BTreeMap<String, (usize, f64)> = Default::default();
        let (mut mic, mut sys) = (0, 0);
        for (speaker, source, secs) in &rows {
            let e = by.entry(speaker.clone().unwrap_or_default()).or_default();
            e.0 += 1;
            e.1 += secs;
            match source.as_deref() {
                Some("mic") => mic += 1,
                Some("system") => sys += 1,
                _ => {}
            }
        }
        eprintln!(
            "  {} rows, {} speakers, source mic/system/none = {mic}/{sys}/{}",
            rows.len(),
            by.len(),
            rows.len() - mic - sys
        );
        for (speaker, (n, secs)) in by {
            eprintln!("    {speaker:<22} {n:>4} rows {secs:>7.1}s");
        }
    };

    eprintln!("before:");
    summary(pool.clone(), meeting_id.clone()).await;
    let model = std::env::var("PARLEY_E2E_MODEL").ok();
    let result = crate::audio::retranscription::start_retranscription_with(
        std::sync::Arc::new(crate::events::NullSink),
        Some(pool.clone()),
        meeting_id.clone(),
        folder.display().to_string(),
        None,
        model,
        None,
    )
    .await
    .unwrap();
    eprintln!("retranscribed: {} segments", result.segments_count);
    eprintln!("after:");
    summary(pool.clone(), meeting_id).await;
}
