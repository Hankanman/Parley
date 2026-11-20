//! Per-segment audio clip playback for the meeting-details transcript.
//!
//! Extracts a `[start, end]` slice of a meeting's recording as a small WAV so
//! the user can listen and verify who's speaking. Decoding uses the bundled
//! ffmpeg rather than the webview's media stack: the AppImage's bundled
//! WebKit/GStreamer ships no plugins, so both `<audio>` and the Web Audio API
//! fail with "element appsink not found". We therefore play the extracted PCM
//! natively (see [`crate::audio::playback`]) rather than in the webview.

use crate::audio::ffmpeg::find_ffmpeg_path;

/// Upper bound on clip length, guarding against a bogus range producing a huge
/// extraction. Real transcript segments are seconds long.
const MAX_CLIP_SECS: f64 = 120.0;

/// The meeting's recording folder. Selects just that column rather than a
/// whole `MeetingModel`: a hand-picked column list decoded into the model
/// broke playback ("no column found for name: status") once the model grew.
async fn recording_folder(pool: &sqlx::SqlitePool, meeting_id: &str) -> Result<String, String> {
    let folder: Option<Option<String>> =
        sqlx::query_scalar("SELECT folder_path FROM meetings WHERE id = ?")
            .bind(meeting_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("Database error: {}", e))?;
    folder
        .flatten()
        .filter(|f| !f.is_empty())
        .ok_or_else(|| "This meeting has no saved recording.".to_string())
}

