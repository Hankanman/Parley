//! Small UI helpers shared across views.

use gpui_kit::{AnyWindowHandle, App, Window};

/// Run `f` against the main window from a context that only has `&mut App`
/// (a core-event subscription callback, or an async task after an `.await`)
/// — via the `tray::MainWindow` global stashed at startup. No-op before the
/// window exists.
///
/// Goes through the *untyped* handle on purpose: `WindowHandle<Root>::update`
/// leases the `Root` entity for the closure, and gpui-kit's window helpers
/// (`push_notification`, `open_dialog`, `close_dialog`, …) update `Root`
/// themselves, which then panics with "cannot update Root while it is
/// already being updated". `AnyWindowHandle::update` only enters the window.
pub fn with_main_window(cx: &mut App, f: impl FnOnce(&mut Window, &mut App)) {
    let Some(handle) = cx.try_global::<crate::tray::MainWindow>().map(|main| main.0) else {
        return;
    };
    let handle: AnyWindowHandle = handle.into();
    let _ = handle.update(cx, |_, window, cx| f(window, cx));
}
