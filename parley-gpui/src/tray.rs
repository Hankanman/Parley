//! StatusNotifierItem tray icon: Start/Stop recording, Open Parley (focus
//! the main window), Quit. Mirrors `frontend/src-tauri/src/tray.rs`'s menu
//! shape where sensible, built on `gpui-tray` the way
//! `spikes/gpui-shell/src/tray.rs` proved out.
//!
//! The menu is kept in sync with the canonical recording-state machine by
//! subscribing to the `recording-state` core event on [`CoreEvents`] — the
//! same event the Tauri shell's `TrayRefreshingSink` reacts to — rather than
//! polling. A tray failure (no StatusNotifierItem host running) is logged
//! and otherwise ignored: the rest of the app must keep working without a
//! tray icon.

use gpui_kit::component::Root;
use gpui_kit::*;
use gpui_tray::{Icon, Tray};

use parley_core::audio::recording_phase::RecordingPhase;
use parley_core::audio::recording_service::{self, RecordingArgs, StartRequest};

use crate::app_state::AppServices;
use crate::core_events::CoreEvent;
use crate::notifications;
use crate::shell::{navigate, Route};

actions!(
    parley_tray,
    [
        TrayStartRecording,
        TrayStopRecording,
        TrayPauseRecording,
        TrayResumeRecording,
        TrayOpenParley,
        TraySettings,
        TrayCheckForUpdates,
        TrayQuit,
    ]
);

/// The main window, stashed here so the tray's "Open Parley" action can
/// bring it back to the front. Set once, right after `main.rs` opens the
/// window.
pub struct MainWindow(pub WindowHandle<Root>);

impl Global for MainWindow {}

/// Current recording phase, mirrored from `recording-state` core events so
/// the tray menu can be rebuilt synchronously (`gpui-tray`'s `menu` builder
/// takes a plain `Fn(&mut App) -> Vec<MenuItem>`, no async).
#[derive(Clone, Copy)]
struct TrayPhase(RecordingPhase);

impl Default for TrayPhase {
    fn default() -> Self {
        Self(RecordingPhase::Idle)
    }
}

impl Global for TrayPhase {}

struct AppTray {
    tray: Tray,
}

impl Global for AppTray {}

/// Whether a tray icon is actually registered. Closing the window is only
/// safe to treat as "close to tray" when there's a tray to get back from.
pub fn is_installed(cx: &App) -> bool {
    cx.has_global::<AppTray>()
}

/// A simple filled circle, red while idle/stopped-ish, matching the
/// spike's icon. `gpui-tray::Tray::set_icon` could recolor this live; out
/// of scope for phase 1.
fn tray_icon() -> gpui_tray::Result<Icon> {
    const SIZE: u32 = 32;
    let mut rgba = vec![0_u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as i32 - 15;
            let dy = y as i32 - 15;
            if dx * dx + dy * dy <= 13 * 13 {
                let offset = ((y * SIZE + x) * 4) as usize;
                rgba[offset..offset + 4].copy_from_slice(&[220, 70, 70, 255]);
            }
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE)
}

fn disabled(item: MenuItem, disabled: bool) -> MenuItem {
    item.disabled(disabled)
}

fn build_menu(cx: &mut App) -> Vec<MenuItem> {
    use RecordingPhase::*;

    let phase = cx.try_global::<TrayPhase>().copied().unwrap_or_default().0;

    let status_label = match phase {
        Idle => "Not recording",
        Starting => "Starting…",
        Recording => "Recording…",
        Paused => "Paused",
        Stopping => "Stopping…",
        Finalising => "Finalising…",
        Error => "Recording error",
    };

    let can_start = matches!(phase, Idle);
    let can_stop = matches!(phase, Recording | Paused);
    let can_pause = matches!(phase, Recording);
    let can_resume = matches!(phase, Paused);

    vec![
        disabled(MenuItem::action(status_label, NoAction), true),
        MenuItem::separator(),
        disabled(
            MenuItem::action("Start recording", TrayStartRecording),
            !can_start,
        ),
        disabled(
            MenuItem::action("Pause recording", TrayPauseRecording),
            !can_pause,
        ),
        disabled(
            MenuItem::action("Resume recording", TrayResumeRecording),
            !can_resume,
        ),
        disabled(
            MenuItem::action("Stop recording", TrayStopRecording),
            !can_stop,
        ),
        MenuItem::separator(),
        MenuItem::action("Open Parley", TrayOpenParley),
        MenuItem::action("Settings", TraySettings),
        MenuItem::action("Check for updates", TrayCheckForUpdates),
        MenuItem::separator(),
        MenuItem::action("Quit", TrayQuit),
    ]
}

