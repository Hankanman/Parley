use super::ffmpeg::find_ffmpeg_path;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use tracing::{debug, error, warn};

/// AAC-LC in MP4, 192 kbps — the recording's final on-disk format.
const AAC_BITRATE: &str = "192k";

/// Spawn an ffmpeg process that reads interleaved little-endian f32 PCM from
/// stdin and writes an AAC/MP4 file. stdout is discarded and stderr is
/// captured on its own thread so ffmpeg can never block on a full pipe while
/// we are still writing its input.
fn spawn_pcm_to_aac(
    sample_rate: u32,
    channels: u16,
    output_path: &Path,
) -> anyhow::Result<(Child, std::thread::JoinHandle<String>)> {
    let ffmpeg_path = find_ffmpeg_path().ok_or_else(|| {
        anyhow::anyhow!("FFmpeg not found. Please install FFmpeg to save recordings.")
    })?;
    let output = output_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("output path is not valid UTF-8: {}", output_path.display()))?;

    let mut command = Command::new(ffmpeg_path);
    command
        .args([
            "-nostdin",
            "-hide_banner",
            "-nostats",
            "-loglevel",
            "error",
            "-f",
            "f32le",
            "-ar",
            &sample_rate.to_string(),
            "-ac",
            &channels.to_string(),
            "-i",
            "pipe:0",
            "-c:a",
            "aac",
            "-b:a",
            AAC_BITRATE,
            "-profile:a",
            "aac_low", // AAC-LC for broad compatibility
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
            "-y",
            output,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    debug!("FFmpeg command: {:?}", command);

    let mut child = command
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn FFmpeg: {}", e))?;

    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture FFmpeg stderr"))?;
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });

    Ok((child, stderr_reader))
}

/// Wait for ffmpeg, always reaping the child, and turn a non-zero exit into
/// an error carrying its stderr.
fn finish_ffmpeg(
    mut child: Child,
    stderr_reader: std::thread::JoinHandle<String>,
    write_result: std::io::Result<()>,
) -> anyhow::Result<()> {
    // Close stdin (if still open) so ffmpeg sees EOF and finalises the file.
    drop(child.stdin.take());
    let status = child.wait();
    let stderr = stderr_reader.join().unwrap_or_default();

    if let Err(e) = write_result {
        error!("Writing PCM to FFmpeg failed: {} (stderr: {})", e, stderr.trim());
        return Err(anyhow::anyhow!(
            "failed to feed audio to FFmpeg: {} ({})",
            e,
            stderr.trim()
        ));
    }
    let status = status.map_err(|e| anyhow::anyhow!("failed to wait for FFmpeg: {}", e))?;
    if !status.success() {
        error!("FFmpeg failed with status {}: {}", status, stderr.trim());
        return Err(anyhow::anyhow!(
            "FFmpeg process failed with status {}: {}",
            status,
            stderr.trim()
        ));
    }
    if !stderr.trim().is_empty() {
        warn!("FFmpeg stderr: {}", stderr.trim());
    }
    Ok(())
}

/// Encode a sequence of raw interleaved f32le PCM files (the recording's
/// checkpoint format) into a single AAC/MP4, streaming file by file so a
/// multi-hour recording is never held in memory.
///
/// The files are concatenated at the PCM level and encoded in one pass, so
/// there are no per-checkpoint encoder priming/padding gaps — the output's
/// timeline matches the sample count the transcript timestamps were computed
/// from. A trailing partial frame in the last file (a checkpoint cut short
/// by a crash) is dropped rather than shifting channel alignment.
pub fn encode_pcm_files_to_aac(
    inputs: &[PathBuf],
    sample_rate: u32,
    channels: u16,
    output_path: &Path,
) -> anyhow::Result<()> {
    if inputs.is_empty() {
        return Err(anyhow::anyhow!("No checkpoint files provided for encoding"));
    }
    let frame_bytes = std::mem::size_of::<f32>() * channels.max(1) as usize;

    let (mut child, stderr_reader) = spawn_pcm_to_aac(sample_rate, channels, output_path)?;
    let write_result = (|| -> std::io::Result<()> {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("failed to open FFmpeg stdin"))?;
        let mut buf = vec![0u8; 1 << 20];
        let mut carry: Vec<u8> = Vec::new();
        for path in inputs {
            let mut file = File::open(path)?;
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                carry.extend_from_slice(&buf[..n]);
                let whole = carry.len() - carry.len() % frame_bytes;
                stdin.write_all(&carry[..whole])?;
                carry.drain(..whole);
            }
        }
        if !carry.is_empty() {
            warn!(
                "Dropping {} trailing bytes that do not form a whole frame",
                carry.len()
            );
        }
        stdin.flush()?;
        drop(stdin);
        Ok(())
    })();
    finish_ffmpeg(child, stderr_reader, write_result)
}
