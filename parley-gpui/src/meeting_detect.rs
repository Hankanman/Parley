//! Shell-side policy for meeting auto-detection (issue #85). The core
//! detector (`parley_core::audio::meeting_detect`) only reports
//! `meeting-app-detected` / `meeting-app-ended`; this decides what to do
//! with them, per Settings → Recording:
//!
//! - **Off** — the detector isn't running at all.
//! - **Notify** (default) — a "Teams call detected — Record?" notification
//!   with a Record action (clicking the body opens the Recording view), plus
//!   a "Record … call" item in the tray menu while the call lasts.
//! - **Auto-record** — start recording straight away, titled after the
//!   calendar event in progress (and linked to it) if there is one, with a
//!   notification saying so.
//!
//! Never starts a second recording: every start path checks
//! `recording_service::is_busy()` first (and `start()` itself refuses an
//! overlapping start). With "stop when the call ends" on, a recording that
//! was started while a call was detected is stopped when that call ends.

use chrono::{DateTime, Local};
use gpui_kit::*;

use parley_core::audio::meeting_detect::{MeetingAppEvent, MeetingDetector, EVENT_DETECTED, EVENT_ENDED};
use parley_core::audio::recording_preferences::{
    load_recording_preferences, MeetingAutoDetect, RecordingPreferences,
};
use parley_core::audio::recording_service::{self, StartRequest};
use parley_core::calendar::repository::CalendarRepository;

use crate::app_state::AppServices;
use crate::core_events::CoreEvent;
use crate::notifications::{self, CallPromptResponse};
use crate::shell::{navigate, Route};

#[derive(Default)]
struct DetectState {
    detector: Option<MeetingDetector>,
    /// Calls currently detected, oldest first.
    active_calls: Vec<MeetingAppEvent>,
    /// App id of the call the current recording belongs to (started from
    /// detection, or started by hand while that call was live) — the one
    /// whose end stops it when "stop when the call ends" is on.
    recording_for_call: Option<String>,
    /// `(meeting_id, event_id)` linked at auto-start; re-linked on stop so
    /// the calendar snapshot lands in the (by then final) meeting folder.
    pending_calendar_link: Option<(String, String)>,
}

impl Global for DetectState {}

/// "Microsoft Teams call" / "Web call in Google Chrome".
pub fn call_label(app: &MeetingAppEvent) -> String {
    if app.is_browser {
        format!("Web call in {}", app.app_name)
    } else {
        format!("{} call", app.app_name)
    }
}

/// Meeting title when no calendar event is in progress.
fn placeholder_title(app: &MeetingAppEvent, now: DateTime<Local>) -> String {
    let who = if app.is_browser { "Web call" } else { app.app_name.as_str() };
    let what = if app.is_browser { "" } else { " call" };
    format!("{who}{what} {}", now.format("%Y-%m-%d %H:%M"))
}

/// Pick the meeting title: the live calendar event's summary if it has one.
fn choose_title(event_summary: Option<&str>, app: &MeetingAppEvent, now: DateTime<Local>) -> String {
    match event_summary.map(str::trim) {
        Some(summary) if !summary.is_empty() => summary.to_string(),
        _ => placeholder_title(app, now),
    }
}

/// The most recently detected call still in progress, for the tray hint.
pub fn active_call(cx: &App) -> Option<MeetingAppEvent> {
    cx.try_global::<DetectState>()
        .and_then(|s| s.active_calls.last().cloned())
}

/// Wire up detection: subscribe to the detector's events and the recording
/// lifecycle, then start the detector unless the setting is Off. Call
/// once, after the tray is installed.
pub fn install(cx: &mut App) {
    cx.set_global(DetectState::default());

    let services = AppServices::global(cx);
    let core_events = services.core_events.clone();
    cx.subscribe(&core_events, |_, event: &CoreEvent, cx| match event.name.as_str() {
        EVENT_DETECTED => {
            if let Some(app) = event.decode::<MeetingAppEvent>() {
                on_detected(cx, app);
            }
        }
        EVENT_ENDED => {
            if let Some(app) = event.decode::<MeetingAppEvent>() {
                on_ended(cx, app);
            }
        }
        "recording-started" => {
            // A recording started by hand during a detected call belongs to
            // that call (the auto paths set this themselves, first).
            let state = cx.global_mut::<DetectState>();
            if state.recording_for_call.is_none() {
                state.recording_for_call = state.active_calls.last().map(|c| c.app_id.clone());
            }
        }
        "recording-stopped" => on_recording_stopped(cx, event),
        _ => {}
    })
    .detach();

    let services = AppServices::global(cx);
    let io = services.io.clone();
    let pool = services.pool();
    cx.spawn(async move |cx| {
        let prefs = load_prefs(&io, pool).await;
        let _ = cx.update(|cx| apply_mode(cx, prefs.meeting_autodetect));
    })
    .detach();
}