/// Bring the main window back: re-opens it if it has been closed (the app
/// keeps running without a window under `QuitMode::Explicit`), then asks the
/// compositor to focus it. Wayland compositors commonly refuse an
/// unsolicited raise, so the re-open is what actually gets the UI back.
fn open_parley(cx: &mut App) {
    let Some(handle) = crate::window::ensure_main_window(cx) else {
        return;
    };
    if let Err(e) = handle.update(cx, |_, window, _cx| window.activate_window()) {
        log::warn!("tray: failed to activate the main window: {}", e);
    }
}

fn start_recording_from_tray(cx: &mut App) {
    let services = AppServices::global(cx);
    let io = services.io.clone();
    let ctx = services.recording_context();
    io.spawn(async move {
        let hooks = recording_service::default_start_hooks(ctx.pool.clone());
        if let Err(e) = recording_service::start(
            ctx,
            hooks,
            StartRequest {
                mic_device_name: None,
                system_device_name: None,
                meeting_name: None,
            },
        )
        .await
        {
            log::error!("tray: failed to start recording: {}", e);
        }
    });
}

fn pause_recording_from_tray(cx: &mut App) {
    let services = AppServices::global(cx);
    let io = services.io.clone();
    let ctx = services.recording_context();
    io.spawn(async move {
        if let Err(e) = recording_service::pause_recording(&ctx).await {
            log::error!("tray: failed to pause recording: {}", e);
        }
    });
}

fn resume_recording_from_tray(cx: &mut App) {
    let services = AppServices::global(cx);
    let io = services.io.clone();
    let ctx = services.recording_context();
    io.spawn(async move {
        if let Err(e) = recording_service::resume_recording(&ctx).await {
            log::error!("tray: failed to resume recording: {}", e);
        }
    });
}

/// Show + focus the main window, then navigate to Settings — used by both
/// the tray's "Settings" item.
fn open_settings(cx: &mut App) {
    open_parley(cx);
    navigate(Route::Settings, cx);
}

fn stop_recording_from_tray(cx: &mut App) {
    let services = AppServices::global(cx);
    let ctx = services.recording_context();
    let save_path = parley_core::paths::app_data_dir()
        .map(|dir| {
            let timestamp = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
            dir.join(format!("recording-{}.wav", timestamp))
                .to_string_lossy()
                .to_string()
        })
        .unwrap_or_else(|_| "recording.wav".to_string());
    services.io.spawn(async move {
        if let Err(e) = recording_service::stop(ctx, RecordingArgs { save_path }).await {
            log::error!("tray: failed to stop recording: {}", e);
        }
    });
}

