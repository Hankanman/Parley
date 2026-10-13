//! Meeting auto-detection (issue #85): notice when a known meeting app
//! (Teams, Zoom, Slack, Discord, Webex, Jitsi, or a browser doing a WebRTC
//! call) starts capturing the microphone, and announce it on the event sink
//! so the shell can offer to record — or record automatically.
//!
//! Three layers, thinnest at the bottom:
//!
//! - [`rules`] — pure: a stream node's property map → [`MeetingApp`] or
//!   `None`. Ignores Parley's own capture streams and sink-monitor captures.
//! - [`debounce`] — pure: a timeline of stream add/remove → `Detected` /
//!   `Ended`, requiring a stream to persist a few seconds and treating a
//!   brief drop/reconnect as the same call.
//! - `watcher` — PipeWire: a registry listener on its own thread + main
//!   loop that feeds the debouncer. Not unit-testable; kept to plumbing.
//!
//! Events (via the injected [`EventSink`]), both with a
//! [`MeetingAppEvent`] payload:
//! - `meeting-app-detected` — a call has been live for the confirm window.
//! - `meeting-app-ended` — every stream for that app has been gone for the
//!   end grace period.
//!
//! Policy (notify vs auto-record, never a second recording, stop when the
//! call ends) is the shell's job; see `parley-gpui/src/meeting_detect.rs`.

pub mod debounce;
pub mod rules;
#[cfg(target_os = "linux")]
mod watcher;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};

use crate::events::{EventSink, EventSinkExt, SharedEventSink};

pub use debounce::{Debouncer, DetectorEvent};
pub use rules::{classify, MeetingApp};

pub const EVENT_DETECTED: &str = "meeting-app-detected";
pub const EVENT_ENDED: &str = "meeting-app-ended";

/// Payload of `meeting-app-detected` / `meeting-app-ended`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeetingAppEvent {
    /// Stable id, e.g. `"teams"`, `"zoom"`, `"chrome"`.
    pub app_id: String,
    /// Display name, e.g. `"Microsoft Teams"`, `"Google Chrome"`.
    pub app_name: String,
    /// A browser capturing the mic (a web call in some tab).
    pub is_browser: bool,
}

impl From<&MeetingApp> for MeetingAppEvent {
    fn from(app: &MeetingApp) -> Self {
        Self {
            app_id: app.id.clone(),
            app_name: app.name.clone(),
            is_browser: app.is_browser,
        }
    }
}

/// Emit one debouncer event on `sink` under its event name.
pub fn emit(sink: &dyn EventSink, event: &DetectorEvent) {
    let (name, app) = match event {
        DetectorEvent::Detected(app) => (EVENT_DETECTED, app),
        DetectorEvent::Ended(app) => (EVENT_ENDED, app),
    };
    info!("meeting-detect: {name}: {} ({})", app.name, app.id);
    if let Err(e) = sink.emit_event(name, &MeetingAppEvent::from(app)) {
        warn!("meeting-detect: failed to emit {name}: {e}");
    }
}

/// How often the debouncer is ticked.
const TICK: Duration = Duration::from_millis(250);

/// A running detector. Dropping it (or [`MeetingDetector::stop`]) signals
/// both threads to exit; neither is joined, so a stalled PipeWire daemon
/// can't hang the caller (same reasoning as `audio::pw`).
pub struct MeetingDetector {
    stop: Arc<AtomicBool>,
}

impl MeetingDetector {
    /// Start watching PipeWire. Events go to `sink`.
    #[cfg(target_os = "linux")]
    pub fn start(sink: SharedEventSink) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<watcher::WatchEvent>();

        let watch_stop = stop.clone();
        std::thread::Builder::new()
            .name("pw-meeting-detect".into())
            .spawn(move || watcher::run(tx, watch_stop))
            .context("failed to spawn the meeting-detect watcher thread")?;

        let debounce_stop = stop.clone();
        std::thread::Builder::new()
            .name("meeting-detect".into())
            .spawn(move || debounce_loop(rx, sink, debounce_stop))
            .context("failed to spawn the meeting-detect debounce thread")?;

        info!("meeting-detect: started");
        Ok(Self { stop })
    }

    pub fn stop(self) {}
}

impl Drop for MeetingDetector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        info!("meeting-detect: stopped");
    }
}

