use super::encode::encode_pcm_files_to_aac;
use super::recording_state::AudioChunk;
use anyhow::{anyhow, Result};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::ffmpeg::find_ffmpeg_path;

/// Checkpoints are raw interleaved little-endian f32 PCM (`.f32`), not AAC.
///
/// Encoding each 30 s checkpoint to AAC separately and merging with
/// `ffmpeg -f concat -c copy` inserted the encoder's priming/padding (~20–40
/// ms) at every boundary — an audible click every 30 s and, over a two-hour
/// meeting, several seconds of drift between the merged audio and the
/// sample-count-based transcript timestamps that per-segment playback seeks
/// by. Raw PCM concatenates losslessly and is encoded exactly once at
/// finalize (or recovery). It also removes the ffmpeg subprocess from the
/// live recording path entirely.
pub const CHECKPOINT_EXTENSION: &str = "f32";
/// Legacy per-checkpoint AAC files written by older builds; still merged on
/// recovery so a crash from before the format change stays recoverable.
const LEGACY_CHECKPOINT_EXTENSION: &str = "mp4";
/// Sidecar describing the PCM layout so recovery doesn't have to guess.
const CHECKPOINT_FORMAT_FILE: &str = "format.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointFormat {
    sample_rate: u32,
    channels: u16,
    /// Always `"f32le"` for now; present so a future change is detectable.
    encoding: String,
}

impl CheckpointFormat {
    fn write(&self, checkpoints_dir: &Path) -> Result<()> {
        let path = checkpoints_dir.join(CHECKPOINT_FORMAT_FILE);
        std::fs::write(&path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    /// Read the sidecar; falls back to the pipeline's stereo 48 kHz layout
    /// when it's missing (recovery of a folder whose sidecar write failed).
    fn read_or_default(checkpoints_dir: &Path) -> Self {
        let path = checkpoints_dir.join(CHECKPOINT_FORMAT_FILE);
        match std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CheckpointFormat>(&bytes).ok())
        {
            Some(format) if format.sample_rate > 0 && format.channels > 0 => format,
            _ => {
                warn!(
                    "Checkpoint format sidecar missing/invalid at {}; assuming 48 kHz stereo",
                    path.display()
                );
                CheckpointFormat {
                    sample_rate: 48_000,
                    channels: 2,
                    encoding: "f32le".to_string(),
                }
            }
        }
    }
}

/// Sorted list of checkpoint files with the given extension in `dir`.
fn list_checkpoint_files(dir: &Path, extension: &str) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().and_then(|s| s.to_str()) == Some(extension)
                && path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|stem| stem.starts_with("audio_chunk_"))
                    .unwrap_or(false)
        })
        .collect();
    // Names are zero-padded (`audio_chunk_000`), so lexical order is chunk order.
    files.sort();
    Ok(files)
}

/// Total PCM duration of a set of `.f32` checkpoint files, in seconds.
fn pcm_files_duration_secs(files: &[PathBuf], format: &CheckpointFormat) -> f64 {
    let bytes: u64 = files
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let bytes_per_second =
        format.sample_rate as f64 * format.channels as f64 * std::mem::size_of::<f32>() as f64;
    if bytes_per_second > 0.0 {
        bytes as f64 / bytes_per_second
    } else {
        0.0
    }
}

/// Audio data without device type (we only store mixed audio)
#[derive(Clone)]
struct AudioData {
    data: Vec<f32>,
    // sample_rate: u32,
}

/// Incremental audio saver that writes raw-PCM checkpoints every 30 seconds
/// to minimize memory usage and enable crash recovery
pub struct IncrementalAudioSaver {
    checkpoint_buffer: Vec<AudioData>,
    checkpoint_interval_samples: usize, // 30s of interleaved samples
    checkpoint_count: u32,
    checkpoints_dir: PathBuf,
    meeting_folder: PathBuf,
    sample_rate: u32,
    /// Interleaved channel count of the incoming chunks (2 = stereo mic/system).
    channels: u16,
}