/// Register tray actions and build the tray icon. Call once at startup,
/// after `AppServices` and `MainWindow` globals are set. The caller should
/// treat an `Err` as non-fatal (log and keep running without a tray icon —
/// e.g. no StatusNotifierItem host on the session bus).
pub fn install(cx: &mut App) -> gpui_tray::Result<()> {
    cx.set_global(TrayPhase::default());

    cx.on_action(|_: &TrayStartRecording, cx: &mut App| {
        log::info!("tray: start recording");
        start_recording_from_tray(cx);
    });
    cx.on_action(|_: &TrayStopRecording, cx: &mut App| {
        log::info!("tray: stop recording");
        stop_recording_from_tray(cx);
    });
    cx.on_action(|_: &TrayPauseRecording, cx: &mut App| {
        log::info!("tray: pause recording");
        pause_recording_from_tray(cx);
    });
    cx.on_action(|_: &TrayResumeRecording, cx: &mut App| {
        log::info!("tray: resume recording");
        resume_recording_from_tray(cx);
    });
    cx.on_action(|_: &TrayOpenParley, cx: &mut App| {
        log::info!("tray: open Parley");
        open_parley(cx);
    });
    cx.on_action(|_: &TraySettings, cx: &mut App| {
        log::info!("tray: settings");
        open_settings(cx);
    });
    cx.on_action(|_: &TrayCheckForUpdates, cx: &mut App| {
        log::info!("tray: check for updates");
        notifications::check_for_updates(cx);
    });
    cx.on_action(|_: &TrayQuit, cx: &mut App| {
        log::info!("tray: quit");
        crate::request_quit(cx);
    });

    let tray = Tray::builder()
        .icon(tray_icon()?)
        .title("Parley")
        .tooltip("Parley")
        .menu(build_menu)
        .build(cx)?;
    cx.set_global(AppTray { tray });
    log::info!("tray: StatusNotifierItem registered");

    let services = AppServices::global(cx);
    let core_events = services.core_events.clone();
    cx.subscribe(&core_events, |_core_events, event: &CoreEvent, cx| {
        match event.name.as_str() {
            "recording-state" => {
                let Some(snapshot) =
                    event.decode::<parley_core::audio::recording_phase::RecordingSnapshot>()
                else {
                    return;
                };
                let previous = cx.try_global::<TrayPhase>().copied().unwrap_or_default().0;
                // Pause/resume have no dedicated `recording-*` event (unlike
                // start/stop) — the Tauri shell doesn't notify on them
                // either (see `notifications.rs`'s module doc). Detect the
                // transition here, off the canonical phase machine.
                if previous == RecordingPhase::Recording && snapshot.phase == RecordingPhase::Paused {
                    notifications::maybe_notify(cx, notifications::Kind::Paused);
                } else if previous == RecordingPhase::Paused && snapshot.phase == RecordingPhase::Recording {
                    notifications::maybe_notify(cx, notifications::Kind::Resumed);
                }
                cx.set_global(TrayPhase(snapshot.phase));
                refresh(cx);
            }
            "recording-started" => notifications::maybe_notify(cx, notifications::Kind::Started),
            "recording-stopped" => notifications::maybe_notify(cx, notifications::Kind::Stopped),
            "meeting-refined" => {
                notifications::maybe_notify(cx, notifications::Kind::TranscriptionComplete)
            }
            "recording-error" => {
                if let Some(message) = event.decode::<String>() {
                    notifications::maybe_notify(cx, notifications::Kind::SystemError(message));
                }
            }
            "transcription-error" => {
                let message = event
                    .payload
                    .get("userMessage")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "Transcription error".to_string());
                notifications::maybe_notify(cx, notifications::Kind::SystemError(message));
            }
            _ => {}
        }
    })
    .detach();

    Ok(())
}

/// Rebuild the tray menu from the current `TrayPhase`. Cheap; called on
/// every `recording-state` event.
fn refresh(cx: &mut App) {
    if let Some(tray) = cx.try_global::<AppTray>() {
        let tray = tray.tray.clone();
        if let Err(e) = tray.refresh_menu(cx) {
            log::warn!("tray: failed to refresh menu: {}", e);
        }
    }
}

/// Tear the tray down (best-effort). Called from `request_quit` before the
/// window closes, mirroring the Tauri tray's `close` on Quit.
pub fn shutdown(cx: &mut App) {
    if let Some(tray) = cx.try_global::<AppTray>() {
        let tray = tray.tray.clone();
        let _ = tray.close(cx);
    }
}
