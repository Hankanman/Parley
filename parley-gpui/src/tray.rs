//! StatusNotifierItem tray icon: Start/Stop recording, Bookmark this
//! moment, Open Parley (focus
//! the main window), Quit. Built on `gpui-tray` the way
//! `spikes/gpui-shell/src/tray.rs` proved out.
//!
//! The menu is kept in sync with the canonical recording-state machine by
//! subscribing to the `recording-state` core event on [`CoreEvents`] rather
//! than polling. A tray failure (no StatusNotifierItem host running) is logged
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
        TrayBookmark,
        TrayRecordDetectedCall,
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
    /// The Parley logo, shown whenever nothing is being captured.
    idle_icon: Icon,
    /// The red dot, shown while audio is being captured.
    recording_icon: Icon,
    showing_recording: bool,
}

impl Global for AppTray {}

/// Whether a tray icon is actually registered. Closing the window is only
/// safe to treat as "close to tray" when there's a tray to get back from.
pub fn is_installed(cx: &App) -> bool {
    cx.has_global::<AppTray>()
}

/// The app icon (the same PNG the AppImage installs), box-filtered down to
/// 64 px so every icon change isn't a megabyte over D-Bus.
fn idle_icon(cx: &App) -> gpui_tray::Result<Icon> {
    const PNG: &[u8] = include_bytes!("../packaging/linux/icon.png");
    const SIZE: u32 = 64;
    let image = Image::from_bytes(ImageFormat::Png, PNG.to_vec());
    let rendered = image
        .to_image_data(cx.svg_renderer())
        .map_err(|e| gpui_tray::Error::InvalidIcon(e.to_string()))?;
    let size = rendered.size(0);
    let (width, height) = (u32::from(size.width), u32::from(size.height));
    if width % SIZE != 0 || height != width {
        return Icon::from_gpui(&image, cx);
    }
    let factor = width / SIZE;
    // GPUI render images are BGRA; the tray wants RGBA (swapped below).
    let src = rendered
        .as_bytes(0)
        .ok_or_else(|| gpui_tray::Error::InvalidIcon("app icon has no frames".into()))?;
    let mut rgba = vec![0_u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            // Colour is weighted by alpha so transparent pixels don't
            // darken the rounded corners.
            let (mut color, mut alpha) = ([0_u32; 3], 0_u32);
            for sy in 0..factor {
                for sx in 0..factor {
                    let offset = (((y * factor + sy) * width + x * factor + sx) * 4) as usize;
                    let a = u32::from(src[offset + 3]);
                    for (channel, total) in color.iter_mut().enumerate() {
                        *total += u32::from(src[offset + channel]) * a;
                    }
                    alpha += a;
                }
            }
            let offset = ((y * SIZE + x) * 4) as usize;
            if alpha > 0 {
                // BGRA in, RGBA out.
                rgba[offset] = (color[2] / alpha) as u8;
                rgba[offset + 1] = (color[1] / alpha) as u8;
                rgba[offset + 2] = (color[0] / alpha) as u8;
                rgba[offset + 3] = (alpha / (factor * factor)) as u8;
            }
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE)
}

/// Whether `phase` has audio capture open, i.e. the tray should show the
/// red recording dot rather than the logo.
fn is_capturing(phase: RecordingPhase) -> bool {
    use RecordingPhase::*;
    matches!(phase, Starting | Recording | Paused | Stopping)
}

/// Swap the tray icon between the logo and the red dot to match `phase`.
fn sync_icon(phase: RecordingPhase, cx: &mut App) {
    let recording = is_capturing(phase);
    let Some(app_tray) = cx.try_global::<AppTray>() else {
        return;
    };
    if app_tray.showing_recording == recording {
        return;
    }
    let tray = app_tray.tray.clone();
    let icon = if recording {
        app_tray.recording_icon.clone()
    } else {
        app_tray.idle_icon.clone()
    };
    if let Err(e) = tray.set_icon(Some(icon), cx) {
        log::warn!("tray: failed to set icon: {}", e);
        return;
    }
    cx.global_mut::<AppTray>().showing_recording = recording;
}

