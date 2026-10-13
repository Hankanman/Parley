//! Real-time RMS/peak meter for the recording page, on native
//! PipeWire capture streams.
//!
//! The frontend asks to monitor a microphone and/or a system-audio
//! device by PipeWire node id (or `"default"`). Levels are emitted via
//! the `audio-levels` event, keyed by **role** (`"mic"` / `"system"`)
//! — the UI renders exactly one meter per role, so role keys avoid
//! making the frontend track device-id changes.

use anyhow::Result;
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::pw::{PwCaptureStream, PwStreamEvent};
use super::recording_state::DeviceType;
use super::stream::capture_target_for;
use crate::events::{EventSinkExt, SharedEventSink};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AudioLevelData {
    /// Role key: `"mic"` or `"system"`.
    pub device_name: String,
    pub device_type: String,
    pub rms_level: f32,
    pub peak_level: f32,
    pub is_active: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AudioLevelUpdate {
    pub timestamp: u64,
    pub levels: Vec<AudioLevelData>,
}

/// Accumulates audio over one emit interval so the reported RMS/peak
/// describe the whole period rather than a single capture buffer.
#[derive(Debug, Default, Clone, Copy)]
struct LevelAccumulator {
    sum_sq: f64,
    count: u64,
    peak: f32,
    /// A buffer has ever arrived for this role (so silence periods with no
    /// buffers still emit a zero level instead of freezing the UI).
    seen: bool,
}

impl LevelAccumulator {
    fn push(&mut self, samples: &[f32]) {
        let mut sum = 0.0_f32;
        let mut peak = self.peak;
        for &x in samples {
            sum += x * x;
            peak = peak.max(x.abs());
        }
        self.sum_sq += sum as f64;
        self.count += samples.len() as u64;
        self.peak = peak;
        self.seen = true;
    }

    /// Level over the accumulated period, resetting the period. `None` if
    /// this role never produced audio.
    fn take(&mut self, role: &str, type_label: &str) -> Option<AudioLevelData> {
        if !self.seen {
            return None;
        }
        let rms = if self.count > 0 {
            (self.sum_sq / self.count as f64).sqrt() as f32
        } else {
            0.0
        };
        let peak = self.peak;
        self.sum_sq = 0.0;
        self.count = 0;
        self.peak = 0.0;
        Some(AudioLevelData {
            device_name: role.to_string(),
            device_type: type_label.to_string(),
            rms_level: rms.min(1.0),
            peak_level: peak.min(1.0),
            is_active: rms > 0.001,
        })
    }
}

/// Emit rate for `audio-levels` (~30 Hz).
const EMIT_INTERVAL_MS: u64 = 33;

/// Monotonically increasing generation; bumping it invalidates the
/// running emit task and any streams from a prior start.
static GENERATION: AtomicU64 = AtomicU64::new(0);

struct MonitorSession {
    streams: Vec<PwCaptureStream>,
}

static SESSION: OnceLock<Mutex<Option<MonitorSession>>> = OnceLock::new();

fn session_slot() -> &'static Mutex<Option<MonitorSession>> {
    SESSION.get_or_init(|| Mutex::new(None))
}

/// Pure decision at the heart of the generation race fix: is a call that
/// captured `call_generation` (at its start) still the most recent one,
/// given the generation counter's current value `current_generation`?
/// A call is current only on an exact match — any later start or stop
/// bumps the counter past it, and a call can never see a generation from
/// the future.
fn is_generation_current(call_generation: u64, current_generation: u64) -> bool {
    call_generation == current_generation
}

fn open_role_stream(
    role: &'static str,
    device_id: &str,
    device_type: DeviceType,
    levels: Arc<Mutex<HashMap<&'static str, LevelAccumulator>>>,
) -> Result<PwCaptureStream> {
    let target = capture_target_for(device_id, device_type);
    PwCaptureStream::open(
        target,
        Box::new(move |samples| {
            if samples.is_empty() {
                return;
            }
            // Real-time callback: one short mutex hold, no allocation after
            // the first buffer (the map entry already exists).
            if let Ok(mut map) = levels.lock() {
                map.entry(role).or_default().push(samples);
            }
        }),
        Box::new(move |event| {
            if !matches!(event, PwStreamEvent::Ended) {
                warn!("level monitor: {} stream event: {:?}", role, event);
            }
        }),
    )
}

