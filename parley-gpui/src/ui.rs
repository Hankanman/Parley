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

// ----------------------------------------------------------------------
// Shared visual building blocks, so every page has the same rhythm.
// ----------------------------------------------------------------------

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use gpui_kit::component::{h_flex, v_flex, ActiveTheme, Icon, Sizable as _};
use gpui_kit::base::StyledExt as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    div, img, px, Animation, AnimationExt as _, Div, ElementId, Hsla, Image, ImageFormat, IntoElement,
    ParentElement, SharedString, Styled,
};

/// Page entrance: content rises 6 px into place while fading in.
pub const ENTER: Duration = Duration::from_millis(220);

/// The app icon as a GPUI image (decoded once).
pub fn logo() -> Arc<Image> {
    static LOGO: OnceLock<Arc<Image>> = OnceLock::new();
    LOGO.get_or_init(|| {
        Arc::new(Image::from_bytes(
            ImageFormat::Png,
            include_bytes!("../packaging/linux/icon.png").to_vec(),
        ))
    })
    .clone()
}

/// The app icon at `size` px, with rounded corners to match its artwork.
pub fn logo_mark(size: f32) -> impl IntoElement {
    img(logo()).size(px(size)).rounded(px(size * 0.22))
}

/// Fade `element` in and slide it up into place. `id` must change for the
/// animation to replay (e.g. include a route or item key).
pub fn fade_in<E: IntoElement + Styled + 'static>(
    element: E,
    id: impl Into<ElementId>,
) -> impl IntoElement {
    element.with_animation(
        id,
        Animation::new(ENTER).with_easing(gpui_kit::ease_out_quint()),
        |el, delta| el.opacity(delta).relative().top(px(6. * (1. - delta))),
    )
}

/// A dot that breathes, for "live" states (recording, listening).
pub fn pulse_dot(color: Hsla, size: f32, id: impl Into<ElementId>) -> impl IntoElement {
    div()
        .size(px(size))
        .rounded_full()
        .bg(color)
        .with_animation(
            id,
            Animation::new(Duration::from_millis(1400))
                .repeat()
                .with_easing(gpui_kit::pulsating_between(0.35, 1.0)),
            |el, delta| el.opacity(delta),
        )
}

/// Page title row: a large title, an optional muted subtitle, and room for
/// actions on the right.
pub fn page_header(
    title: impl Into<SharedString>,
    subtitle: Option<SharedString>,
    cx: &gpui_kit::App,
) -> Div {
    h_flex().w_full().items_start().gap_4().child(
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_1()
            .child(div().text_2xl().font_semibold().child(title.into()))
            .when_some(subtitle, |this, subtitle| {
                this.child(div().text_sm().text_color(cx.theme().muted_foreground).child(subtitle))
            }),
    )
}

/// A raised surface: card background, hairline border, large radius.
pub fn card(cx: &gpui_kit::App) -> Div {
    v_flex()
        .gap_3()
        .p_4()
        .rounded(cx.theme().radius_lg)
        .bg(cx.theme().group_box)
        .border_1()
        .border_color(cx.theme().border)
}

/// Small uppercase label above a group of content.
pub fn section_label(text: impl Into<SharedString>, cx: &gpui_kit::App) -> Div {
    div()
        .text_xs()
        .font_semibold()
        .text_color(cx.theme().muted_foreground)
        .child(text.into().to_uppercase())
}

/// Centered placeholder for an empty list or page: an icon in a soft
/// accent circle, a title and a line of help.
pub fn empty_state(
    icon: impl Into<Icon>,
    title: impl Into<SharedString>,
    body: impl Into<SharedString>,
    cx: &gpui_kit::App,
) -> Div {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_3()
        .p_8()
        .child(
            div()
                .size(px(56.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(cx.theme().primary.opacity(0.12))
                .text_color(cx.theme().primary)
                .child(icon.into().large()),
        )
        .child(div().text_lg().font_semibold().child(title.into()))
        .child(
            div()
                .max_w(px(420.))
                .text_sm()
                .text_center()
                .text_color(cx.theme().muted_foreground)
                .child(body.into()),
        )
}

/// Level-meter fill for a 0–1 level: green while comfortable, amber when
/// hot, red near clipping.
pub fn level_color(level: f32, cx: &gpui_kit::App) -> Hsla {
    match level {
        l if l >= 0.92 => cx.theme().danger,
        l if l >= 0.75 => cx.theme().warning,
        _ => cx.theme().success,
    }
}

/// A compact metadata pill: small icon plus muted text (dates, places,
/// counts).
pub fn meta_chip(
    icon: impl Into<Icon>,
    text: impl Into<SharedString>,
    cx: &gpui_kit::App,
) -> Div {
    h_flex()
        .gap_1p5()
        .items_center()
        .px_2()
        .py_0p5()
        .rounded_full()
        .bg(cx.theme().muted)
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(icon.into().xsmall())
        .child(text.into())
}
