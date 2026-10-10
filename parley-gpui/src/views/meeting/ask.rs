//! "Ask Parley" panel: chat with the open meeting (issue #80).
//!
//! A self-contained entity the meeting page shows as the "Ask" tab of its
//! right-hand pane. It loads and stores the per-meeting history through
//! `parley_core::chat`, sends questions with the configured summary model,
//! renders streamed tokens as they arrive (built-in model) and turns every
//! `[mm:ss]` citation in an answer into a link. Clicking one emits
//! [`AskPanelEvent::JumpTo`], which the meeting page answers by scrolling
//! the transcript to that moment.
//!
//! Messages are variable-height (wrapped markdown), so the list is a
//! `list()` + `ListState`, bottom-aligned so it reads like a chat.

use std::collections::HashSet;

use gpui_kit::component::{
    ActiveTheme, Disableable as _, IconName,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    text::TextView,
    v_flex,
};
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::IconNameExt as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use parley_core::chat::context::TranscriptCoverage;
use parley_core::chat::repository::{MeetingChatMessage, MeetingChatRepository};
use parley_core::chat::{self, CHAT_STREAM_EVENT};

use crate::app_state::AppServices;
use crate::runtime::Io;

type Lucide = gpui_kit::assets::IconName;

const LIST_OVERDRAW: Pixels = px(400.);

/// One-click starters shown while a meeting has no chat yet.
const SUGGESTIONS: &[&str] = &[
    "What decisions were made?",
    "What are the action items, and who owns them?",
    "Draft a short follow-up email to the attendees.",
];

pub enum AskPanelEvent {
    /// A citation was clicked: show the transcript at this many seconds.
    JumpTo(f64),
}

impl EventEmitter<AskPanelEvent> for AskPanel {}

/// What one list row shows.
#[derive(Clone)]
enum Row {
    Message(MeetingChatMessage),
    /// The question in flight (not yet confirmed stored).
    PendingQuestion(String),
    /// The answer being generated: streamed text so far, possibly empty.
    PendingAnswer(String),
}

pub struct AskPanel {
    meeting_id: Option<String>,
    /// Bumped on every `set_meeting`; stale async completions compare it.
    generation: u64,
    messages: Vec<MeetingChatMessage>,
    loading: bool,
    list_state: ListState,
    input: Entity<InputState>,
    /// Meetings with an ask in flight. Keyed by id so navigating away and
    /// back mid-answer still shows the busy state.
    in_flight: HashSet<String>,
    /// The current meeting's in-flight question, shown until it resolves.
    pending_question: Option<String>,
    /// Streamed answer text for the current meeting's in-flight ask.
    streaming: String,
    error: Option<String>,
    /// Shown under the list after an answer built from excerpts only.
    coverage_note: Option<String>,
    clearing: bool,
    _subscriptions: Vec<Subscription>,
}