impl IncrementalAudioSaver {
    /// Create a new incremental saver
    ///
    /// # Arguments
    /// * `meeting_folder` - Path to the meeting folder (contains .checkpoints/)
    /// * `sample_rate` - Sample rate of audio (typically 48000)
    /// * `channels` - Interleaved channel count of the chunks (2 for the
    ///   stereo mic-left/system-right recording; 1 for legacy mono callers)
    pub fn new(meeting_folder: PathBuf, sample_rate: u32, channels: u16) -> Result<Self> {
        let checkpoints_dir = meeting_folder.join(".checkpoints");

        // Verify checkpoints directory exists
        if !checkpoints_dir.exists() {
            return Err(anyhow!(
                "Checkpoints directory does not exist: {}",
                checkpoints_dir.display()
            ));
        }

        CheckpointFormat {
            sample_rate,
            channels,
            encoding: "f32le".to_string(),
        }
        .write(&checkpoints_dir)?;

        Ok(Self {
            checkpoint_buffer: Vec::new(),
            // Interleaved samples for 30s = sample_rate * 30 * channels.
            checkpoint_interval_samples: sample_rate as usize * 30 * channels as usize,
            checkpoint_count: 0,
            checkpoints_dir,
            meeting_folder,
            sample_rate,
            channels,
        })
    }

    /// Add an audio chunk to the buffer
    /// Automatically saves a checkpoint when buffer reaches 30 seconds
    pub fn add_chunk(&mut self, chunk: AudioChunk) -> Result<()> {
        let audio_data = AudioData {
            data: chunk.data,
            // sample_rate: chunk.sample_rate,
        };

        self.checkpoint_buffer.push(audio_data);

        // Calculate total samples in buffer
        let total_samples: usize = self.checkpoint_buffer.iter().map(|c| c.data.len()).sum();

        // Save checkpoint when buffer reaches threshold (30 seconds)
        if total_samples >= self.checkpoint_interval_samples {
            self.save_checkpoint()?;
            self.checkpoint_buffer.clear();
        }

        Ok(())
    }

    /// Save current buffer as a checkpoint file
    fn save_checkpoint(&mut self) -> Result<()> {
        // Concatenate all chunks in buffer
        let audio_data: Vec<f32> = self
            .checkpoint_buffer
            .iter()
            .flat_map(|c| &c.data)
            .cloned()
            .collect();

        if audio_data.is_empty() {
            warn!("Attempted to save empty checkpoint, skipping");
            return Ok(());
        }

        // Generate checkpoint filename
        let checkpoint_path = self.checkpoints_dir.join(format!(
            "audio_chunk_{:03}.{}",
            self.checkpoint_count, CHECKPOINT_EXTENSION
        ));

        // Raw interleaved f32le PCM: no subprocess, no codec delay, and a
        // partially written file is still recoverable up to the last frame.
        let bytes: &[u8] = bytemuck::cast_slice(&audio_data);
        let tmp_path = checkpoint_path.with_extension("f32.part");
        std::fs::write(&tmp_path, bytes)?;
        std::fs::rename(&tmp_path, &checkpoint_path)?;

        // `audio_data` is interleaved, so divide out the channel count to get
        // per-frame duration.
        let duration_seconds =
            audio_data.len() as f32 / (self.sample_rate as f32 * self.channels as f32);
        self.checkpoint_count += 1;

        info!(
            "Saved checkpoint {}: {:.2}s of audio ({} samples)",
            self.checkpoint_count,
            duration_seconds,
            audio_data.len()
        );

        Ok(())
    }

    /// Finalize the recording: save final checkpoint, merge all checkpoints, cleanup
    ///
    /// Returns the path to the final merged audio.mp4 file
    pub async fn finalize(&mut self) -> Result<PathBuf> {
        info!("Finalizing incremental recording...");

        // Save final buffer if not empty
        if !self.checkpoint_buffer.is_empty() {
            info!(
                "Saving final checkpoint with remaining {} chunks",
                self.checkpoint_buffer.len()
            );
            self.save_checkpoint()?;
            self.checkpoint_buffer.clear();
        }

        if self.checkpoint_count == 0 {
            return Err(anyhow!(
                "No audio checkpoints to merge - recording may have failed"
            ));
        }

        // One AAC encode over the concatenated PCM checkpoints.
        let final_audio_path = self.meeting_folder.join("audio.mp4");
        self.encode_checkpoints(&final_audio_path).await?;

        // Clean up checkpoints directory
        info!("Cleaning up {} checkpoint files", self.checkpoint_count);
        if let Err(e) = std::fs::remove_dir_all(&self.checkpoints_dir) {
            warn!("Failed to clean up checkpoints directory: {}", e);
            // Non-fatal - user can manually delete
        }

        info!("Finalized recording: {}", final_audio_path.display());

        Ok(final_audio_path)
    }

