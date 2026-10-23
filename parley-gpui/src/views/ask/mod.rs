//! Global "Ask Parley": questions across every meeting (issue #81).
//!
//! A chat page reachable from the sidebar. Each question runs
//! `parley_core::chat::across::ask_across` with the filters from the bar
//! above the chat (person, date range, calendar series). Answers render as
//! markdown with `[M1 12:34]` citations turned into links; clicking one, or
//! one of the source chips under the answer, opens that meeting with its
//! transcript scrolled to the cited moment.
//!
//! History lives in this view's memory for the session (see the
//! `chat::across` docs for why it isn't stored) and is passed back in for
//! follow-up questions. The row layout mirrors the per-meeting Ask panel
//! (`views/meeting/ask.rs`).

mod logic;

use gpui_kit::base::StyledExt as _;
use gpui_kit::component::IconNameExt as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::{
    ActiveTheme, Disableable as _, Icon, IconName, IndexPath,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    select::{Select, SelectEvent, SelectItem, SelectState},
    text::TextView,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use parley_core::chat::across::{
    self, AskFilters, PersonFilter, SeriesFilter, SourceRef, ASK_ACROSS_STREAM_EVENT,
};
use parley_core::chat::context::ChatTurn;

use crate::app_state::AppServices;
use crate::runtime::Io;

type Lucide = gpui_kit::assets::IconName;

const LIST_OVERDRAW: Pixels = px(400.);

const SUGGESTIONS: &[&str] = &[
    "What decisions did we make recently?",
    "What open questions keep coming up?",
    "Which action items are still unresolved?",
];

#[derive(Debug, Clone, PartialEq)]
struct Option_ {
    key: SharedString,
    label: SharedString,
}

impl SelectItem for Option_ {
    type Value = SharedString;
    fn title(&self) -> SharedString {
        self.label.clone()
    }
    fn value(&self) -> &Self::Value {
        &self.key
    }
}

fn opt(key: impl Into<SharedString>, label: impl Into<SharedString>) -> Option_ {
    Option_ {
        key: key.into(),
        label: label.into(),
    }
}

#[derive(Clone)]
enum Turn {
    User(String),
    Assistant {
        id: u64,
        answer: String,
        sources: Vec<SourceRef>,
        note: Option<String>,
    },
}

#[derive(Clone)]
enum Row {
    Turn(Turn),
    PendingQuestion(String),
    PendingAnswer(String),
}

pub struct AskView {
    turns: Vec<Turn>,
    list_state: ListState,
    input: Entity<InputState>,
    /// Request id of the ask in flight.
    in_flight: Option<u64>,
    pending_question: Option<String>,
    streaming: String,
    error: Option<String>,
    status: Option<String>,

    people: Vec<parley_core::chat::across::PersonOption>,
    series: Vec<parley_core::chat::across::SeriesOption>,
    person_select: Entity<SelectState<Vec<Option_>>>,
    date_select: Entity<SelectState<Vec<Option_>>>,
    series_select: Entity<SelectState<Vec<Option_>>>,
    date_from: Entity<InputState>,
    date_to: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl AskView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Ask across all your meetings…"));
        let person_select = cx.new(|cx| {
            SelectState::new(vec![opt("", "Anyone")], Some(IndexPath::default()), window, cx)
        });
        let date_items: Vec<Option_> = logic::DATE_PRESETS.iter().map(|(k, l)| opt(*k, *l)).collect();
        let date_select = cx.new(|cx| SelectState::new(date_items, Some(IndexPath::default()), window, cx));
        let series_select = cx.new(|cx| {
            SelectState::new(vec![opt("", "All meetings")], Some(IndexPath::default()), window, cx)
        });
        let date_from = cx.new(|cx| InputState::new(window, cx).placeholder("From YYYY-MM-DD"));
        let date_to = cx.new(|cx| InputState::new(window, cx).placeholder("To YYYY-MM-DD"));

        let core_events = AppServices::global(cx).core_events.clone();
        let subscriptions = vec![
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.send(window, cx);
                }
            }),
            cx.subscribe(&core_events, |this, _, event, cx| {
                if event.name == ASK_ACROSS_STREAM_EVENT {
                    this.on_stream(event, cx);
                }
            }),
            // The custom-range inputs appear/disappear with the preset.
            cx.subscribe(&date_select, |_, _, _: &SelectEvent<Vec<Option_>>, cx| cx.notify()),
        ];

        Self {
            turns: Vec::new(),
            list_state: ListState::new(0, ListAlignment::Bottom, LIST_OVERDRAW),
            input,
            in_flight: None,
            pending_question: None,
            streaming: String::new(),
            error: None,
            status: None,
            people: Vec::new(),
            series: Vec::new(),
            person_select,
            date_select,
            series_select,
            date_from,
            date_to,
            _subscriptions: subscriptions,
        }
    }

    /// Called by the shell whenever the page is shown: refresh the filter
    /// options and the semantic-index status, and let the background
    /// indexer pick up new meetings. While it indexes (or the built-in
    /// model downloads) the status is refreshed every few seconds.
    pub fn on_show(&mut self, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move {
                    across::refresh_semantic_index(&pool).await;
                    let people = across::list_people(&pool).await.unwrap_or_default();
                    let series = across::list_series(&pool).await.unwrap_or_default();
                    let (availability, progress) = across::semantic_status(&pool).await;
                    (people, series, logic::semantic_status_text(&availability, progress))
                })
                .await;
            let Ok((people, series, status)) = result else { return };
            let shown = this.update_in(cx, |this, window, cx| {
                this.status = Some(status);
                this.set_people(people, window, cx);
                this.set_series(series, window, cx);
                cx.notify();
            });
            if shown.is_err() {
                return;
            }
            // Bounded: an hour of polling at most, even if something sticks.
            // Each tick re-triggers indexing before checking whether anything
            // is still busy, so a model download finishing mid-wait goes
            // straight on to indexing instead of ending the loop.
            let mut busy = across::semantic_index_busy();
            for _ in 0..1200 {
                if !busy {
                    break;
                }
                cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
                let Some(pool) = cx.update(|cx| AppServices::global(cx).pool()) else {
                    return;
                };
                let status = io
                    .spawn(async move {
                        across::refresh_semantic_index(&pool).await;
                        let (availability, progress) = across::semantic_status(&pool).await;
                        (logic::semantic_status_text(&availability, progress), across::semantic_index_busy())
                    })
                    .await;
                let Ok((status, still_busy)) = status else { return };
                busy = still_busy;
                let updated = this.update(cx, |this, cx| {
                    this.status = Some(status);
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    fn set_people(
        &mut self,
        people: Vec<parley_core::chat::across::PersonOption>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.person_select.read(cx).selected_value().cloned();
        let items: Vec<Option_> = std::iter::once(opt("", "Anyone"))
            .chain(people.iter().map(|p| opt(p.voice_profile_id.clone(), p.name.clone())))
            .collect();
        self.people = people;
        self.person_select.update(cx, |s, cx| {
            s.set_items(items, window, cx);
            match current {
                Some(v) => s.set_selected_value(&v, window, cx),
                None => s.set_selected_index(Some(IndexPath::default()), window, cx),
            }
        });
    }

    fn set_series(
        &mut self,
        series: Vec<parley_core::chat::across::SeriesOption>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.series_select.read(cx).selected_value().cloned();
        let items: Vec<Option_> = std::iter::once(opt("", "All meetings"))
            .chain(series.iter().map(|s| {
                opt(
                    series_key(&s.filter),
                    format!("{} ({})", s.filter.title, s.meeting_count),
                )
            }))
            .collect();
        self.series = series;
        self.series_select.update(cx, |s, cx| {
            s.set_items(items, window, cx);
            match current {
                Some(v) => s.set_selected_value(&v, window, cx),
                None => s.set_selected_index(Some(IndexPath::default()), window, cx),
            }
        });
    }

    fn filters(&self, cx: &App) -> AskFilters {
        let person_key = self.person_select.read(cx).selected_value().cloned().unwrap_or_default();
        let person = self
            .people
            .iter()
            .find(|p| !person_key.is_empty() && p.voice_profile_id == person_key.as_ref())
            .map(|p| PersonFilter {
                voice_profile_id: Some(p.voice_profile_id.clone()),
                name: p.name.clone(),
            });
        let series_key_sel = self.series_select.read(cx).selected_value().cloned().unwrap_or_default();
        let series: Option<SeriesFilter> = self
            .series
            .iter()
            .find(|s| !series_key_sel.is_empty() && series_key(&s.filter) == series_key_sel.as_ref())
            .map(|s| s.filter.clone());
        let date_key = self.date_key(cx);
        let today = chrono::Local::now().date_naive();
        let (from, to) = logic::date_range(
            &date_key,
            today,
            &self.date_from.read(cx).value(),
            &self.date_to.read(cx).value(),
        );
        AskFilters { person, from, to, series }
    }

    fn date_key(&self, cx: &App) -> String {
        self.date_select
            .read(cx)
            .selected_value()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "any".into())
    }

    fn is_busy(&self) -> bool {
        self.in_flight.is_some()
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self.turns.iter().cloned().map(Row::Turn).collect();
        if self.is_busy() {
            if let Some(q) = &self.pending_question {
                rows.push(Row::PendingQuestion(q.clone()));
            }
            rows.push(Row::PendingAnswer(self.streaming.clone()));
        }
        rows
    }

    fn sync_list(&mut self) {
        self.list_state.reset(self.rows().len());
    }

    /// Prior turns for a follow-up, oldest first.
    fn history(&self) -> Vec<ChatTurn> {
        self.turns
            .iter()
            .map(|t| match t {
                Turn::User(q) => ChatTurn { is_user: true, content: q.clone() },
                Turn::Assistant { answer, .. } => ChatTurn { is_user: false, content: answer.clone() },
            })
            .collect()
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let question = self.input.read(cx).value().trim().to_string();
        self.ask(question, window, cx);
    }

    fn ask(&mut self, question: String, window: &mut Window, cx: &mut Context<Self>) {
        if question.is_empty() || self.is_busy() {
            return;
        }
        let Some(pool) = AppServices::global(cx).pool() else {
            self.error = Some("No database — complete onboarding first.".into());
            cx.notify();
            return;
        };
        let sink = AppServices::global(cx).sink.clone();
        let io = Io::global(cx);
        let filters = self.filters(cx);
        let history = self.history();
        let request_id = across::next_request_id();

        self.in_flight = Some(request_id);
        self.pending_question = Some(question.clone());
        self.streaming.clear();
        self.error = None;
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        self.sync_list();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let q = question.clone();
            let result = io
                .spawn(async move { across::ask_across(sink, pool, request_id, q, history, filters).await })
                .await
                .unwrap_or_else(|e| Err(format!("Task failed: {e}")));
            let _ = this.update_in(cx, |this, window, cx| {
                if this.in_flight != Some(request_id) {
                    return;
                }
                this.in_flight = None;
                this.pending_question = None;
                this.streaming.clear();
                match result {
                    Ok(outcome) => {
                        let note = logic::answer_note(&outcome.semantic, outcome.stats.fallback, outcome.sources.len());
                        this.turns.push(Turn::User(outcome.question));
                        this.turns.push(Turn::Assistant {
                            id: request_id,
                            answer: outcome.answer,
                            sources: outcome.sources,
                            note,
                        });
                    }
                    Err(e) => {
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
        across::cancel_ask_across();
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        self.turns.clear();
        self.error = None;
        self.sync_list();
        cx.notify();
    }

    fn on_stream(&mut self, event: &crate::core_events::CoreEvent, cx: &mut Context<Self>) {
        #[derive(serde::Deserialize)]
        struct Delta {
            request_id: u64,
            delta: String,
        }
        let Some(payload) = event.decode::<Delta>() else {
            return;
        };
        if self.in_flight != Some(payload.request_id) {
            return;
        }
        self.streaming.push_str(&payload.delta);
        let count = self.rows().len();
        if count > 0 && self.list_state.item_count() == count {
            self.list_state.splice(count - 1..count, 1);
        } else {
            self.sync_list();
        }
        cx.notify();
    }
}

/// Stable select key for a series.
fn series_key(s: &SeriesFilter) -> String {
    format!("{}\u{1f}{}", s.source_id, s.ics_uid)
}

/// Open `source`'s meeting, at `seconds` if given, else at its first excerpt.
fn open_source(source: &SourceRef, seconds: Option<f64>, cx: &mut App) {
    crate::shell::open_meeting_at(source.meeting_id.clone(), seconds.or(source.first_seconds), cx);
}

impl Render for AskView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.is_busy();
        let rows = self.rows();
        let is_empty = rows.is_empty();
        let custom_dates = self.date_key(cx) == "custom";

        let header = v_flex()
            .w_full()
            .gap_3()
            .p_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_xl().font_semibold().child("Ask Parley"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(self.status.clone().unwrap_or_else(|| {
                                        "Answers use your summary model and cite meetings and timestamps.".into()
                                    })),
                            ),
                    )
                    .child(
                        Button::new("ask-all-clear")
                            .ghost()
                            .icon(Lucide::Eraser)
                            .label("New chat")
                            .disabled(self.turns.is_empty() || busy)
                            .on_click(cx.listener(|this, _, _, cx| this.clear(cx))),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(div().w_48().child(Select::new(&self.person_select).disabled(busy)))
                    .child(div().w_40().child(Select::new(&self.date_select).disabled(busy)))
                    .when(custom_dates, |this| {
                        this.child(div().w_40().child(Input::new(&self.date_from).disabled(busy)))
                            .child(div().w_40().child(Input::new(&self.date_to).disabled(busy)))
                    })
                    .when(!self.series.is_empty(), |this| {
                        this.child(div().w_56().child(Select::new(&self.series_select).disabled(busy)))
                    }),
            );

        let body: AnyElement = if is_empty {
            self.render_empty_state(cx)
        } else {
            let entity = cx.entity().downgrade();
            list(self.list_state.clone(), move |ix, _window, cx| render_row(ix, &rows[ix], entity.clone(), cx))
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
            .when_some(self.error.clone(), |this, err| {
                this.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_start()
                        .justify_between()
                        .child(div().flex_1().min_w_0().text_xs().text_color(cx.theme().danger).child(err))
                        .child(
                            Button::new("ask-all-dismiss-error")
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
                            Button::new("ask-all-send")
                                .primary()
                                .icon(Lucide::SendHorizontal)
                                .tooltip("Send (Enter)")
                                .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                        )
                    })
                    .when(busy, |this| {
                        this.child(
                            Button::new("ask-all-stop")
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

impl AskView {
    fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let suggestions = SUGGESTIONS.iter().enumerate().map(|(ix, suggestion)| {
            let text = suggestion.to_string();
            crate::ui::fade_in(
                h_flex()
                    .id(SharedString::from(format!("ask-all-suggestion-{ix}")))
                    .w_full()
                    .gap_3()
                    .items_center()
                    .px_4()
                    .py_3()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.group_box)
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.accent).border_color(theme.primary.opacity(0.5)))
                    .on_click(cx.listener(move |this, _, window, cx| this.ask(text.clone(), window, cx)))
                    .child(div().text_color(theme.primary).child(Lucide::MessageSquare.view(cx)))
                    .child(div().flex_1().text_sm().child(*suggestion))
                    .child(div().text_color(theme.muted_foreground).child(Lucide::ArrowRight.view(cx))),
                ("ask-all-suggestion-in", ix),
            )
        });
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_6()
            .p_8()
            .child(
                v_flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .size(px(56.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(theme.primary.opacity(0.12))
                            .text_color(theme.primary)
                            .child(Icon::from(Lucide::Sparkles).large()),
                    )
                    .child(div().text_xl().font_semibold().child("Ask anything across your meetings"))
                    .child(
                        div()
                            .max_w(px(520.))
                            .text_sm()
                            .text_center()
                            .text_color(theme.muted_foreground)
                            .child(
                                "Parley searches your transcripts, picks the most relevant passages and answers \
                                 with citations like [M1 12:34]. Click one to open that meeting at that moment.",
                            ),
                    ),
            )
            .child(v_flex().w_full().max_w(px(560.)).gap_2().children(suggestions))
            .into_any_element()
    }
}

fn render_row(ix: usize, row: &Row, _entity: WeakEntity<AskView>, cx: &mut App) -> AnyElement {
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
        Row::Turn(Turn::User(q)) | Row::PendingQuestion(q) => user_bubble(q.clone()).into_any_element(),
        Row::Turn(Turn::Assistant { id, answer, sources, note }) => {
            assistant_row(SharedString::from(format!("ask-all-msg-{id}")), answer, sources, note.clone(), cx)
                .into_any_element()
        }
        Row::PendingAnswer(text) if text.trim().is_empty() => div()
            .px_4()
            .py_2()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child("Searching your meetings and thinking…")
            .into_any_element(),
        // While streaming, sources aren't known yet: show the text without
        // links (still escaped).
        Row::PendingAnswer(text) => div()
            .w_full()
            .px_4()
            .py_2()
            .text_sm()
            .child(TextView::markdown(
                SharedString::from(format!("ask-all-streaming-{ix}")),
                parley_core::chat::context::escape_untrusted_markdown(text),
            ))
            .into_any_element(),
    }
}

/// An answer: markdown with citation links, then its sources as chips.
fn assistant_row(
    id: SharedString,
    answer: &str,
    sources: &[SourceRef],
    note: Option<String>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let markdown = across::link_citations(answer, sources.len());
    let link_sources = sources.to_vec();
    let chips = sources.iter().enumerate().map(|(ix, source)| {
        let title = if source.title.trim().is_empty() { "Untitled meeting".to_string() } else { source.title.clone() };
        let date = source.created_at.as_ref().map(across::format_meeting_date).unwrap_or_default();
        let label = if date.is_empty() {
            format!("{} · {title}", source.label)
        } else {
            format!("{} · {title} · {date}", source.label)
        };
        let source = source.clone();
        Button::new(SharedString::from(format!("{id}-src-{ix}")))
            .outline()
            .xsmall()
            .label(label)
            .tooltip("Open this meeting")
            .on_click(move |_, _, cx| open_source(&source, None, cx))
    });
    v_flex()
        .w_full()
        .px_4()
        .py_2()
        .gap_2()
        .text_sm()
        .child(
            TextView::markdown(id.clone(), markdown)
                .selectable(true)
                .on_link_click(move |url, _, _, cx| {
                    // Only citations act; answers are untrusted model output,
                    // so nothing else in them is ever opened.
                    if let Some((ix, seconds)) = across::citation_link_target(url) {
                        if let Some(source) = link_sources.get(ix) {
                            open_source(source, seconds, cx);
                        }
                    }
                }),
        )
        .when(!sources.is_empty(), |this| {
            this.child(h_flex().w_full().flex_wrap().gap_1().children(chips))
        })
        .when_some(note, |this, note| {
            this.child(div().text_xs().text_color(theme.muted_foreground).child(note))
        })
}
