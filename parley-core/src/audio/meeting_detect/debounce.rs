//! Pure debouncing: turns a timeline of "stream N (app X) appeared" /
//! "stream N disappeared" observations into `Detected` / `Ended` events.
//!
//! Per app (not per stream — an app may open several capture streams, or
//! re-open one when the user switches microphone):
//!
//! ```text
//!            stream added                 present >= confirm_after
//!   (none) ───────────────▶ Pending ───────────────────────────▶ Active  ⇒ Detected
//!     ▲                       │ all streams gone                  │  ▲
//!     │                       ▼                                   │  │ stream re-added
//!     └──────────────────── (none)          all streams gone      ▼  │ (no event)
//!     ▲                                    ─────────────────▶ Ending
//!     │              gone for >= end_grace                        │
//!     └───────────────────────────────────────────────────────────┘  ⇒ Ended
//! ```
//!
//! So a stream must be present for `confirm_after` before it counts, and a
//! brief drop (device switch, reconnect) shorter than `end_grace` neither
//! ends the call nor announces a new one. Time is passed in explicitly, so
//! tests drive it with synthetic `Instant`s.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::rules::MeetingApp;

/// Default: a mic stream must persist this long before it's a call.
pub const DEFAULT_CONFIRM_AFTER: Duration = Duration::from_secs(3);
/// Default: a call survives its streams vanishing for this long.
pub const DEFAULT_END_GRACE: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectorEvent {
    Detected(MeetingApp),
    Ended(MeetingApp),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Pending { since: Instant },
    Active,
    Ending { since: Instant },
}

#[derive(Debug)]
struct AppState {
    app: MeetingApp,
    phase: Phase,
}

#[derive(Debug)]
pub struct Debouncer {
    confirm_after: Duration,
    end_grace: Duration,
    /// Live stream node id → the app it belongs to (by `MeetingApp::id`).
    streams: HashMap<u32, String>,
    apps: HashMap<String, AppState>,
}

impl Default for Debouncer {
    fn default() -> Self {
        Self::new(DEFAULT_CONFIRM_AFTER, DEFAULT_END_GRACE)
    }
}

impl Debouncer {
    pub fn new(confirm_after: Duration, end_grace: Duration) -> Self {
        Self {
            confirm_after,
            end_grace,
            streams: HashMap::new(),
            apps: HashMap::new(),
        }
    }

    fn live_streams(&self, app_id: &str) -> usize {
        self.streams.values().filter(|id| id.as_str() == app_id).count()
    }

    /// A stream node `node_id` is (still) present and attributed to `app`.
    /// Idempotent: PipeWire re-sends node info on every property change.
    pub fn stream_present(&mut self, node_id: u32, app: MeetingApp, now: Instant) {
        if self.streams.get(&node_id) == Some(&app.id) {
            return;
        }
        // A node re-attributed to a different app (shouldn't happen, but
        // props can change): treat as remove + add.
        if self.streams.contains_key(&node_id) {
            self.stream_gone(node_id, now);
        }
        self.streams.insert(node_id, app.id.clone());
        match self.apps.get_mut(&app.id) {
            None => {
                self.apps.insert(
                    app.id.clone(),
                    AppState {
                        app,
                        phase: Phase::Pending { since: now },
                    },
                );
            }
            Some(state) => {
                if let Phase::Ending { .. } = state.phase {
                    // Reconnected within the grace period: same call.
                    state.phase = Phase::Active;
                }
            }
        }
    }

    /// Stream node `node_id` went away (or stopped matching). Unknown ids
    /// are ignored.
    pub fn stream_gone(&mut self, node_id: u32, now: Instant) {
        let Some(app_id) = self.streams.remove(&node_id) else {
            return;
        };
        if self.live_streams(&app_id) > 0 {
            return;
        }
        let Some(state) = self.apps.get_mut(&app_id) else {
            return;
        };
        match state.phase {
            // Never confirmed: a blip, forget it silently.
            Phase::Pending { .. } => {
                self.apps.remove(&app_id);
            }
            Phase::Active => state.phase = Phase::Ending { since: now },
            Phase::Ending { .. } => {}
        }
    }

    /// Every stream is gone at once (PipeWire connection lost). Active
    /// calls enter their grace period, so a quick reconnect that
    /// re-announces the same streams (under new ids) is seamless.
    pub fn reset_streams(&mut self, now: Instant) {
        let ids: Vec<u32> = self.streams.keys().copied().collect();
        for id in ids {
            self.stream_gone(id, now);
        }
    }

    /// Advance time: confirm pending apps and end expired ones. Call
    /// periodically (the watcher ticks a few times a second).
    pub fn tick(&mut self, now: Instant) -> Vec<DetectorEvent> {
        let mut events = Vec::new();
        let mut ended = Vec::new();
        // Deterministic order for tests/logs.
        let mut ids: Vec<&String> = self.apps.keys().collect();
        ids.sort();
        let ids: Vec<String> = ids.into_iter().cloned().collect();
        for id in ids {
            let state = self.apps.get_mut(&id).expect("id from keys");
            match state.phase {
                Phase::Pending { since } if now.saturating_duration_since(since) >= self.confirm_after => {
                    state.phase = Phase::Active;
                    events.push(DetectorEvent::Detected(state.app.clone()));
                }
                Phase::Ending { since } if now.saturating_duration_since(since) >= self.end_grace => {
                    events.push(DetectorEvent::Ended(state.app.clone()));
                    ended.push(id.clone());
                }
                _ => {}
            }
        }
        for id in ended {
            self.apps.remove(&id);
        }
        events
    }