/// Start or stop the detector to match `mode`. Called at startup and when
/// the setting changes.
pub fn apply_mode(cx: &mut App, mode: MeetingAutoDetect) {
    if !cx.has_global::<DetectState>() {
        return;
    }
    let sink = AppServices::global(cx).sink.clone();
    let state = cx.global_mut::<DetectState>();
    match mode {
        MeetingAutoDetect::Off => {
            state.detector = None;
            state.active_calls.clear();
        }
        MeetingAutoDetect::Notify | MeetingAutoDetect::AutoRecord => {
            if state.detector.is_none() {
                match MeetingDetector::start(sink) {
                    Ok(detector) => state.detector = Some(detector),
                    Err(e) => log::warn!("meeting-detect: failed to start: {e}"),
                }
            }
        }
    }
    crate::tray::refresh(cx);
}

async fn load_prefs(io: &crate::runtime::Io, pool: Option<sqlx::SqlitePool>) -> RecordingPreferences {
    io.spawn(async move { load_recording_preferences(pool).await })
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default()
}

fn on_detected(cx: &mut App, app: MeetingAppEvent) {
    {
        let state = cx.global_mut::<DetectState>();
        state.active_calls.retain(|c| c.app_id != app.app_id);
        state.active_calls.push(app.clone());
    }
    crate::tray::refresh(cx);

    let services = AppServices::global(cx);
    let io = services.io.clone();
    let pool = services.pool();
    cx.spawn(async move |cx| {
        let prefs = load_prefs(&io, pool.clone()).await;
        let busy = io.spawn(recording_service::is_busy()).await.unwrap_or(true);
        if busy {
            log::info!(
                "meeting-detect: {} detected while a recording is active; not offering another",
                app.app_name
            );
            return;
        }
        match prefs.meeting_autodetect {
            MeetingAutoDetect::Off => {}
            MeetingAutoDetect::AutoRecord => {
                let _ = cx.update(|cx| record_call(cx, app, true));
            }
            MeetingAutoDetect::Notify => {
                let label = call_label(&app);
                let response = io
                    .spawn(notifications::prompt_call_detected(pool, label))
                    .await
                    .unwrap_or(CallPromptResponse::Dismissed);
                let _ = cx.update(|cx| match response {
                    CallPromptResponse::Record => record_call(cx, app, false),
                    CallPromptResponse::Open => {
                        crate::tray::open_parley(cx);
                        navigate(Route::Recording, cx);
                    }
                    CallPromptResponse::Dismissed => {}
                });
            }
        }
    })
    .detach();
}

fn on_ended(cx: &mut App, app: MeetingAppEvent) {
    let was_recording_this_call = {
        let state = cx.global_mut::<DetectState>();
        state.active_calls.retain(|c| c.app_id != app.app_id);
        state.recording_for_call.as_deref() == Some(app.app_id.as_str())
    };
    crate::tray::refresh(cx);
    if !was_recording_this_call {
        return;
    }

    let services = AppServices::global(cx);
    let io = services.io.clone();
    let pool = services.pool();
    cx.spawn(async move |cx| {
        let prefs = load_prefs(&io, pool).await;
        if !prefs.stop_when_call_ends {
            return;
        }
        let recording = io.spawn(recording_service::is_recording()).await.unwrap_or(false);
        if !recording {
            return;
        }
        log::info!("meeting-detect: {} ended; stopping the recording", app.app_name);
        let _ = cx.update(|cx| crate::tray::stop_recording(cx));
    })
    .detach();
}

fn on_recording_stopped(cx: &mut App, event: &CoreEvent) {
    let stopped_meeting = event
        .payload
        .get("meeting_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let link = {
        let state = cx.global_mut::<DetectState>();
        state.recording_for_call = None;
        state.pending_calendar_link.take()
    };
    let Some((meeting_id, event_id)) = link else { return };
    if stopped_meeting.as_deref() != Some(meeting_id.as_str()) {
        return;
    }
    let services = AppServices::global(cx);
    let Some(pool) = services.pool() else { return };
    services.io.spawn(async move {
        if let Err(e) =
            parley_core::calendar::service::link_meeting_with_snapshot(&pool, &meeting_id, Some(&event_id)).await
        {
            log::warn!("meeting-detect: failed to re-link meeting {meeting_id} to event {event_id}: {e}");
        }
    });
}

