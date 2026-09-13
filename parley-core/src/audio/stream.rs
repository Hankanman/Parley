//! Capture stream management on top of the native PipeWire layer.
//!
//! Every stream delivers interleaved f32 @ 48 kHz stereo (the PipeWire
//! graph negotiates/resamples), feeding `AudioCapture` → pipeline
//! unchanged. Device identity is the PipeWire `node.name` (or the
//! literal `"default"` for the system default source/sink).

use anyhow::{Context as _, Result};
use log::{error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::devices::AudioDevice;
use super::pipeline::AudioCapture;
use super::pw::{
    CaptureTarget, PwCaptureStream, PwDevice, PwDeviceKind, PwStreamEvent, CAPTURE_CHANNELS,
    CAPTURE_RATE,
};
use super::recording_state::{AudioError, DeviceType, RecordingState};

/// Translate a selected device into a PipeWire capture target.
pub fn capture_target_for(device_id: &str, device_type: DeviceType) -> CaptureTarget {
    match (device_id, device_type) {
        ("default", DeviceType::Microphone) => CaptureTarget::DefaultMicrophone,
        ("default", DeviceType::System) => CaptureTarget::DefaultSystem,
        (id, DeviceType::Microphone) => CaptureTarget::Node {
            id: id.to_string(),
            kind: PwDeviceKind::Microphone,
        },
        (id, DeviceType::System) => CaptureTarget::Node {
            id: id.to_string(),
            kind: PwDeviceKind::System,
        },
    }
}

fn pw_kind_for(device_type: DeviceType) -> PwDeviceKind {
    match device_type {
        DeviceType::Microphone => PwDeviceKind::Microphone,
        DeviceType::System => PwDeviceKind::System,
    }
}

/// Whether an explicitly-chosen device id should be kept as-is, given the
/// devices currently visible on the PipeWire registry. Pure/testable: the
/// literal `"default"` is always kept (PipeWire resolves it dynamically),
/// anything else must be present in `available` with a matching kind.
fn device_id_is_valid(id: &str, kind: PwDeviceKind, available: &[PwDevice]) -> bool {
    id == "default" || available.iter().any(|d| d.id == id && d.kind == kind)
}

/// Validate an explicitly-selected device against the current PipeWire
/// registry, falling back to `"default"` (with a warning) if it's stale
/// (e.g. the device was unplugged since the frontend's picker was
/// populated). `"default"` is always accepted without a roundtrip.
///
/// Enumeration failures are not treated as validation failures — we don't
/// want a flaky/slow registry roundtrip to block recording start; if the
/// device id really is bad, `PwCaptureStream::open` will surface that.
async fn validate_or_fallback(device: Arc<AudioDevice>, device_type: DeviceType) -> Arc<AudioDevice> {
    if device.name == "default" {
        return device;
    }

    let kind = pw_kind_for(device_type);
    let id = device.name.clone();
    let devices = tokio::task::spawn_blocking(super::pw::enumerate_devices).await;

    match devices {
        Ok(Ok(devices)) => {
            if device_id_is_valid(&id, kind, &devices) {
                device
            } else {
                warn!(
                    "Requested {:?} device '{}' is not present in the current PipeWire registry \
                     (likely unplugged/stale); falling back to the default device",
                    device_type, id
                );
                Arc::new(AudioDevice::new(
                    "default".to_string(),
                    device.device_type.clone(),
                ))
            }
        }
        Ok(Err(e)) => {
            warn!(
                "Could not validate {:?} device '{}' against the PipeWire registry ({}); \
                 proceeding with the requested id",
                device_type, id, e
            );
            device
        }
        Err(e) => {
            warn!(
                "Device validation task for {:?} device '{}' failed ({}); proceeding with the \
                 requested id",
                device_type, id, e
            );
            device
        }
    }
}

/// A running capture stream bound to the recording pipeline.
pub struct AudioStream {
    device: Arc<AudioDevice>,
    stream: PwCaptureStream,
    /// Cleared from the loop thread when the underlying PipeWire stream
    /// dies (error, device disconnect, or clean shutdown) so callers can
    /// tell a stream that "exists" from one that is actually delivering
    /// audio.
    active: Arc<AtomicBool>,
}

impl AudioStream {
    pub async fn create(
        device: Arc<AudioDevice>,
        state: Arc<RecordingState>,
        device_type: DeviceType,
    ) -> Result<Self> {
        info!(
            "🎵 Stream: opening PipeWire capture for '{}' ({:?})",
            device.name, device_type
        );

        let event_state = state.clone();
        let capture = AudioCapture::new(
            device.clone(),
            state,
            CAPTURE_RATE,
            CAPTURE_CHANNELS as u16,
            device_type,
        );

        let target = capture_target_for(&device.name, device_type);
        let active = Arc::new(AtomicBool::new(true));
        let active_for_cb = active.clone();
        let device_name_for_cb = device.name.clone();

        // PwCaptureStream::open blocks synchronously on a PipeWire
        // roundtrip (up to 5s on a stalled daemon) — keep it off the
        // tokio runtime.
        let stream = tokio::task::spawn_blocking(move || {
            let mut capture = capture;
            PwCaptureStream::open(
                target,
                Box::new(move |samples| capture.process_audio_data(samples)),
                Box::new(move |event| match event {
                    PwStreamEvent::Error(msg) => {
                        error!(
                            "pw: {:?} capture stream '{}' failed: {}",
                            device_type, device_name_for_cb, msg
                        );
                        active_for_cb.store(false, Ordering::SeqCst);
                        event_state.report_error(AudioError::StreamFailed);
                    }
                    PwStreamEvent::Disconnected => {
                        warn!(
                            "pw: {:?} capture device '{}' disconnected",
                            device_type, device_name_for_cb
                        );
                        active_for_cb.store(false, Ordering::SeqCst);
                        event_state.report_error(AudioError::DeviceDisconnected);
                    }
                    PwStreamEvent::Ended => {
                        active_for_cb.store(false, Ordering::SeqCst);
                    }
                }),
            )
        })
        .await
        .context("capture open task panicked")??;

        Ok(Self {
            device,
            stream,
            active,
        })
    }

    pub fn device(&self) -> &AudioDevice {
        &self.device
    }

    /// Whether the underlying PipeWire stream is still believed to be
    /// alive (i.e. hasn't reported `Error`/`Disconnected`/`Ended`).
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// Stop the stream, blocking until the loop thread has joined.
    /// Synchronous — callers on an async runtime should run this inside
    /// `spawn_blocking` (or a detached thread, as `AudioStreamManager`'s
    /// `Drop` does) rather than calling it inline.
    pub fn stop(self) -> Result<()> {
        info!("Stopping audio stream for device: {}", self.device.name);
        self.stream.stop();
        Ok(())
    }
}

/// Manages the microphone + system capture stream pair.
pub struct AudioStreamManager {
    microphone_stream: Option<AudioStream>,
    system_stream: Option<AudioStream>,
    state: Arc<RecordingState>,
}

impl AudioStreamManager {
    pub fn new(state: Arc<RecordingState>) -> Self {
        Self {
            microphone_stream: None,
            system_stream: None,
            state,
        }
    }

    /// Start audio streams for the given devices.
    ///
    /// Explicit (non-`"default"`) device ids are validated against the
    /// current PipeWire registry first and fall back to `"default"` if
    /// stale. The microphone and system streams are then opened
    /// concurrently, so a slow/stalled device doesn't serialize startup
    /// latency with the other.
    pub async fn start_streams(
        &mut self,
        microphone_device: Option<Arc<AudioDevice>>,
        system_device: Option<Arc<AudioDevice>>,
    ) -> Result<()> {
        let microphone_device = match microphone_device {
            Some(d) => Some(validate_or_fallback(d, DeviceType::Microphone).await),
            None => None,
        };
        let system_device = match system_device {
            Some(d) => Some(validate_or_fallback(d, DeviceType::System).await),
            None => None,
        };

        let mic_state = self.state.clone();
        let sys_state = self.state.clone();

        let mic_fut = async move {
            match microphone_device {
                Some(mic_device) => {
                    info!("🎤 Creating microphone stream: {}", mic_device.name);
                    let result =
                        AudioStream::create(mic_device.clone(), mic_state, DeviceType::Microphone)
                            .await;
                    Some(result.map(|stream| (mic_device, stream)))
                }
                None => None,
            }
        };
        let sys_fut = async move {
            match system_device {
                Some(sys_device) => {
                    info!("🔊 Creating system audio stream: {}", sys_device.name);
                    let result =
                        AudioStream::create(sys_device.clone(), sys_state, DeviceType::System)
                            .await;
                    Some(result.map(|stream| (sys_device, stream)))
                }
                None => None,
            }
        };

        let (mic_result, sys_result) = tokio::join!(mic_fut, sys_fut);

        match mic_result {
            Some(Ok((mic_device, stream))) => {
                self.state.set_microphone_device(mic_device);
                self.microphone_stream = Some(stream);
                info!("✅ Microphone stream created successfully");
            }
            Some(Err(e)) => {
                error!("❌ Failed to create microphone stream: {}", e);
                return Err(e);
            }
            None => info!("ℹ️ No microphone device specified, skipping microphone stream"),
        }

        match sys_result {
            Some(Ok((sys_device, stream))) => {
                self.state.set_system_device(sys_device);
                self.system_stream = Some(stream);
                info!("✅ System audio stream created successfully");
            }
            Some(Err(e)) => {
                // Don't fail the whole recording if only system audio fails.
                error!("⚠️ Failed to create system audio stream: {}", e);
            }
            None => info!("ℹ️ No system device specified, skipping system audio stream"),
        }

        if self.microphone_stream.is_none() && self.system_stream.is_none() {
            return Err(anyhow::anyhow!("No audio streams could be created"));
        }

        Ok(())
    }

    /// Stop all audio streams, joining their loop threads off the tokio
    /// runtime (concurrently) so shutdown doesn't stall a worker thread.
    pub async fn stop_streams(&mut self) -> Result<()> {
        info!("Stopping all audio streams");

        let mic = self.microphone_stream.take();
        let sys = self.system_stream.take();

        let mic_fut = async {
            match mic {
                Some(s) => tokio::task::spawn_blocking(move || s.stop())
                    .await
                    .context("microphone stream stop task panicked")?,
                None => Ok(()),
            }
        };
        let sys_fut = async {
            match sys {
                Some(s) => tokio::task::spawn_blocking(move || s.stop())
                    .await
                    .context("system stream stop task panicked")?,
                None => Ok(()),
            }
        };

        let (mic_res, sys_res) = tokio::join!(mic_fut, sys_fut);
        mic_res?;
        sys_res?;

        info!("All audio streams stopped");
        Ok(())
    }

    pub fn active_stream_count(&self) -> usize {
        self.microphone_stream.as_ref().map(AudioStream::is_active).unwrap_or(false) as usize
            + self.system_stream.as_ref().map(AudioStream::is_active).unwrap_or(false) as usize
    }

    pub fn has_active_streams(&self) -> bool {
        self.active_stream_count() > 0
    }
}

impl Drop for AudioStreamManager {
    fn drop(&mut self) {
        let mic = self.microphone_stream.take();
        let sys = self.system_stream.take();
        if mic.is_none() && sys.is_none() {
            return;
        }
        // Stopping a stream joins its loop thread. Do that on a detached
        // thread rather than blocking whoever is dropping this manager
        // (often a tokio worker thread).
        let spawned = std::thread::Builder::new()
            .name("pw-stream-drop".into())
            .spawn(move || {
                if let Some(s) = mic {
                    if let Err(e) = s.stop() {
                        error!("Error stopping microphone stream during drop: {}", e);
                    }
                }
                if let Some(s) = sys {
                    if let Err(e) = s.stop() {
                        error!("Error stopping system stream during drop: {}", e);
                    }
                }
            });
        if let Err(e) = spawned {
            error!("Failed to spawn stream-drop thread: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, kind: PwDeviceKind) -> PwDevice {
        PwDevice {
            id: id.to_string(),
            label: id.to_string(),
            kind,
        }
    }

    #[test]
    fn default_is_always_valid_even_with_empty_registry() {
        assert!(device_id_is_valid("default", PwDeviceKind::Microphone, &[]));
    }

    #[test]
    fn known_device_id_with_matching_kind_is_valid() {
        let devices = vec![
            dev("alsa_input.usb-foo", PwDeviceKind::Microphone),
            dev("alsa_output.pci-bar", PwDeviceKind::System),
        ];
        assert!(device_id_is_valid(
            "alsa_input.usb-foo",
            PwDeviceKind::Microphone,
            &devices
        ));
        assert!(device_id_is_valid(
            "alsa_output.pci-bar",
            PwDeviceKind::System,
            &devices
        ));
    }

    #[test]
    fn stale_device_id_is_invalid() {
        let devices = vec![dev("alsa_input.usb-foo", PwDeviceKind::Microphone)];
        assert!(!device_id_is_valid(
            "alsa_input.usb-unplugged",
            PwDeviceKind::Microphone,
            &devices
        ));
    }

    #[test]
    fn device_id_present_with_wrong_kind_is_invalid() {
        // e.g. a node that is a sink but was requested as a microphone.
        let devices = vec![dev("alsa_output.pci-bar", PwDeviceKind::System)];
        assert!(!device_id_is_valid(
            "alsa_output.pci-bar",
            PwDeviceKind::Microphone,
            &devices
        ));
    }
}
