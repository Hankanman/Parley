//! Action Items page: every open (or done, or all) action item across every
//! meeting, grouped by meeting — mirrors the frontend's
//! `app/action-items/page.tsx`. Lets the user check items off, edit their
//! text, delete them, add new ones, or jump to the owning meeting.
//!
//! Data comes straight from `ActionItemsRepository` / `MeetingsRepository`
//! (no Tauri command layer to go through); it refreshes on load and again
//! whenever the background extractor emits `action-items-extracted` or
//! `live-action-items`.

mod logic;

use gpui_kit::component::{
    ActiveTheme, Disableable as _, IconName, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState},
    v_flex,
};
use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::base::StyledExt as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use parley_core::database::models::ActionItem;
use parley_core::database::repositories::action_item::{
    ActionItemsRepository, NewActionItem, STATUS_DONE, STATUS_OPEN, SOURCE_MANUAL,
};
use parley_core::database::repositories::meeting::MeetingsRepository;

use crate::app_state::AppServices;
use crate::core_events::CoreEvent;
use crate::runtime::Io;
use crate::shell::{self, Route};
use logic::{Filter, MeetingRef};

/// Core events that mean the action-item list may be stale.
const REFRESH_EVENTS: &[&str] = &["action-items-extracted", "live-action-items"];

pub struct ActionItemsView {
    items: Vec<ActionItem>,
    meetings: Vec<MeetingRef>,
    loading: bool,
    error: Option<String>,
    filter: Filter,
    /// Item currently shown with an editable text field, if any.
    editing_id: Option<String>,
    edit_input: Entity<InputState>,
    /// Meeting currently showing an "add item" row, if any.
    adding_to: Option<String>,
    add_input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl ActionItemsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let edit_input = cx.new(|cx| InputState::new(window, cx).placeholder("Action item text"));
        let add_input = cx.new(|cx| InputState::new(window, cx).placeholder("Add an action item…"));

        let core_events = AppServices::global(cx).core_events.clone();
        let subscriptions = vec![cx.subscribe(&core_events, |this, _, event: &CoreEvent, cx| {
            if REFRESH_EVENTS.contains(&event.name.as_str()) {
                this.refresh(cx);
            }
        })];