/// A filled red circle: the "recording" indicator.
fn recording_icon() -> gpui_tray::Result<Icon> {
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

    // Meeting auto-detection hint: while a call is detected and nothing is
    // recording, offer a one-click "Record … call".
    let detected_call = if can_start {
        crate::meeting_detect::active_call(cx)
    } else {
        None
    };
    let status_label = match &detected_call {
        Some(call) => format!(
            "{} detected",
            crate::meeting_detect::call_label(call)
        ),
        None => status_label.to_string(),
    };

    let mut items = vec![
        disabled(MenuItem::action(status_label, NoAction), true),
        MenuItem::separator(),
    ];
    if let Some(call) = &detected_call {
        items.push(MenuItem::action(
            format!("Record {}", crate::meeting_detect::call_label(call)),
            TrayRecordDetectedCall,
        ));
    }
    items.extend([
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
        disabled(
            MenuItem::action("Bookmark this moment", TrayBookmark),
            !can_stop,
        ),
        MenuItem::separator(),
        MenuItem::action("Open Parley", TrayOpenParley),
        MenuItem::action("Settings", TraySettings),
        MenuItem::action("Check for updates", TrayCheckForUpdates),
        MenuItem::separator(),
        MenuItem::action("Quit", TrayQuit),
    ]);
    items
}

/// Bring the main window back: re-opens it if it has been closed (the app
/// keeps running without a window under `QuitMode::Explicit`), then asks the
/// compositor to focus it. Wayland compositors commonly refuse an
/// unsolicited raise, so the re-open is what actually gets the UI back.
pub(crate) fn open_parley(cx: &mut App) {
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

pub(crate) fn stop_recording(cx: &mut App) {
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
        stop_recording(cx);
    });
    cx.on_action(|_: &TrayPauseRecording, cx: &mut App| {
        log::info!("tray: pause recording");
        pause_recording_from_tray(cx);
    });
    cx.on_action(|_: &TrayResumeRecording, cx: &mut App| {
        log::info!("tray: resume recording");
        resume_recording_from_tray(cx);
    });
    cx.on_action(|_: &TrayBookmark, cx: &mut App| {
        log::info!("tray: bookmark");
        crate::bookmark::bookmark_now(cx);
    });
    cx.on_action(|_: &TrayRecordDetectedCall, cx: &mut App| {
        if let Some(call) = crate::meeting_detect::active_call(cx) {
            log::info!("tray: record detected {} call", call.app_name);
            crate::meeting_detect::record_call(cx, call, false);
        }
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

    let recording_icon = recording_icon()?;
    // A logo that won't decode isn't worth losing the tray over.
    let idle_icon = idle_icon(cx).unwrap_or_else(|e| {
        log::warn!("tray: failed to load the app icon, using the dot: {}", e);
        recording_icon.clone()
    });
    let tray = Tray::builder()
        .icon(idle_icon.clone())
        .title("Parley")
        .tooltip("Parley")
        .menu(build_menu)
        .build(cx)?;
    cx.set_global(AppTray {
        tray,
        idle_icon,
        recording_icon,
        showing_recording: false,
    });
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
                // start/stop). Detect the transition here, off the canonical
                // phase machine.
                if previous == RecordingPhase::Recording && snapshot.phase == RecordingPhase::Paused {
                    notifications::maybe_notify(cx, notifications::Kind::Paused);
                } else if previous == RecordingPhase::Paused && snapshot.phase == RecordingPhase::Recording {
                    notifications::maybe_notify(cx, notifications::Kind::Resumed);
                }
                cx.set_global(TrayPhase(snapshot.phase));
                sync_icon(snapshot.phase, cx);
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
pub(crate) fn refresh(cx: &mut App) {
    if let Some(tray) = cx.try_global::<AppTray>() {
        let tray = tray.tray.clone();
        if let Err(e) = tray.refresh_menu(cx) {
            log::warn!("tray: failed to refresh menu: {}", e);
        }
    }
}

/// Tear the tray down (best-effort). Called from `request_quit` before the
/// window closes, so the StatusNotifierItem disappears with the app.
pub fn shutdown(cx: &mut App) {
    if let Some(tray) = cx.try_global::<AppTray>() {
        let tray = tray.tray.clone();
        let _ = tray.close(cx);
    }
}