    /// Encode every `.f32` checkpoint, in order, into `output` in a single
    /// ffmpeg pass (streamed file by file, run off the async runtime).
    async fn encode_checkpoints(&self, output: &Path) -> Result<()> {
        let files = list_checkpoint_files(&self.checkpoints_dir, CHECKPOINT_EXTENSION)?;
        info!(
            "Encoding {} PCM checkpoints into final audio file...",
            files.len()
        );
        if files.len() != self.checkpoint_count as usize {
            warn!(
                "Expected {} checkpoint files, found {} — encoding what is on disk",
                self.checkpoint_count,
                files.len()
            );
        }
        if files.is_empty() {
            return Err(anyhow!(
                "No checkpoint files found in {}",
                self.checkpoints_dir.display()
            ));
        }

        let sample_rate = self.sample_rate;
        let channels = self.channels;
        let output_path = output.to_path_buf();
        tokio::task::spawn_blocking(move || {
            encode_pcm_files_to_aac(&files, sample_rate, channels, &output_path)
        })
        .await
        .map_err(|e| anyhow!("encode task panicked: {}", e))??;

        if !output.exists() {
            return Err(anyhow!(
                "Encoded audio file was not created: {}",
                output.display()
            ));
        }
        info!("Successfully encoded checkpoints → {}", output.display());
        Ok(())
    }

    /// Get the meeting folder path
    pub fn get_meeting_folder(&self) -> &PathBuf {
        &self.meeting_folder
    }

    /// Get current checkpoint count
    pub fn get_checkpoint_count(&self) -> u32 {
        self.checkpoint_count
    }
}

/// Audio recovery status for transcript recovery feature
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioRecoveryStatus {
    pub status: String, // "success" | "partial" | "failed" | "none"
    pub chunk_count: u32,
    pub estimated_duration_seconds: f64,
    pub audio_file_path: Option<String>,
    pub message: String,
}

