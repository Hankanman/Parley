//! Per-segment audio clip playback for the meeting-details transcript.
//!
//! Extracts a `[start, end]` slice of a meeting's recording as a small WAV so
//! the user can listen and verify who's speaking. Decoding uses the bundled
//! ffmpeg rather than the webview's media stack: the AppImage's bundled
//! WebKit/GStreamer ships no plugins, so both `<audio>` and the Web Audio API
//! fail with "element appsink not found". We therefore play the extracted PCM
//! natively (see [`crate::audio::playback`]) rather than in the webview.

use crate::audio::ffmpeg::find_ffmpeg_path;
use crate::database::models::MeetingModel;

/// Upper bound on clip length, guarding against a bogus range producing a huge
/// extraction. Real transcript segments are seconds long.
const MAX_CLIP_SECS: f64 = 120.0;

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
    let meeting: Option<MeetingModel> = sqlx::query_as(
        "SELECT id, title, created_at, updated_at, folder_path FROM meetings WHERE id = ?",
    )
    .bind(meeting_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("Database error: {}", e))?;

    let folder_path = meeting
        .and_then(|m| m.folder_path)
        .ok_or_else(|| "This meeting has no saved recording.".to_string())?;

    let audio_path = std::path::Path::new(&folder_path).join("audio.mp4");
    if !audio_path.is_file() {
        return Err("No audio recording found for this meeting.".to_string());
    }

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
                "-ar", "16000", "-c:a", "pcm_s16le", "-f", "wav", &tmp_path_str,
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
        let size = u32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]])
            as usize;
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