/// Start (or restart) level monitoring.
///
/// `mic_device` / `system_device`: PipeWire node id, `"default"`, or
/// `None` to skip that role.
pub async fn start_monitoring(
    sink: SharedEventSink,
    mic_device: Option<String>,
    system_device: Option<String>,
) -> Result<()> {
    info!(
        "level monitor: start (mic={:?}, system={:?})",
        mic_device, system_device
    );

    // Capture our generation *before* doing anything async. Any later
    // `start_monitoring`/`stop_monitoring` call bumps GENERATION further,
    // which is how we notice — after our (slow, blocking) stream-open
    // completes — that we've been superseded and must not touch SESSION.
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;

    let levels: Arc<Mutex<HashMap<&'static str, LevelAccumulator>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Opening streams blocks on the PipeWire roundtrip — keep it off
    // the async runtime.
    let levels_for_open = levels.clone();
    let streams = tokio::task::spawn_blocking(move || {
        let mut streams = Vec::new();
        if let Some(mic) = mic_device {
            match open_role_stream("mic", &mic, DeviceType::Microphone, levels_for_open.clone()) {
                Ok(s) => streams.push(s),
                Err(e) => warn!("level monitor: could not open mic '{}': {}", mic, e),
            }
        }
        if let Some(system) = system_device {
            match open_role_stream(
                "system",
                &system,
                DeviceType::System,
                levels_for_open.clone(),
            ) {
                Ok(s) => streams.push(s),
                Err(e) => warn!(
                    "level monitor: could not open system audio '{}': {}",
                    system, e
                ),
            }
        }
        streams
    })
    .await?;

    if streams.is_empty() {
        warn!("level monitor: no streams opened; UI will receive no events");
        return Ok(());
    }

    // Install the freshly opened streams as SESSION only if we are still
    // the current generation — otherwise a newer `start_monitoring` (or a
    // `stop_monitoring`) raced ahead of us while we were blocked opening
    // streams, and installing now would silently replace its live streams
    // with ours (freezing its meters) while ours leak, uncaptured by
    // anything. Either way, whatever ends up discarded (our own streams if
    // we're stale, or the previous session's streams if we win) is dropped
    // off the async runtime below — dropping a `PwCaptureStream` joins its
    // capture thread, which must never happen on the runtime.
    let (installed, discarded) = {
        let mut guard = session_slot()
            .lock()
            .map_err(|_| anyhow::anyhow!("level monitor: session mutex poisoned"))?;
        if is_generation_current(generation, GENERATION.load(Ordering::SeqCst)) {
            let old = guard.replace(MonitorSession { streams });
            (true, old.map(|s| s.streams))
        } else {
            (false, Some(streams))
        }
    };

    if let Some(streams) = discarded {
        tokio::task::spawn_blocking(move || drop(streams)).await?;
    }

    if !installed {
        debug!(
            "level monitor: generation {} superseded before install",
            generation
        );
        return Ok(());
    }

    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(EMIT_INTERVAL_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while is_generation_current(generation, GENERATION.load(Ordering::SeqCst)) {
            interval.tick().await;

            let snapshot: Vec<AudioLevelData> = match levels.lock() {
                Ok(mut guard) => guard
                    .iter_mut()
                    .filter_map(|(role, acc)| {
                        let label = if *role == "mic" { "input" } else { "output" };
                        acc.take(role, label)
                    })
                    .collect(),
                Err(_) => continue,
            };
            if snapshot.is_empty() {
                continue;
            }

            let update = AudioLevelUpdate {
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                levels: snapshot,
            };

            if sink.emit_event("audio-levels", &update).is_err() {
                break;
            }
        }
        debug!(
            "level monitor: emit task exiting (generation {})",
            generation
        );
    });

    Ok(())
}

pub async fn stop_monitoring() -> Result<()> {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    // Stream teardown joins the PipeWire loop threads — keep it off
    // the async runtime.
    tokio::task::spawn_blocking(stop_current_session).await?;
    Ok(())
}

fn stop_current_session() {
    if let Ok(mut guard) = session_slot().lock() {
        if let Some(session) = guard.take() {
            for stream in session.streams {
                stream.stop();
            }
        }
    }
}

pub fn is_monitoring() -> bool {
    session_slot().lock().map(|g| g.is_some()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{is_generation_current, LevelAccumulator};

    #[test]
    fn accumulator_reports_rms_and_peak_over_the_whole_period() {
        let mut acc = LevelAccumulator::default();
        acc.push(&[0.0, 0.0, 0.0, 0.0]);
        acc.push(&[0.5, -1.0]);
        let d = acc.take("mic", "input").unwrap();
        let expected = ((0.25_f64 + 1.0) / 6.0).sqrt() as f32;
        assert!((d.rms_level - expected).abs() < 1e-6);
        assert_eq!(d.peak_level, 1.0);
        assert!(d.is_active);
    }

    #[test]
    fn accumulator_resets_after_take_and_reports_silence() {
        let mut acc = LevelAccumulator::default();
        assert!(acc.take("mic", "input").is_none());
        acc.push(&[0.5]);
        acc.take("mic", "input").unwrap();
        let d = acc.take("mic", "input").unwrap();
        assert_eq!(d.rms_level, 0.0);
        assert_eq!(d.peak_level, 0.0);
        assert!(!d.is_active);
    }

    #[test]
    fn current_generation_matches_exactly() {
        assert!(is_generation_current(3, 3));
    }

    #[test]
    fn superseded_by_a_later_generation() {
        // A slow start_monitoring call captured generation 3, but by the
        // time it finishes opening streams a newer start/stop call has
        // bumped the counter to 4 (or beyond) — it must not install.
        assert!(!is_generation_current(3, 4));
        assert!(!is_generation_current(3, 10));
    }

    #[test]
    fn stale_call_never_wins_even_against_generation_zero_reset() {
        // Defensive: a call can never be "current" against a smaller
        // counter value either (the counter only ever increases).
        assert!(!is_generation_current(3, 2));
    }
}
