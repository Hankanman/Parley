//! "Bookmark this moment" while recording (issue #87): one entry point
//! shared by the recording view's button, the tray item and the keyboard
//! shortcut.
//!
//! GPUI has no system-wide hotkey API (and Wayland compositors only grant
//! those through the GlobalShortcuts portal), so the shortcut is an
//! app-level key binding: it works whenever a Parley window has focus,
//! whatever view is showing. The new bookmark is announced through
//! `parley_core::bookmarks::BOOKMARKS_CHANGED_EVENT`, which the recording
//! view turns into a confirmation toast.

use gpui_kit::*;

use crate::app_state::AppServices;

actions!(parley_bookmark, [BookmarkMoment]);

/// The shortcut, shown in tooltips.
pub const SHORTCUT_LABEL: &str = "Ctrl+Shift+B";

/// Register the action handler and its key binding. Call once at startup.
pub fn install(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("ctrl-shift-b", BookmarkMoment, None)]);
    cx.on_action(|_: &BookmarkMoment, cx: &mut App| bookmark_now(cx));
}

/// Bookmark the current moment of the recording in progress, if any.
/// Failures (nothing recording, no database) are logged and otherwise
/// ignored — a stray keypress must never interrupt anything.
pub fn bookmark_now(cx: &mut App) {
    let services = AppServices::global(cx);
    let sink = services.sink.clone();
    let pool = services.pool();
    services.io.spawn(async move {
        match parley_core::bookmarks::bookmark_now(sink, pool, None).await {
            Ok(b) => log::info!(
                "bookmark: {} at {:.1}s in meeting {}",
                b.id,
                b.offset_seconds,
                b.meeting_id
            ),
            Err(e) => log::info!("bookmark: not added: {e}"),
        }
    });
}