#[cfg(target_os = "linux")]
fn debounce_loop(
    rx: mpsc::Receiver<watcher::WatchEvent>,
    sink: SharedEventSink,
    stop: Arc<AtomicBool>,
) {
    let mut debouncer = Debouncer::default();
    while !stop.load(Ordering::SeqCst) {
        match rx.recv_timeout(TICK) {
            Ok(event) => {
                let now = Instant::now();
                match event {
                    watcher::WatchEvent::Present { node_id, app } => {
                        debouncer.stream_present(node_id, app, now)
                    }
                    watcher::WatchEvent::Gone { node_id } => debouncer.stream_gone(node_id, now),
                    watcher::WatchEvent::Reset => debouncer.reset_streams(now),
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        for event in debouncer.tick(Instant::now()) {
            emit(sink.as_ref(), &event);
        }
    }
    debug!("meeting-detect: debounce thread exiting");
}

/// List the capture streams present right now with their classification,
/// for debugging / the live smoke test. Read-only.
#[cfg(target_os = "linux")]
pub fn snapshot_streams(
    timeout: Duration,
) -> Result<
    Vec<(
        u32,
        std::collections::HashMap<String, String>,
        Option<MeetingApp>,
    )>,
> {
    watcher::snapshot_streams(timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::RecordingSink;

    fn app() -> MeetingApp {
        MeetingApp {
            id: "teams".into(),
            name: "Microsoft Teams".into(),
            is_browser: false,
        }
    }

    #[test]
    fn emits_detected_and_ended_with_the_app_payload() {
        let sink = RecordingSink::new();
        emit(&sink, &DetectorEvent::Detected(app()));
        emit(&sink, &DetectorEvent::Ended(app()));
        let expected = serde_json::json!({
            "app_id": "teams",
            "app_name": "Microsoft Teams",
            "is_browser": false,
        });
        assert_eq!(sink.payloads(EVENT_DETECTED), vec![expected.clone()]);
        assert_eq!(sink.payloads(EVENT_ENDED), vec![expected]);
    }

    /// The full pure path: property maps → classify → debounce → events.
    #[test]
    fn property_maps_through_debouncer_to_events() {
        let props = |binary: &str| -> std::collections::HashMap<String, String> {
            [
                ("media.class", "Stream/Input/Audio"),
                ("application.process.binary", binary),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
        };
        let sink = RecordingSink::new();
        let t0 = Instant::now();
        let mut d = Debouncer::new(Duration::from_secs(3), Duration::from_secs(8));
        // Parley itself and an unrelated recorder never register.
        for (node, binary) in [(1, "zoom"), (2, "audacity")] {
            if let Some(app) = classify(&props(binary), 1) {
                d.stream_present(node, app, t0);
            }
        }
        let mut own = props("chrome");
        own.insert("application.process.id".into(), "1".into());
        assert!(classify(&own, 1).is_none());

        for e in d.tick(t0 + Duration::from_secs(1)) {
            emit(&sink, &e);
        }
        assert!(sink.events().is_empty());
        for e in d.tick(t0 + Duration::from_secs(3)) {
            emit(&sink, &e);
        }
        d.stream_gone(1, t0 + Duration::from_secs(10));
        for e in d.tick(t0 + Duration::from_secs(18)) {
            emit(&sink, &e);
        }
        let names: Vec<String> = sink.events().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec![EVENT_DETECTED, EVENT_ENDED]);
        assert_eq!(sink.payloads(EVENT_DETECTED)[0]["app_id"], "zoom");
    }

    /// Live end-to-end watch: runs the real detector for 20 s and prints
    /// every event. Pair it with a fake "Zoom" capture stream, e.g.
    /// `timeout 12 pw-record -P '{ application.name = "ZOOM VoiceEngine" }' /dev/null`.
    /// Run explicitly:
    /// `cargo test -p parley-core --lib meeting_detect::tests::live_watch -- --ignored --nocapture`
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn live_watch() {
        let sink = Arc::new(RecordingSink::new());
        let detector = MeetingDetector::start(sink.clone()).expect("start failed");
        std::thread::sleep(Duration::from_secs(25));
        drop(detector);
        for (name, payload) in sink.events() {
            println!("  {name}: {payload}");
        }
    }

    /// Live, read-only smoke test against the running PipeWire session:
    /// lists every capture stream and how it classifies. Run explicitly:
    /// `cargo test -p parley-core --lib meeting_detect::tests::live_snapshot -- --ignored --nocapture`
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn live_snapshot() {
        let streams = snapshot_streams(Duration::from_secs(3)).expect("snapshot failed");
        println!("{} capture stream(s):", streams.len());
        for (id, props, app) in streams {
            let get = |k: &str| props.get(k).cloned().unwrap_or_default();
            println!(
                "  node {id}: app.name={:?} binary={:?} media.name={:?} node.name={:?} -> {:?}",
                get("application.name"),
                get("application.process.binary"),
                get("media.name"),
                get("node.name"),
                app.map(|a| a.name)
            );
        }
    }
}