/// Start recording a detected call: titled after (and linked to) the
/// calendar event in progress, or a placeholder title. `announce` shows the
/// "recording started" notification (auto-record; a user-clicked Record
/// doesn't need telling).
pub fn record_call(cx: &mut App, app: MeetingAppEvent, announce: bool) {
    let services = AppServices::global(cx);
    let io = services.io.clone();
    let ctx = services.recording_context();
    let pool = services.pool();
    {
        let state = cx.global_mut::<DetectState>();
        state.recording_for_call = Some(app.app_id.clone());
    }

    cx.spawn(async move |cx| {
        let outcome = io
            .spawn(async move {
                if recording_service::is_busy().await {
                    return Err("A recording is already in progress".to_string());
                }
                let event = match &pool {
                    Some(pool) => CalendarRepository::find_event_for_now(pool).await.ok().flatten(),
                    None => None,
                };
                let title = choose_title(event.as_ref().and_then(|e| e.summary.as_deref()), &app, Local::now());
                let hooks = recording_service::default_start_hooks(ctx.pool.clone());
                recording_service::start(
                    ctx,
                    hooks,
                    StartRequest {
                        mic_device_name: None,
                        system_device_name: None,
                        meeting_name: Some(title.clone()),
                    },
                )
                .await?;

                // Link right away (so a crash mid-call keeps the link); the
                // `recording-stopped` handler links again to write the
                // calendar snapshot into the finished meeting folder.
                let mut link = None;
                if let (Some(pool), Some(event), Some(meeting_id)) =
                    (&pool, &event, recording_service::current_meeting_id())
                {
                    if let Err(e) = parley_core::calendar::service::link_meeting_with_snapshot(
                        pool,
                        &meeting_id,
                        Some(&event.id),
                    )
                    .await
                    {
                        log::warn!("meeting-detect: failed to link meeting {meeting_id} to event {}: {e}", event.id);
                    }
                    link = Some((meeting_id, event.id.clone()));
                }
                Ok::<_, String>((app, title, link))
            })
            .await
            .unwrap_or_else(|e| Err(format!("recording task failed: {e}")));

        let _ = cx.update(|cx| match outcome {
            Ok((app, title, link)) => {
                log::info!("meeting-detect: recording \"{title}\" for {}", app.app_name);
                cx.global_mut::<DetectState>().pending_calendar_link = link;
                if announce {
                    notifications::maybe_notify(
                        cx,
                        notifications::Kind::CallRecordingStarted {
                            app_label: call_label(&app),
                            title,
                        },
                    );
                }
            }
            Err(e) => {
                log::warn!("meeting-detect: couldn't start recording: {e}");
                let state = cx.global_mut::<DetectState>();
                // Leave an existing recording's attribution alone.
                if !e.contains("already") {
                    state.recording_for_call = None;
                }
            }
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that would pull in gpui's `#[test]` attribute macro.
    use super::{call_label, choose_title, MeetingAppEvent};
    use chrono::{DateTime, Local, TimeZone};

    fn teams() -> MeetingAppEvent {
        MeetingAppEvent {
            app_id: "teams".into(),
            app_name: "Microsoft Teams".into(),
            is_browser: false,
        }
    }

    fn chrome() -> MeetingAppEvent {
        MeetingAppEvent {
            app_id: "chrome".into(),
            app_name: "Google Chrome".into(),
            is_browser: true,
        }
    }

    fn now() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 10, 14, 5, 0).unwrap()
    }

    #[test]
    fn labels_native_apps_and_browsers() {
        assert_eq!(call_label(&teams()), "Microsoft Teams call");
        assert_eq!(call_label(&chrome()), "Web call in Google Chrome");
    }

    #[test]
    fn prefers_the_calendar_event_title() {
        assert_eq!(choose_title(Some("Weekly sync"), &teams(), now()), "Weekly sync");
    }

    #[test]
    fn falls_back_to_a_placeholder_title() {
        assert_eq!(choose_title(None, &teams(), now()), "Microsoft Teams call 2026-10-10 14:05");
        assert_eq!(choose_title(Some("   "), &teams(), now()), "Microsoft Teams call 2026-10-10 14:05");
        assert_eq!(choose_title(None, &chrome(), now()), "Web call 2026-10-10 14:05");
    }
}