/// Recover audio from checkpoint files
/// This is called by the transcript recovery system after a crash: PCM
/// checkpoints are encoded into `audio.mp4` in one pass; legacy AAC
/// checkpoints are stream-copy concatenated.
pub async fn recover_audio_from_checkpoints(
    meeting_folder: String,
    _sample_rate: u32,
) -> Result<AudioRecoveryStatus, String> {
    info!("Starting audio recovery for folder: {}", meeting_folder);

    let folder_path = PathBuf::from(&meeting_folder);
    let checkpoints_dir = folder_path.join(".checkpoints");

    // Check if checkpoints directory exists
    if !checkpoints_dir.exists() {
        info!(
            "No checkpoints directory found at: {}",
            checkpoints_dir.display()
        );
        return Ok(AudioRecoveryStatus {
            status: "none".to_string(),
            chunk_count: 0,
            estimated_duration_seconds: 0.0,
            audio_file_path: None,
            message: "No audio checkpoints found".to_string(),
        });
    }

    let pcm_files = list_checkpoint_files(&checkpoints_dir, CHECKPOINT_EXTENSION)
        .map_err(|e| format!("Failed to read checkpoints directory: {}", e))?;
    let legacy_files = list_checkpoint_files(&checkpoints_dir, LEGACY_CHECKPOINT_EXTENSION)
        .map_err(|e| format!("Failed to read checkpoints directory: {}", e))?;

    if pcm_files.is_empty() && legacy_files.is_empty() {
        info!(
            "No checkpoint files found in: {}",
            checkpoints_dir.display()
        );
        return Ok(AudioRecoveryStatus {
            status: "none".to_string(),
            chunk_count: 0,
            estimated_duration_seconds: 0.0,
            audio_file_path: None,
            message: "No audio checkpoint files found".to_string(),
        });
    }

    let output_path = folder_path.join("audio.mp4");
    let output_path_str = output_path
        .to_str()
        .ok_or("Invalid output path")?
        .to_string();

    // Current format: raw PCM checkpoints, one encode.
    if !pcm_files.is_empty() {
        let format = CheckpointFormat::read_or_default(&checkpoints_dir);
        let chunk_count = pcm_files.len() as u32;
        let estimated_duration = pcm_files_duration_secs(&pcm_files, &format);
        info!(
            "Found {} PCM checkpoint files ({} Hz, {} ch), duration: {:.2}s",
            chunk_count, format.sample_rate, format.channels, estimated_duration
        );

        let files = pcm_files.clone();
        let out = output_path.clone();
        let result = tokio::task::spawn_blocking(move || {
            encode_pcm_files_to_aac(&files, format.sample_rate, format.channels, &out)
        })
        .await
        .map_err(|e| format!("Recovery encode task panicked: {}", e))?;

        return Ok(match result {
            Ok(()) => {
                info!("Successfully recovered audio: {}", output_path_str);
                AudioRecoveryStatus {
                    status: "success".to_string(),
                    chunk_count,
                    estimated_duration_seconds: estimated_duration,
                    audio_file_path: Some(output_path_str),
                    message: format!("Successfully recovered {} audio checkpoints", chunk_count),
                }
            }
            Err(e) => {
                error!("Audio recovery encode failed: {}", e);
                AudioRecoveryStatus {
                    status: "failed".to_string(),
                    chunk_count,
                    estimated_duration_seconds: estimated_duration,
                    audio_file_path: None,
                    message: format!("FFmpeg failed: {}", e),
                }
            }
        });
    }

    // Legacy format: per-checkpoint AAC files from older builds. Stream-copy
    // concat (imperfect at the boundaries, but this is what those files allow).
    let chunk_count = legacy_files.len() as u32;
    let estimated_duration = (chunk_count as f64) * 30.0; // 30 seconds per chunk
    info!(
        "Found {} legacy AAC checkpoint files, estimated duration: {:.2}s",
        chunk_count, estimated_duration
    );

    let concat_file_path = checkpoints_dir.join("concat_list.txt");
    let mut concat_content = String::new();
    for path in &legacy_files {
        let path = path
            .canonicalize()
            .map_err(|e| format!("Failed to canonicalize path: {}", e))?;
        concat_content.push_str(&format!("file '{}'\n", path.display()));
    }
    std::fs::write(&concat_file_path, concat_content)
        .map_err(|e| format!("Failed to write concat file: {}", e))?;

    let ffmpeg_path = find_ffmpeg_path()
        .ok_or_else(|| "FFmpeg not found. Please install FFmpeg to recover audio.".to_string())?;
    info!("Using FFmpeg at: {:?}", ffmpeg_path);

    let concat_file_str = concat_file_path
        .to_str()
        .ok_or("Invalid concat list path")?
        .to_string();
    let output_for_cmd = output_path_str.clone();
    let ffmpeg_result = tokio::task::spawn_blocking(move || {
        std::process::Command::new(ffmpeg_path)
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
                &concat_file_str,
                "-c",
                "copy",
                "-y",
                &output_for_cmd,
            ])
            .stdin(std::process::Stdio::null())
            .output()
    })
    .await
    .map_err(|e| format!("Recovery merge task panicked: {}", e))?;

    match ffmpeg_result {
        Ok(output) if output.status.success() => {
            let _ = std::fs::remove_file(concat_file_path);
            info!("Successfully recovered audio: {}", output_path_str);
            Ok(AudioRecoveryStatus {
                status: "success".to_string(),
                chunk_count,
                estimated_duration_seconds: estimated_duration,
                audio_file_path: Some(output_path_str),
                message: format!("Successfully recovered {} audio chunks", chunk_count),
            })
        }
        Ok(output) => {
            let error = String::from_utf8_lossy(&output.stderr);
            error!("FFmpeg recovery failed: {}", error);
            Ok(AudioRecoveryStatus {
                status: "failed".to_string(),
                chunk_count,
                estimated_duration_seconds: estimated_duration,
                audio_file_path: None,
                message: format!("FFmpeg failed: {}", error),
            })
        }
        Err(e) => {
            error!("Failed to run FFmpeg: {}", e);
            Ok(AudioRecoveryStatus {
                status: "failed".to_string(),
                chunk_count,
                estimated_duration_seconds: estimated_duration,
                audio_file_path: None,
                message: format!("Failed to run FFmpeg: {}", e),
            })
        }
    }
}