impl AskPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Ask about this meeting…")
        });
        let core_events = AppServices::global(cx).core_events.clone();
        let subscriptions = vec![
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.send(window, cx);
                }
            }),
            cx.subscribe(&core_events, |this, _, event, cx| {
                if event.name == CHAT_STREAM_EVENT {
                    this.on_stream(event, cx);
                }
            }),
        ];
        Self {
            meeting_id: None,
            generation: 0,
            messages: Vec::new(),
            loading: false,
            list_state: ListState::new(0, ListAlignment::Bottom, LIST_OVERDRAW),
            input,
            in_flight: HashSet::new(),
            pending_question: None,
            streaming: String::new(),
            error: None,
            coverage_note: None,
            clearing: false,
            _subscriptions: subscriptions,
        }
    }

    /// Show `id`'s chat (called by the meeting page on every load).
    pub fn set_meeting(&mut self, id: String, cx: &mut Context<Self>) {
        self.generation += 1;
        self.meeting_id = Some(id);
        self.messages.clear();
        self.pending_question = None;
        self.streaming.clear();
        self.error = None;
        self.coverage_note = None;
        self.clearing = false;
        self.reload(cx);
    }

    fn is_busy(&self) -> bool {
        self.meeting_id
            .as_ref()
            .is_some_and(|id| self.in_flight.contains(id))
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self.messages.iter().cloned().map(Row::Message).collect();
        if self.is_busy() {
            if let Some(q) = &self.pending_question {
                rows.push(Row::PendingQuestion(q.clone()));
            }
            rows.push(Row::PendingAnswer(self.streaming.clone()));
        }
        rows
    }

    /// Re-sync the list after rows were added or removed (keeps it pinned
    /// to the newest message — the list is bottom-aligned).
    fn sync_list(&mut self) {
        self.list_state.reset(self.rows().len());
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        self.loading = true;
        self.sync_list();
        cx.notify();
        let io = Io::global(cx);
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move { MeetingChatRepository::list_by_meeting(&pool, &id).await })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(Ok(messages)) => this.messages = messages,
                    Ok(Err(e)) => this.error = Some(format!("Failed to load chat: {e}")),
                    Err(e) => this.error = Some(format!("Failed to load chat: {e}")),
                }
                this.sync_list();
                cx.notify();
            });
        })
        .detach();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let question = self.input.read(cx).value().trim().to_string();
        self.ask(question, window, cx);
    }

    fn ask(&mut self, question: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        if question.is_empty() || self.is_busy() || self.clearing {
            return;
        }
        let Some(pool) = AppServices::global(cx).pool() else {
            self.error = Some("No database — complete onboarding first.".into());
            cx.notify();
            return;
        };
        let sink = AppServices::global(cx).sink.clone();
        let io = Io::global(cx);

        self.in_flight.insert(id.clone());
        self.pending_question = Some(question.clone());
        self.streaming.clear();
        self.error = None;
        self.coverage_note = None;
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        self.sync_list();
        cx.notify();

        let asked_id = id.clone();
        cx.spawn(async move |this, cx| {
            let q = question.clone();
            let result = io
                .spawn(async move { chat::ask_meeting(sink, pool, id, q).await })
                .await
                .unwrap_or_else(|e| Err(format!("Task failed: {e}")));
            let _ = this.update_in(cx, |this, window, cx| {
                this.in_flight.remove(&asked_id);
                if this.meeting_id.as_deref() != Some(asked_id.as_str()) {
                    // The user moved on; the stored turns show up when they
                    // come back.
                    return;
                }
                this.pending_question = None;
                this.streaming.clear();
                match result {
                    Ok(outcome) => {
                        this.messages.push(outcome.question);
                        this.messages.push(outcome.answer);
                        if let TranscriptCoverage::Excerpts { selected, total } = outcome.coverage {
                            this.coverage_note = Some(format!(
                                "Long meeting: answered from the {selected} of {total} transcript sections that best match the question."
                            ));
                        }
                    }
                    Err(e) => {
                        // The question was rolled back out of history; hand
                        // it back so the user can retry or edit it.
                        if this.input.read(cx).value().trim().is_empty() {
                            this.input.update(cx, |s, cx| s.set_value(question.clone(), window, cx));
                        }
                        if e != "Stopped." {
                            this.error = Some(e);
                        }
                    }
                }
                this.sync_list();
                cx.notify();
            });
        })
        .detach();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = &self.meeting_id {
            chat::cancel_ask(id);
        }
        cx.notify();
    }

    fn clear_chat(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        if self.is_busy() || self.clearing {
            return;
        }
        self.clearing = true;
        cx.notify();
        let io = Io::global(cx);
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move { MeetingChatRepository::clear(&pool, &id).await })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.clearing = false;
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(Ok(_)) => {
                        this.messages.clear();
                        this.error = None;
                        this.coverage_note = None;
                    }
                    Ok(Err(e)) => this.error = Some(format!("Failed to clear chat: {e}")),
                    Err(e) => this.error = Some(format!("Failed to clear chat: {e}")),
                }
                this.sync_list();
                cx.notify();
            });
        })
        .detach();
    }

    fn on_stream(&mut self, event: &crate::core_events::CoreEvent, cx: &mut Context<Self>) {
        #[derive(serde::Deserialize)]
        struct Delta {
            meeting_id: String,
            delta: String,
        }
        let Some(payload) = event.decode::<Delta>() else {
            return;
        };
        if self.meeting_id.as_deref() != Some(payload.meeting_id.as_str()) || !self.is_busy() {
            return;
        }
        self.streaming.push_str(&payload.delta);
        // Only the last row (the answer in progress) changed height.
        let count = self.rows().len();
        if count > 0 && self.list_state.item_count() == count {
            self.list_state.splice(count - 1..count, 1);
        } else {
            self.sync_list();
        }
        cx.notify();
    }
}