        let mut this = Self {
            items: Vec::new(),
            meetings: Vec::new(),
            loading: true,
            error: None,
            filter: Filter::Open,
            editing_id: None,
            edit_input,
            adding_to: None,
            add_input,
            _subscriptions: subscriptions,
        };
        this.refresh(cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            self.error = Some("No database — complete onboarding in the Tauri app first.".into());
            self.loading = false;
            return;
        };
        self.loading = true;
        self.error = None;
        cx.notify();

        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let items_pool = pool.clone();
            let items_result =
                io.spawn(async move { ActionItemsRepository::list_all(&items_pool).await }).await;
            let meetings_result =
                io.spawn(async move { MeetingsRepository::get_meetings(&pool).await }).await;

            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match items_result {
                    Ok(Ok(items)) => this.items = items,
                    Ok(Err(e)) => this.error = Some(format!("Failed to load action items: {e}")),
                    Err(e) => this.error = Some(format!("Failed to load action items: {e}")),
                }
                match meetings_result {
                    Ok(Ok(meetings)) => {
                        this.meetings = meetings
                            .into_iter()
                            .map(|m| MeetingRef { id: m.id, title: m.title })
                            .collect();
                    }
                    Ok(Err(e)) => log::error!("Failed to load meetings for action items page: {e}"),
                    Err(e) => log::error!("Failed to load meetings for action items page: {e}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn toggle_status(&mut self, id: String, currently_done: bool, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let next = if currently_done { STATUS_OPEN } else { STATUS_DONE };
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.status = next.to_string();
        }
        cx.notify();

        let io = Io::global(cx);
        let id_for_task = id.clone();
        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move { ActionItemsRepository::set_status(&pool, &id_for_task, next).await })
                .await;
            if !matches!(result, Ok(Ok(Some(_)))) {
                let _ = this.update(cx, |this, cx| this.refresh(cx));
            }
        })
        .detach();
    }

    fn start_edit(&mut self, item: &ActionItem, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_id = Some(item.id.clone());
        let text = item.text.clone();
        self.edit_input.update(cx, |state, cx| state.set_value(text, window, cx));
        cx.notify();
    }

    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.editing_id = None;
        cx.notify();
    }

    fn save_edit(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.editing_id.take() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let text = self.edit_input.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.text = text.trim().to_string();
        }
        cx.notify();

        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move { ActionItemsRepository::update(&pool, &id, Some(text.trim()), None, None).await })
                .await;
            if !matches!(result, Ok(Ok(Some(_)))) {
                let _ = this.update(cx, |this, cx| this.refresh(cx));
            }
        })
        .detach();
    }

    fn confirm_delete(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            let id = id.clone();
            alert
                .title("Delete action item")
                .description("This permanently removes the item. This cannot be undone.")
                .show_cancel(true)
                .on_ok(move |_, _, cx| {
                    this.update(cx, |this, cx| this.delete(id.clone(), cx));
                    true
                })
        });
    }

    fn delete(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        self.items.retain(|i| i.id != id);
        cx.notify();

        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let result = io.spawn(async move { ActionItemsRepository::delete(&pool, &id).await }).await;
            if !matches!(result, Ok(Ok(true))) {
                let _ = this.update(cx, |this, cx| this.refresh(cx));
            }
        })
        .detach();
    }

    fn start_add(&mut self, meeting_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.adding_to = Some(meeting_id);
        self.add_input.update(cx, |state, cx| state.set_value("", window, cx));
        cx.notify();
    }

    fn cancel_add(&mut self, cx: &mut Context<Self>) {
        self.adding_to = None;
        cx.notify();
    }

    fn save_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(meeting_id) = self.adding_to.take() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let text = self.add_input.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }

        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let item = NewActionItem { text, ..Default::default() };
            let result = io
                .spawn(async move { ActionItemsRepository::create(&pool, &meeting_id, &item, SOURCE_MANUAL).await })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(_)) => this.refresh(cx),
                    Ok(Err(e)) => log::error!("Failed to add action item: {e}"),
                    Err(e) => log::error!("Failed to add action item: {e}"),
                }
            });
        })
        .detach();
        let _ = window;
    }
}

impl Render for ActionItemsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .child(self.render_header(cx))
            .child(self.render_body(cx))
    }
}

impl ActionItemsView {
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let open_count = self.items.iter().filter(|i| i.status != STATUS_DONE).count();
        let done_count = self.items.len() - open_count;

