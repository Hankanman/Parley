//! "Calendar" settings page: ICS calendar sources — list, add, remove,
//! refresh, and last sync status/errors.
//!
//! Mirrors `frontend/src/components/CalendarSettings.tsx` against the same
//! Tauri-free core (`parley_core::calendar`): `CalendarRepository` for
//! list/add/remove, and the new `calendar::refresh_source` helper (moved
//! out of the `calendar_refresh_source` Tauri command, which is now a thin
//! wrapper over it) for refresh + status.

use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
    input::{Input, InputState},
    label::Label,
    setting::{SettingGroup, SettingItem, SettingPage},
    ActiveTheme, Disableable as _, Icon, Sizable as _,
};
use gpui_kit::*;

use super::state::SettingsCache;
use super::SettingsView;

pub fn page(view: &Entity<SettingsView>, cx: &mut Context<SettingsView>) -> SettingPage {
    let _ = cx;
    let view = view.clone();

    let mut items = vec![add_source_item(&view)];
    let source_count = SettingsCache::global(cx).calendar_sources.len();
    for i in 0..source_count {
        items.push(source_row_item(&view, i));
    }

    SettingPage::new("Calendar")
        .icon(Icon::new(gpui_kit::assets::IconName::Calendar))
        .group(
            SettingGroup::new()
                .title("ICS calendar sources")
                .description("Public/private ICS feed URLs (Google Calendar \"secret address\", Outlook \"publish\" link, etc.) used to match meetings to calendar events.")
                .items(items),
        )
}

fn add_source_item(view: &Entity<SettingsView>) -> SettingItem {
    let view = view.clone();
    SettingItem::render(move |_options, window, cx| {
        struct AddState {
            url: Entity<InputState>,
            label: Entity<InputState>,
        }

        let state = window.use_keyed_state(SharedString::from("calendar-add-source"), cx, |window, cx| {
            AddState {
                url: cx.new(|cx| InputState::new(window, cx).placeholder("https://.../calendar.ics")),
                label: cx.new(|cx| InputState::new(window, cx).placeholder("Label (optional)")),
            }
        });
        let url_input = state.read(cx).url.clone();
        let label_input = state.read(cx).label.clone();

        let view_for_click = view.clone();
        let url_for_click = url_input.clone();
        let label_for_click = label_input.clone();

        h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .child(Input::new(&url_input).small().flex_1())
            .child(Input::new(&label_input).small().w_40())
            .child(Button::new("calendar-add").outline().small().label("Add").on_click(
                move |_, window, cx| {
                    let url = url_for_click.read(cx).value().to_string();
                    let label = label_for_click.read(cx).value().to_string();
                    if url.trim().is_empty() {
                        return;
                    }
                    url_for_click.update(cx, |input, cx| input.set_value("", window, cx));
                    label_for_click.update(cx, |input, cx| input.set_value("", window, cx));
                    super::state::calendar_add(
                        cx,
                        view_for_click.clone(),
                        url,
                        if label.trim().is_empty() { None } else { Some(label) },
                    );
                },
            ))
            .into_any_element()
    })
    .keywords(["add calendar", "ics", "calendar source"])
}

fn source_row_item(view: &Entity<SettingsView>, index: usize) -> SettingItem {
    let view = view.clone();
    SettingItem::render(move |_options, _window, cx| {
        let cache = SettingsCache::global(cx);
        let Some(source) = cache.calendar_sources.get(index).cloned() else {
            return div().into_any_element();
        };
        let busy = cache.calendar_busy.get(&source.id).copied().unwrap_or(false);

        let status: SharedString = if let Some(err) = &source.last_error {
            SharedString::from(format!("Error: {}", err))
        } else if let Some(at) = &source.last_fetched_at {
            SharedString::from(format!("Last synced {}", at))
        } else {
            SharedString::from("Never synced")
        };
        let status_color = if source.last_error.is_some() {
            cx.theme().danger
        } else {
            cx.theme().muted_foreground
        };

        let refresh_view = view.clone();
        let refresh_id = source.id.clone();
        let remove_view = view.clone();
        let remove_id = source.id.clone();

        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                v_flex()
                    .flex_1()
                    .gap_0p5()
                    .child(Label::new(source.label.clone().unwrap_or_else(|| source.url.clone())))
                    .child(Label::new(status).text_color(status_color).text_xs()),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new(SharedString::from(format!("calendar-refresh-{}", source.id)))
                            .ghost()
                            .small()
                            .icon(gpui_kit::assets::IconName::RefreshCw)
                            .disabled(busy)
                            .on_click(move |_, _, cx| {
                                super::state::calendar_refresh(cx, refresh_view.clone(), refresh_id.clone());
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("calendar-remove-{}", source.id)))
                            .ghost()
                            .small()
                            .icon(gpui_kit::assets::IconName::Trash)
                            .on_click(move |_, _, cx| {
                                super::state::calendar_remove(cx, remove_view.clone(), remove_id.clone());
                            }),
                    ),
            )
            .into_any_element()
    })
}