/// Extract `[start_secs, end_secs]` of a meeting's recording as WAV bytes
/// (mono 16 kHz PCM s16le). Shared by the native-playback command and the
/// legacy base64 command. Errors with a user-facing message when the meeting
/// has no saved recording.
pub async fn extract_clip_wav(
    pool: &sqlx::SqlitePool,
    meeting_id: &str,
    start_secs: f64,
    end_secs: f64,
    source: Option<&str>,
) -> Result<Vec<u8>, String> {
    let folder_path = recording_folder(pool, meeting_id).await?;

    // Imports retain their original container (e.g. audio.mkv or audio.wav).
    // Use the same lookup as re-transcription and speaker re-detection.
    let audio_path = super::retranscription::find_audio_file(std::path::Path::new(&folder_path))
        .map_err(|_| "No audio recording found for this meeting.".to_string())?;

    let ffmpeg = find_ffmpeg_path().ok_or_else(|| "ffmpeg is not available.".to_string())?;

    let start = start_secs.max(0.0);
    let dur = (end_secs - start_secs).clamp(0.05, MAX_CLIP_SECS);

    // Extract to a temp WAV *file* (not a pipe): ffmpeg can then seek back and
    // write a correct RIFF size header, which some decoders require. Mono
    // 16 kHz PCM is ample for an ear check and keeps the payload small.
    let tmp = tempfile::Builder::new()
        .prefix("parley-clip-")
        .suffix(".wav")
        .tempfile()
        .map_err(|e| format!("Failed to create temp file: {}", e))?;
    let tmp_path = tmp.path().to_path_buf();

    let audio_path_str = audio_path.to_string_lossy().into_owned();
    let tmp_path_str = tmp_path.to_string_lossy().into_owned();

    // Recordings are stereo (mic = left, system = right). Pick the channel that
    // matches the segment's source so a remote speaker plays back from the
    // clean system channel and the local user from the mic — no mic bleed and
    // no cross-talk. Unknown source (or a legacy mono file) downmixes instead.
    let channel: Option<u8> = match source {
        Some("mic") => Some(0),
        Some("system") => Some(1),
        _ => None,
    };

    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let run = |select: Option<u8>| -> std::io::Result<bool> {
            let mut cmd = std::process::Command::new(&ffmpeg);
            cmd.args([
                "-nostdin",
                "-loglevel",
                "error",
                "-y",
                // Seek before -i for a fast input seek; -t after -i bounds the
                // output duration from that point.
                "-ss",
                &format!("{:.3}", start),
                "-i",
                &audio_path_str,
                "-t",
                &format!("{:.3}", dur),
            ]);
            // Isolate one channel to mono, or downmix all channels to mono.
            match select {
                Some(idx) => {
                    cmd.args(["-af", &format!("pan=mono|c0=c{idx}")]);
                }
                None => {
                    cmd.args(["-ac", "1"]);
                }
            }
            cmd.args([
                "-vn",
                "-ar",
                "16000",
                "-c:a",
                "pcm_s16le",
                "-f",
                "wav",
                &tmp_path_str,
            ]);
            Ok(cmd.status()?.success())
        };

        // Try the requested source channel; fall back to a mono downmix if it
        // fails — e.g. a legacy mono recording that has no right channel.
        let ok = run(channel).map_err(|e| format!("Failed to run ffmpeg: {}", e))?;
        if !ok {
            let fell_back = channel.is_some()
                && run(None).map_err(|e| format!("Failed to run ffmpeg: {}", e))?;
            if !fell_back {
                return Err("Failed to extract the audio clip.".to_string());
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("ffmpeg task failed: {}", e))??;

    let bytes = std::fs::read(&tmp_path).map_err(|e| format!("Failed to read clip: {}", e))?;
    if bytes.is_empty() {
        return Err("The extracted clip was empty.".to_string());
    }

    Ok(bytes)
}

/// Minimal reader for the PCM WAV clips we generate (RIFF/WAVE, integer PCM,
/// 16-bit). Scans chunks rather than assuming a fixed 44-byte header, since
/// ffmpeg may interleave extra metadata chunks. Returns the interleaved i16
/// samples plus sample rate and channel count.
pub fn parse_wav_pcm16(bytes: &[u8]) -> Result<(Vec<i16>, u32, u16), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("Clip is not a WAV file.".to_string());
    }

    let mut fmt: Option<(u16, u16, u32)> = None; // (format_tag, channels, sample_rate)
    let mut bits_per_sample: u16 = 16;
    let mut data: Option<&[u8]> = None;

    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body_start = pos + 8;
        let body_end = body_start.saturating_add(size).min(bytes.len());

        match id {
            b"fmt " if body_end - body_start >= 16 => {
                let b = &bytes[body_start..body_end];
                fmt = Some((
                    u16::from_le_bytes([b[0], b[1]]),
                    u16::from_le_bytes([b[2], b[3]]),
                    u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
                ));
                bits_per_sample = u16::from_le_bytes([b[14], b[15]]);
            }
            b"data" => data = Some(&bytes[body_start..body_end]),
            _ => {}
        }

        // Chunks are word-aligned: an odd size is padded with a trailing byte.
        pos = body_start + size + (size & 1);
    }

    let (format_tag, channels, sample_rate) = fmt.ok_or("WAV is missing its fmt chunk.")?;
    let data = data.ok_or("WAV is missing its data chunk.")?;
    if format_tag != 1 || bits_per_sample != 16 {
        return Err(format!(
            "Unsupported clip format (tag={format_tag}, bits={bits_per_sample})."
        ));
    }

    let samples = data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok((samples, sample_rate, channels))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> sqlx::SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        pool
    }

    #[tokio::test]
    async fn recording_folder_reads_the_current_schema() {
        let pool = test_pool().await;
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at, folder_path) \
             VALUES ('m1', 'With audio', datetime('now'), datetime('now'), '/rec/m1'), \
                    ('m2', 'No audio', datetime('now'), datetime('now'), NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(recording_folder(&pool, "m1").await.unwrap(), "/rec/m1");
        assert_eq!(
            recording_folder(&pool, "m2").await.unwrap_err(),
            "This meeting has no saved recording."
        );
        assert_eq!(
            recording_folder(&pool, "missing").await.unwrap_err(),
            "This meeting has no saved recording."
        );
    }

    #[tokio::test]
    async fn missing_media_keeps_the_recording_error() {
        let pool = test_pool().await;
        let folder = tempfile::tempdir().unwrap();
        sqlx::query("INSERT INTO meetings (id, title, created_at, updated_at, folder_path) VALUES ('missing-media', 'Missing media', 'x', 'x', ?)")
            .bind(folder.path().to_string_lossy().as_ref()).execute(&pool).await.unwrap();
        assert_eq!(
            extract_clip_wav(&pool, "missing-media", 0.0, 1.0, None)
                .await
                .unwrap_err(),
            "No audio recording found for this meeting."
        );
    }

    #[tokio::test]
    #[ignore = "requires FFmpeg with lavfi test sources and standard audio/video encoders"]
    async fn imported_media_extracts_the_requested_transcript_interval() {
        let ffmpeg = find_ffmpeg_path().expect("FFmpeg is required for this integration test");
        let pool = test_pool().await;
        let root = tempfile::tempdir().unwrap();
        for (extension, video_codec, audio_codec) in [
            ("mkv", Some("ffv1"), "pcm_s16le"),
            ("mp4", Some("mpeg4"), "aac"),
            ("wav", None, "pcm_s16le"),
        ] {
            let folder = root.path().join(extension);
            std::fs::create_dir(&folder).unwrap();
            let path = folder.join(format!("audio.{extension}"));
            // One second of silence followed by a tone. A wrong seek can
            // produce a valid WAV, so check the actual decoded samples too.
            let mut command = std::process::Command::new(&ffmpeg);
            command.args([
                "-nostdin",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=16x16:r=5:d=2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=16000:duration=2",
                "-af",
                "volume=0:enable='lt(t,1)'",
            ]);
            if let Some(codec) = video_codec {
                command.args(["-c:v", codec]);
            } else {
                command.arg("-vn");
            }
            let generated = command
                .args(["-c:a", audio_codec, "-shortest"])
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                generated.status.success(),
                "fixture: {}",
                String::from_utf8_lossy(&generated.stderr)
            );

            sqlx::query("INSERT INTO meetings (id, title, created_at, updated_at, folder_path) VALUES (?, 'Imported media', 'x', 'x', ?)")
                .bind(extension).bind(folder.to_string_lossy().as_ref()).execute(&pool).await.unwrap();
            for (start, expected_tone) in [(0.1, false), (1.2, true)] {
                let bytes = extract_clip_wav(&pool, extension, start, start + 0.5, None)
                    .await
                    .unwrap();
                let (samples, rate, channels) = parse_wav_pcm16(&bytes).unwrap();
                assert_eq!((rate, channels), (16000, 1));
                let duration = samples.len() as f64 / rate as f64;
                assert!(
                    (0.45..=0.55).contains(&duration),
                    "{extension}: {duration}s"
                );
                let rms = (samples.iter().map(|s| (*s as f64).powi(2)).sum::<f64>()
                    / samples.len() as f64)
                    .sqrt();
                if expected_tone {
                    assert!(rms > 500.0, "{extension}: expected tone, RMS={rms}");
                } else {
                    assert!(rms < 10.0, "{extension}: expected silence, RMS={rms}");
                }
            }
        }
    }
}