        h_flex()
            .w_full()
            .items_center()
            .gap_3()
            .p_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(IconName::CircleCheck.view(cx))
            .child(div().text_lg().font_semibold().child("Action items"))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{open_count} open · {done_count} done")),
            )
            .child(div().flex_1())
            .child(self.filter_button("Open", Filter::Open, cx))
            .child(self.filter_button("Done", Filter::Done, cx))
            .child(self.filter_button("All", Filter::All, cx))
    }

    fn filter_button(&self, label: &'static str, value: Filter, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.filter == value;
        let button = Button::new(format!("filter-{label}")).label(label);
        let button = if active { button.primary() } else { button.ghost() };
        button.on_click(cx.listener(move |this, _, _, cx| {
            this.filter = value;
            cx.notify();
        }))
    }

    fn render_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.loading {
            return div().size_full().p_6().child("Loading action items…").into_any_element();
        }
        if let Some(err) = &self.error {
            return div()
                .size_full()
                .p_6()
                .text_color(cx.theme().danger)
                .child(err.clone())
                .into_any_element();
        }

        let filtered: Vec<ActionItem> = self
            .items
            .iter()
            .filter(|i| logic::matches_filter(i, self.filter))
            .cloned()
            .collect();
        let groups = logic::group_by_meeting(&filtered, &self.meetings);

        if groups.is_empty() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .p_6()
                .text_color(cx.theme().muted_foreground)
                .child("No action items here.")
                .into_any_element();
        }

        div()
            .id("action-items-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(
                v_flex().w_full().gap_5().p_4().children(
                    groups.into_iter().map(|group| self.render_group(group, cx)),
                ),
            )
            .into_any_element()
    }

    fn render_group(&self, group: logic::Group<'_>, cx: &mut Context<Self>) -> AnyElement {
        let meeting_id = group.meeting_id.clone();
        let count = group.items.len();

        v_flex()
            .w_full()
            .gap_2()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new(format!("group-title-{meeting_id}"))
                            .ghost()
                            .label(group.title.clone())
                            .on_click(cx.listener(move |_, _, _, cx| {
                                shell::navigate(Route::Meeting(meeting_id.clone()), cx);
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(count.to_string()),
                    ),
            )
            .children(group.items.iter().map(|item| self.render_item(item, cx)))
            .child(self.render_add_row(group.meeting_id, cx))
            .into_any_element()
    }

    fn render_item(&self, item: &ActionItem, cx: &mut Context<Self>) -> AnyElement {
        let is_done = item.status == STATUS_DONE;
        let id = item.id.clone();

        if self.editing_id.as_deref() == Some(item.id.as_str()) {
            return h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().border)
                .child(div().flex_1().min_w_0().child(Input::new(&self.edit_input)))
                .child(
                    Button::new(format!("save-item-{id}"))
                        .primary()
                        .label("Save")
                        .on_click(cx.listener(|this, _, _, cx| this.save_edit(cx))),
                )
                .child(
                    Button::new(format!("cancel-item-{id}"))
                        .ghost()
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_edit(cx))),
                )
                .into_any_element();
        }

        let id_for_toggle = id.clone();
        let id_for_edit = id.clone();
        let id_for_delete = id.clone();

        h_flex()
            .w_full()
            .items_start()
            .gap_2()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .child(
                Checkbox::new(format!("done-{id}")).checked(is_done).on_click(cx.listener(
                    move |this, _, _, cx| this.toggle_status(id_for_toggle.clone(), is_done, cx),
                )),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .when(is_done, |this| {
                                this.line_through().text_color(cx.theme().muted_foreground)
                            })
                            .child(item.text.clone()),
                    )
                    .when(item.assignee.is_some() || item.due_hint.is_some(), |this| {
                        this.child(
                            h_flex()
                                .gap_2()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .when_some(item.assignee.clone(), |this, a| this.child(a))
                                .when_some(item.due_hint.clone(), |this, d| this.child(d)),
                        )
                    }),
            )
            .child(
                Button::new(format!("edit-item-{id}"))
                    .ghost()
                    .icon(AssetIcon::Pencil)
                    .tooltip("Edit")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let id = id_for_edit.clone();
                        if let Some(item) = this.items.iter().find(|i| i.id == id).cloned() {
                            this.start_edit(&item, window, cx);
                        }
                    })),
            )
            .child(
                Button::new(format!("delete-item-{id}"))
                    .ghost()
                    .icon(IconName::Delete)
                    .tooltip("Delete")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_delete(id_for_delete.clone(), window, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_add_row(&self, meeting_id: String, cx: &mut Context<Self>) -> impl IntoElement {
        if self.adding_to.as_deref() == Some(meeting_id.as_str()) {
            return h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .child(div().flex_1().min_w_0().child(Input::new(&self.add_input)))
                .child(
                    Button::new(format!("save-add-{meeting_id}"))
                        .primary()
                        .label("Add")
                        .on_click(cx.listener(|this, _, window, cx| this.save_add(window, cx))),
                )
                .child(
                    Button::new(format!("cancel-add-{meeting_id}"))
                        .ghost()
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_add(cx))),
                )
                .into_any_element();
        }

        Button::new(format!("add-item-{meeting_id}"))
            .ghost()
            .icon(IconName::Plus)
            .label("Add item")
            .disabled(self.adding_to.is_some())
            .on_click(cx.listener(move |this, _, window, cx| {
                this.start_add(meeting_id.clone(), window, cx);
            }))
            .into_any_element()
    }
}
