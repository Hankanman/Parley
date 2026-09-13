//! Opening (and re-opening) the main window.
//!
//! Startup and the tray's "Open Parley" both go through [`open_main_window`]
//! so the window is always set up identically: saved theme applied, the
//! close handler registered, and the [`crate::tray::MainWindow`] global
//! pointed at the live handle.
//!
//! Re-opening matters because the window can genuinely go away while the app
//! keeps running: the app uses `QuitMode::Explicit`, so closing the last
//! window doesn't quit, and GPUI's Wayland backend closes a window outright
//! when no `should_close` callback is registered. Without a way back, the
//! app becomes a tray-only zombie ("window not found" from Open Parley).

use gpui_kit::component::{Root, TitleBar};
use gpui_kit::*;

use crate::root::RootView;

/// Open the main window, register its handlers, and store the handle in the
/// `MainWindow` global. Returns the new handle.
pub fn open_main_window(cx: &mut App) -> Result<WindowHandle<Root>> {
    let mut options = TitleBar::window_options();
    options.window_bounds = Some(WindowBounds::centered(size(px(1100.), px(720.)), cx));
    // Matches the desktop entry's name and `StartupWMClass`, so the dock
    // pairs the window with Parley's icon.
    options.app_id = Some(parley_core::paths::APP_IDENTIFIER.to_string());
    // Prefer the system's own title bar and borders. GPUI asks the
    // compositor for server-side decorations and silently falls back to
    // client-side when it can't provide them (GNOME/Wayland, notably) —
    // gpui-kit's `TitleBar` then draws our own controls, and skips them
    // when the compositor is drawing its own, so there's never a double
    // title bar either way.
    options.window_decorations = Some(WindowDecorations::Server);

    let window = cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| RootView::new(window, cx));
        cx.new(|cx| Root::new(view, window, cx))
    })?;

    // Registering the close handler is not optional: it's what puts a close
    // request under [`close_to_tray_or_quit`] instead of the Wayland default
    // of destroying the window regardless. Loud on failure — silence here is
    // exactly what hid that bug.
    if let Err(e) = window.update(cx, |_, window, cx| {
        crate::views::settings::init_theme(window, cx);
        window.on_window_should_close(cx, |_, cx| close_to_tray_or_quit(cx));
    }) {
        log::error!(
            "Failed to register main-window handlers ({e}); closing the window \
             would skip finishing an active recording"
        );
    }

    // Report what we actually got, so "is this using system decorations /
    // the system theme?" is answerable from the log.
    let _ = window.update(cx, |_, window, cx| {
        let decorations = match window.window_decorations() {
            Decorations::Server => "server-side (system)",
            Decorations::Client { .. } => "client-side (drawn by the app)",
        };
        log::info!(
            "Main window: {decorations} decorations, system appearance {:?}",
            cx.window_appearance()
        );
    });

    cx.set_global(crate::tray::MainWindow(window));
    Ok(window)
}

/// What a close request should do, for both close paths (the compositor's
/// close button and gpui-kit's own title-bar X). With a tray registered the
/// window closes and the app keeps running — an active recording continues,
/// and the tray's "Open Parley" brings the UI back. Without a tray there'd
/// be no way back, so a close means quit: [`crate::request_quit`] finishes
/// any recording first.
///
/// Returns whether the window may actually close.
pub fn close_to_tray_or_quit(cx: &mut App) -> bool {
    if crate::tray::is_installed(cx) {
        log::info!("Main window closed to tray; the app keeps running");
        true
    } else {
        crate::request_quit(cx);
        false
    }
}

/// The live main window, re-opening it if it has gone away. `None` only if
/// opening a window failed outright.
pub fn ensure_main_window(cx: &mut App) -> Option<WindowHandle<Root>> {
    if let Some(handle) = cx.try_global::<crate::tray::MainWindow>().map(|main| main.0) {
        // `is_window_active` is just a cheap liveness probe here: it errors
        // when the handle's window is no longer in the app's window map.
        if handle.update(cx, |_, _, _| ()).is_ok() {
            return Some(handle);
        }
        log::info!("Main window was closed; re-opening it");
    }

    match open_main_window(cx) {
        Ok(handle) => Some(handle),
        Err(e) => {
            log::error!("Failed to open the main window: {}", e);
            None
        }
    }
}