impl Render for AskPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.is_busy();
        let rows = self.rows();
        let is_empty = rows.is_empty();
        let has_messages = !self.messages.is_empty();

        let header = h_flex()
            .w_full()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_2()
            .p_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().font_semibold().child("Ask Parley"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Answers use your summary model and cite the transcript."),
                    ),
            )
            .child(
                Button::new("ask-clear-chat")
                    .ghost()
                    .icon(Lucide::Eraser)
                    .label("Clear chat")
                    .loading(self.clearing)
                    .disabled(!has_messages || busy || self.clearing)
                    .on_click(cx.listener(|this, _, _, cx| this.clear_chat(cx))),
            );

        let body: AnyElement = if self.loading && is_empty {
            div()
                .flex_1()
                .p_4()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Loading chat…")
                .into_any_element()
        } else if is_empty {
            self.render_empty_state(cx)
        } else {
            let entity = cx.entity().downgrade();
            list(self.list_state.clone(), move |ix, _window, cx| {
                render_row(ix, &rows[ix], entity.clone(), cx)
            })
            .flex_1()
            .size_full()
            .into_any_element()
        };

        let composer = v_flex()
            .w_full()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .when_some(self.coverage_note.clone(), |this, note| {
                this.child(div().text_xs().text_color(cx.theme().muted_foreground).child(note))
            })
            .when_some(self.error.clone(), |this, err| {
                this.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_start()
                        .justify_between()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .child(err),
                        )
                        .child(
                            Button::new("ask-dismiss-error")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .tooltip("Dismiss")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().min_w_0().child(Input::new(&self.input).disabled(busy)))
                    .when(!busy, |this| {
                        this.child(
                            Button::new("ask-send")
                                .primary()
                                .icon(Lucide::SendHorizontal)
                                .tooltip("Send (Enter)")
                                .disabled(self.clearing || self.meeting_id.is_none())
                                .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                        )
                    })
                    .when(busy, |this| {
                        this.child(
                            Button::new("ask-stop")
                                .outline()
                                .icon(Lucide::Square)
                                .tooltip("Stop")
                                .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                        )
                    }),
            );

        v_flex()
            .size_full()
            .child(header)
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .child(composer)
    }
}

impl AskPanel {
    fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut col = v_flex()
            .flex_1()
            .gap_3()
            .p_4()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Lucide::Sparkles.view(cx))
                    .child(div().text_sm().font_semibold().child("Ask anything about this meeting")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Answers cite timestamps like [12:34]; click one to jump to that moment in the transcript."),
            );
        for (ix, suggestion) in SUGGESTIONS.iter().enumerate() {
            let text = suggestion.to_string();
            col = col.child(
                Button::new(SharedString::from(format!("ask-suggestion-{ix}")))
                    .outline()
                    .small()
                    .label(*suggestion)
                    .disabled(self.meeting_id.is_none())
                    .on_click(cx.listener(move |this, _, window, cx| this.ask(text.clone(), window, cx))),
            );
        }
        col.into_any_element()
    }
}

fn render_row(ix: usize, row: &Row, entity: WeakEntity<AskPanel>, cx: &mut App) -> AnyElement {
    let theme = cx.theme();
    let user_bubble = |text: String| {
        h_flex().w_full().justify_end().px_4().py_2().child(
            div()
                .max_w(relative(0.85))
                .px_3()
                .py_2()
                .rounded_lg()
                .bg(theme.primary.opacity(0.12))
                .text_sm()
                .child(text),
        )
    };
    match row {
        Row::Message(m) if m.is_user() => user_bubble(m.content.clone()).into_any_element(),
        Row::PendingQuestion(q) => user_bubble(q.clone()).into_any_element(),
        Row::Message(m) => assistant_row(
            SharedString::from(format!("ask-msg-{}", m.id)),
            &m.content,
            entity,
        )
        .into_any_element(),
        Row::PendingAnswer(text) if text.trim().is_empty() => div()
            .px_4()
            .py_2()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child("Thinking…")
            .into_any_element(),
        Row::PendingAnswer(text) => assistant_row(
            SharedString::from(format!("ask-streaming-{ix}")),
            text,
            entity,
        )
        .into_any_element(),
    }
}

/// An answer rendered as markdown, its `[mm:ss]` citations turned into
/// links that jump the transcript instead of opening anything.
fn assistant_row(id: SharedString, content: &str, entity: WeakEntity<AskPanel>) -> impl IntoElement {
    let markdown = chat::link_citations(content);
    div().w_full().px_4().py_2().text_sm().child(
        TextView::markdown(id, markdown)
            .selectable(true)
            .on_link_click(move |url, _, _, cx| {
                // Only citations act. Answers are model output steered by
                // transcript text, so nothing else in them is opened.
                if let Some(seconds) = chat::citation_link_seconds(url) {
                    let _ = entity.update(cx, |_, cx| cx.emit(AskPanelEvent::JumpTo(seconds)));
                }
            }),
    )
}
