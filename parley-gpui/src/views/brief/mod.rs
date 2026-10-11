//! "Brief" page: the pre-meeting brief for one upcoming calendar event
//! (issue #84).
//!
//! Reached from a meeting reminder notification ("Open brief") or the
//! sidebar's "Upcoming" group. Opening it calls
//! `parley_core::followup::brief::get_or_generate_brief`, which returns the
//! cached brief when its context hasn't changed, generates one otherwise,
//! and says so plainly when there's no history to brief on.
//!
//! The brief is untrusted model output: it's rendered with
//! `brief::render_markdown` (everything escaped; `[M1]` citations become
//! links labelled with the meeting's title and date), and the only links
//! acted on are those citations, which open the cited meeting in Parley.

use gpui_kit::base::StyledExt as _;
use gpui_kit::component::IconNameExt as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::{
    ActiveTheme, Disableable as _,
    button::Button,
    h_flex,
    text::TextView,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use parley_core::followup::brief::{self, prompt::BriefSource, Brief, BriefBody};

use crate::app_state::AppServices;
use crate::runtime::Io;

type Lucide = gpui_kit::assets::IconName;

#[derive(Debug, Clone, PartialEq)]
enum State {
    Idle,
    Loading,
    Ready(Brief),
    Error(String),
}

pub struct BriefView {
    event_id: Option<String>,
    state: State,
    /// Bumped per load; only the latest load's result applies.
    load_seq: u64,
}

impl BriefView {
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            event_id: None,
            state: State::Idle,
            load_seq: 0,
        }
    }

    /// Show the brief for `event_id` (called by the shell on navigation).
    pub fn load(&mut self, event_id: String, cx: &mut Context<Self>) {
        let same_event = self.event_id.as_deref() == Some(event_id.as_str());
        if same_event && matches!(self.state, State::Loading) {
            return;
        }
        self.event_id = Some(event_id);
        self.fetch(false, cx);
    }

    fn fetch(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(event_id) = self.event_id.clone() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            self.state = State::Error("No database — complete onboarding first.".into());
            cx.notify();
            return;
        };
        self.load_seq += 1;
        let seq = self.load_seq;
        self.state = State::Loading;
        cx.notify();

        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(brief::get_or_generate_brief(pool, event_id, force))
                .await
                .unwrap_or_else(|e| Err(format!("Task failed: {e}")));
            let _ = this.update(cx, |this, cx| {
                if this.load_seq != seq {
                    return;
                }
                this.state = match result {
                    Ok(brief) => State::Ready(brief),
                    Err(e) => State::Error(e),
                };
                cx.notify();
            });
        })
        .detach();
    }
}

fn open_source(source: &BriefSource, cx: &mut App) {
    crate::shell::open_meeting_at(source.meeting_id.clone(), None, cx);
}

impl Render for BriefView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let icon = Lucide::CalendarClock.view(cx);
        let theme = cx.theme().clone();
        let loading = matches!(self.state, State::Loading);

        let header = match &self.state {
            State::Ready(b) => {
                let mut sub = brief::format_event_start(&b.event_start);
                if !b.people.is_empty() {
                    sub.push_str(" · with ");
                    sub.push_str(&b.people.iter().take(8).cloned().collect::<Vec<_>>().join(", "));
                    if b.people.len() > 8 {
                        sub.push_str(&format!(" and {} more", b.people.len() - 8));
                    }
                }
                Some((b.event_title.clone(), sub))
            }
            _ => None,
        };
        let can_regenerate = matches!(&self.state, State::Ready(b) if matches!(b.body, BriefBody::Generated { .. }));

        let top = h_flex()
            .w_full()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_2()
            .p_4()
            .border_b_1()
            .border_color(theme.border)
            .child(
                v_flex()
                    .gap_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(icon)
                            .child(div().text_lg().font_semibold().child(
                                header.as_ref().map(|(t, _)| t.clone()).unwrap_or_else(|| "Meeting brief".into()),
                            )),
                    )
                    .when_some(header.map(|(_, s)| s), |this, sub| {
                        this.child(div().text_sm().text_color(theme.muted_foreground).child(sub))
                    }),
            )
            .child(
                Button::new("brief-regenerate")
                    .outline()
                    .small()
                    .icon(Lucide::RefreshCw)
                    .label("Regenerate")
                    .loading(loading)
                    .disabled(loading || !can_regenerate)
                    .on_click(cx.listener(|this, _, _, cx| this.fetch(true, cx))),
            );

        let body: AnyElement = match &self.state {
            State::Idle => div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Open a brief from an upcoming meeting in the sidebar, or from a meeting reminder.")
                .into_any_element(),
            State::Loading => div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Looking through past meetings and preparing your brief…")
                .into_any_element(),
            State::Error(e) => div().text_sm().text_color(theme.danger).child(e.clone()).into_any_element(),
            State::Ready(b) => render_brief(b, cx),
        };

        v_flex()
            .size_full()
            .child(top)
            .child(
                div()
                    .id("brief-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .child(v_flex().w_full().max_w(px(760.)).gap_4().child(body)),
            )
    }
}

fn render_brief(b: &Brief, cx: &App) -> AnyElement {
    let theme = cx.theme();
    match &b.body {
        BriefBody::NoHistory(message) => v_flex()
            .gap_2()
            .child(div().text_sm().child(message.clone()))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Briefs draw on past meetings in the same calendar series or with the same people."),
            )
            .into_any_element(),
        BriefBody::Generated {
            text,
            generated_at,
            from_cache,
        } => {
            let markdown = brief::render_markdown(text, &b.sources);
            let link_sources = b.sources.clone();
            let chips = b.sources.iter().enumerate().map(|(ix, source)| {
                let source = source.clone();
                Button::new(SharedString::from(format!("brief-src-{ix}")))
                    .outline()
                    .xsmall()
                    .label(format!("{} · {}", source.label, source.display()))
                    .tooltip("Open this meeting")
                    .on_click(move |_, _, cx| open_source(&source, cx))
            });
            let when = chrono::DateTime::parse_from_rfc3339(generated_at)
                .map(|d| d.with_timezone(&chrono::Local).format("%a %-d %b, %H:%M").to_string())
                .unwrap_or_default();
            let mut note = if *from_cache {
                format!("Generated {when}; nothing has changed since.")
            } else {
                format!("Generated {when}.")
            };
            if b.total_related > b.sources.len() {
                note.push_str(&format!(
                    " Based on the {} most relevant of {} related meetings.",
                    b.sources.len(),
                    b.total_related
                ));
            }
            note.push_str(" AI-generated: check it against the meetings.");
            v_flex()
                .gap_3()
                .child(
                    div().text_sm().child(
                        TextView::markdown("brief-text", markdown)
                            .selectable(true)
                            .on_link_click(move |url, _, _, cx| {
                                // Only citations act: the brief is model
                                // output, so nothing else in it is opened.
                                if let Some(source) = brief::link_target(url).and_then(|ix| link_sources.get(ix)) {
                                    open_source(source, cx);
                                }
                            }),
                    ),
                )
                .when(!b.sources.is_empty(), |this| {
                    this.child(div().text_xs().font_semibold().child("Past meetings"))
                        .child(h_flex().w_full().flex_wrap().gap_1().children(chips))
                })
                .child(div().text_xs().text_color(theme.muted_foreground).child(note))
                .into_any_element()
        }
    }
}