    /// Apps currently confirmed as in a call (including ones in their end
    /// grace period).
    pub fn active_apps(&self) -> Vec<MeetingApp> {
        let mut apps: Vec<MeetingApp> = self
            .apps
            .values()
            .filter(|s| matches!(s.phase, Phase::Active | Phase::Ending { .. }))
            .map(|s| s.app.clone())
            .collect();
        apps.sort_by(|a, b| a.id.cmp(&b.id));
        apps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str) -> MeetingApp {
        MeetingApp {
            id: id.to_string(),
            name: id.to_uppercase(),
            is_browser: false,
        }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn ms(m: u64) -> Duration {
        Duration::from_millis(m)
    }

    /// Run a scripted timeline, ticking every 250 ms, and collect events
    /// with the time (since t0) they were emitted at.
    enum Step {
        Add(u64, u32, &'static str),
        Gone(u64, u32),
        Reset(u64),
    }

    fn run(steps: &[Step], until_ms: u64) -> Vec<(u64, DetectorEvent)> {
        let t0 = Instant::now();
        let mut d = Debouncer::new(secs(3), secs(8));
        let mut out = Vec::new();
        let mut t = 0;
        while t <= until_ms {
            for step in steps {
                match *step {
                    Step::Add(at, node, id) if at == t => d.stream_present(node, app(id), t0 + ms(t)),
                    Step::Gone(at, node) if at == t => d.stream_gone(node, t0 + ms(t)),
                    Step::Reset(at) if at == t => d.reset_streams(t0 + ms(t)),
                    _ => {}
                }
            }
            for e in d.tick(t0 + ms(t)) {
                out.push((t, e));
            }
            t += 250;
        }
        out
    }

    #[test]
    fn detects_after_the_confirm_window_and_ends_after_grace() {
        let events = run(&[Step::Add(0, 10, "teams"), Step::Gone(60_000, 10)], 80_000);
        assert_eq!(
            events,
            vec![
                (3_000, DetectorEvent::Detected(app("teams"))),
                (68_000, DetectorEvent::Ended(app("teams"))),
            ]
        );
    }

    #[test]
    fn a_short_blip_is_never_announced() {
        let events = run(&[Step::Add(0, 10, "zoom"), Step::Gone(2_000, 10)], 20_000);
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn a_reconnect_within_grace_is_the_same_call() {
        // Stream drops at 20 s and a new node (new id) appears 3 s later.
        let events = run(
            &[
                Step::Add(0, 10, "teams"),
                Step::Gone(20_000, 10),
                Step::Add(23_000, 11, "teams"),
                Step::Gone(40_000, 11),
            ],
            60_000,
        );
        assert_eq!(
            events,
            vec![
                (3_000, DetectorEvent::Detected(app("teams"))),
                (48_000, DetectorEvent::Ended(app("teams"))),
            ]
        );
    }

    #[test]
    fn a_reconnect_after_grace_is_a_new_call() {
        let events = run(
            &[
                Step::Add(0, 10, "teams"),
                Step::Gone(10_000, 10),
                Step::Add(30_000, 11, "teams"),
            ],
            40_000,
        );
        assert_eq!(
            events,
            vec![
                (3_000, DetectorEvent::Detected(app("teams"))),
                (18_000, DetectorEvent::Ended(app("teams"))),
                (33_000, DetectorEvent::Detected(app("teams"))),
            ]
        );
    }

    #[test]
    fn multiple_streams_from_one_app_count_once() {
        let events = run(
            &[
                Step::Add(0, 10, "discord"),
                Step::Add(1_000, 11, "discord"),
                Step::Gone(5_000, 10),
                Step::Gone(30_000, 11),
            ],
            50_000,
        );
        assert_eq!(
            events,
            vec![
                (3_000, DetectorEvent::Detected(app("discord"))),
                (38_000, DetectorEvent::Ended(app("discord"))),
            ]
        );
    }

    #[test]
    fn apps_are_tracked_independently() {
        let events = run(
            &[
                Step::Add(0, 10, "slack"),
                Step::Add(1_000, 20, "zoom"),
                Step::Gone(10_000, 10),
            ],
            20_000,
        );
        assert_eq!(
            events,
            vec![
                (3_000, DetectorEvent::Detected(app("slack"))),
                (4_000, DetectorEvent::Detected(app("zoom"))),
                (18_000, DetectorEvent::Ended(app("slack"))),
            ]
        );
    }

    #[test]
    fn repeated_info_for_the_same_stream_is_idempotent() {
        let events = run(
            &[
                Step::Add(0, 10, "teams"),
                Step::Add(2_000, 10, "teams"),
                Step::Add(5_000, 10, "teams"),
            ],
            10_000,
        );
        assert_eq!(events, vec![(3_000, DetectorEvent::Detected(app("teams")))]);
    }

    #[test]
    fn connection_reset_then_reannounce_keeps_the_call() {
        let events = run(
            &[
                Step::Add(0, 10, "teams"),
                Step::Reset(10_000),
                Step::Add(12_000, 99, "teams"),
            ],
            30_000,
        );
        assert_eq!(events, vec![(3_000, DetectorEvent::Detected(app("teams")))]);
    }

    #[test]
    fn unknown_removals_are_ignored() {
        let events = run(&[Step::Gone(1_000, 77)], 5_000);
        assert!(events.is_empty());
    }

    #[test]
    fn active_apps_reports_confirmed_calls_only() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(secs(3), secs(8));
        d.stream_present(1, app("zoom"), t0);
        assert!(d.active_apps().is_empty());
        d.tick(t0 + secs(3));
        assert_eq!(d.active_apps(), vec![app("zoom")]);
        d.stream_gone(1, t0 + secs(4));
        assert_eq!(d.active_apps(), vec![app("zoom")], "still in grace");
        d.tick(t0 + secs(12));
        assert!(d.active_apps().is_empty());
    }
}