/// Clean up checkpoint files after successful recording or recovery
/// This command is called by the frontend after successful save to clean up checkpoint files
pub async fn cleanup_checkpoints(meeting_folder: String) -> Result<(), String> {
    info!("Cleaning up checkpoints for folder: {}", meeting_folder);

    let folder_path = PathBuf::from(&meeting_folder);
    let checkpoints_dir = folder_path.join(".checkpoints");

    if checkpoints_dir.exists() {
        std::fs::remove_dir_all(&checkpoints_dir)
            .map_err(|e| format!("Failed to remove checkpoints directory: {}", e))?;
        info!("Successfully cleaned up checkpoints directory");
    } else {
        info!("No checkpoints directory to clean up");
    }

    Ok(())
}

/// Check if a meeting folder has audio checkpoint files
/// Returns true if .checkpoints/ directory exists and contains .mp4 files
pub async fn has_audio_checkpoints(meeting_folder: String) -> Result<bool, String> {
    let folder_path = PathBuf::from(&meeting_folder);
    let checkpoints_dir = folder_path.join(".checkpoints");

    // Check if checkpoints directory exists
    if !checkpoints_dir.exists() {
        return Ok(false);
    }

    // Current (.f32 PCM) or legacy (.mp4 AAC) checkpoint files.
    let has_files = std::fs::read_dir(&checkpoints_dir)
        .map_err(|e| format!("Failed to read checkpoints directory: {}", e))?
        .filter_map(|entry| entry.ok())
        .any(|entry| {
            matches!(
                entry.path().extension().and_then(|s| s.to_str()),
                Some(CHECKPOINT_EXTENSION) | Some(LEGACY_CHECKPOINT_EXTENSION)
            )
        });

    Ok(has_files)
}

#[cfg(test)]
mod tests {
    use super::super::recording_state::DeviceType;
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_checkpoint_creation() {
        // Create temp meeting folder
        let temp_dir = tempdir().unwrap();
        let meeting_folder = temp_dir.path().join("Test_Meeting");
        std::fs::create_dir_all(&meeting_folder).unwrap();
        std::fs::create_dir_all(meeting_folder.join(".checkpoints")).unwrap();

        let mut saver = IncrementalAudioSaver::new(meeting_folder.clone(), 48000, 1).unwrap();

        // Add 60 seconds worth of audio (should create 2 checkpoints)
        for i in 0..120 {
            // 120 chunks of 0.5s each
            let chunk = AudioChunk {
                data: vec![0.5f32; 24000], // 0.5s at 48kHz
                sample_rate: 48000,
                timestamp: i as f64 * 0.5, // timestamp in seconds
                chunk_id: i as u64,
                device_type: DeviceType::Microphone,
            };
            saver.add_chunk(chunk).unwrap();
        }

        // Verify 2 raw-PCM checkpoints created
        assert_eq!(saver.checkpoint_count, 2);
        let files = list_checkpoint_files(&meeting_folder.join(".checkpoints"), CHECKPOINT_EXTENSION)
            .unwrap();
        assert_eq!(files.len(), 2);
        // 30 s mono f32 @ 48 kHz per checkpoint.
        assert_eq!(std::fs::metadata(&files[0]).unwrap().len(), 48_000 * 30 * 4);
        let format = CheckpointFormat::read_or_default(&meeting_folder.join(".checkpoints"));
        assert_eq!((format.sample_rate, format.channels), (48_000, 1));

        if find_ffmpeg_path().is_none() {
            eprintln!("skipping finalize: ffmpeg not available");
            return;
        }

        // Finalize and verify the single-pass encode
        let final_path = saver.finalize().await.unwrap();
        assert!(final_path.exists());

        // Verify checkpoints directory deleted
        assert!(!meeting_folder.join(".checkpoints").exists());
    }

    #[tokio::test]
    async fn test_empty_recording() {
        let temp_dir = tempdir().unwrap();
        let meeting_folder = temp_dir.path().join("Empty_Test");
        std::fs::create_dir_all(&meeting_folder).unwrap();
        std::fs::create_dir_all(meeting_folder.join(".checkpoints")).unwrap();

        let mut saver = IncrementalAudioSaver::new(meeting_folder.clone(), 48000, 1).unwrap();

        // Try to finalize without adding any chunks
        let result = saver.finalize().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No audio checkpoints"));
    }
}
