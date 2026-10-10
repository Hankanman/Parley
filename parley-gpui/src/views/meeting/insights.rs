//! "Insights" panel: talk-time and speaking analytics for the open meeting
//! (issue #86).
//!
//! A self-contained entity the meeting page shows as the third tab of its
//! right-hand pane. The numbers come from `parley_core::analytics` (pure
//! statistics over the stored transcript segments, no LLM), loaded when the
//! tab opens and again whenever the meeting page reports that the
//! transcript or its speakers changed while the tab is showing. Clicking a
//! longest-monologue figure emits [`InsightsPanelEvent::JumpTo`], answered
//! by the meeting page exactly like an "Ask" citation.

use gpui_kit::component::{
    ActiveTheme, Disableable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use parley_core::analytics::{self, format_clock, format_span, MeetingStats, Monologue, SpeakerStats};

use crate::app_state::AppServices;
use crate::runtime::Io;

type Lucide = gpui_kit::assets::IconName;

pub enum InsightsPanelEvent {
    /// A monologue was clicked: show the transcript at this many seconds.
    JumpTo(f64),
}

impl EventEmitter<InsightsPanelEvent> for InsightsPanel {}

pub struct InsightsPanel {
    meeting_id: Option<String>,
    /// Bumped on every load; a completion applies only if it's the latest.
    load_seq: u64,
    stats: Option<MeetingStats>,
    loading: bool,
    error: Option<String>,
}

impl InsightsPanel {
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { meeting_id: None, load_seq: 0, stats: None, loading: false, error: None }
    }

    /// Show `id`'s insights (called by the meeting page on every load).
    /// Computes straight away only if the tab is showing.
    pub fn set_meeting(&mut self, id: String, visible: bool, cx: &mut Context<Self>) {
        self.meeting_id = Some(id);
        self.load_seq += 1;
        self.stats = None;
        self.loading = false;
        self.error = None;
        if visible {
            self.refresh(cx);
        }
        cx.notify();
    }

    /// The transcript or its speakers changed. Recompute now if the tab is
    /// showing; otherwise drop the cached numbers so the next open reloads.
    pub fn invalidate(&mut self, visible: bool, cx: &mut Context<Self>) {
        if visible {
            self.refresh(cx);
        } else {
            self.load_seq += 1;
            self.stats = None;
            self.loading = false;
        }
    }

    /// (Re)compute from the database. The previous numbers stay on screen
    /// until the new ones arrive, so reopening the tab doesn't flicker.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            self.error = Some("No database — complete onboarding first.".into());
            cx.notify();
            return;
        };
        self.load_seq += 1;
        let seq = self.load_seq;
        self.loading = true;
        cx.notify();
        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let result = io.spawn(async move { analytics::meeting_stats(&pool, &id).await }).await;
            let _ = this.update(cx, |this, cx| {
                if this.load_seq != seq {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(Ok(stats)) => {
                        this.stats = Some(stats);
                        this.error = None;
                    }
                    Ok(Err(e)) => this.error = Some(format!("Failed to compute insights: {e}")),
                    Err(e) => this.error = Some(format!("Failed to compute insights: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for InsightsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .child(div().text_sm().font_semibold().child("Insights"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Talk time and speaking patterns, computed from the transcript."),
                    ),
            )
            .child(
                Button::new("insights-refresh")
                    .ghost()
                    .small()
                    .icon(Lucide::RefreshCw)
                    .tooltip("Recompute")
                    .loading(self.loading)
                    .disabled(self.loading || self.meeting_id.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            );

        let muted = |text: String, cx: &App| {
            div().p_4().text_sm().text_color(cx.theme().muted_foreground).child(text).into_any_element()
        };
        let body: AnyElement = match (&self.stats, &self.error) {
            (_, Some(err)) => div().p_4().text_sm().text_color(cx.theme().danger).child(err.clone()).into_any_element(),
            (None, None) => muted("Computing insights…".into(), cx),
            (Some(stats), None) if stats.is_empty() => {
                muted("No transcript yet — insights appear once the meeting has been transcribed.".into(), cx)
            }
            (Some(stats), None) => self.render_stats(stats, cx),
        };

        v_flex()
            .size_full()
            .child(header)
            .child(div().id("insights-scroll").flex_1().min_h_0().overflow_y_scroll().child(body))
    }
}

impl InsightsPanel {
    fn render_stats(&self, stats: &MeetingStats, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let palette = [theme.chart_1, theme.chart_2, theme.chart_3, theme.chart_4, theme.chart_5];

        // ---- Key numbers -----------------------------------------------
        let mut tiles: Vec<AnyElement> = Vec::new();
        if let Some(d) = stats.duration_secs {
            let sub = stats.silence_ratio.map(|r| format!("{} silent", percent(r)));
            tiles.push(tile("Duration", format_span(d), sub, cx));
        }
        if let Some(me) = stats.me_share {
            tiles.push(tile("You vs others", format!("{} / {}", percent(me), percent(1.0 - me)), Some("of talk time".into()), cx));
        }
        tiles.push(tile("Questions", stats.total_questions.to_string(), None, cx));
        tiles.push(tile(
            "Interruptions",
            stats.total_interruptions.to_string(),
            Some("talking over someone".into()),
            cx,
        ));
        let monologue_tile = stats.longest_monologue.as_ref().map(|(name, m)| {
            monologue_tile(name, *m, cx.entity().downgrade(), cx)
        });

        let key_numbers = h_flex()
            .w_full()
            .flex_wrap()
            .gap_2()
            .children(tiles)
            .children(monologue_tile);

        // ---- Per-speaker talk share ------------------------------------
        let speakers = v_flex().w_full().gap_3().children(stats.speakers.iter().enumerate().map(|(ix, s)| {
            let color = palette[ix % palette.len()];
            self.render_speaker(ix, s, color, cx)
        }));

        v_flex()
            .w_full()
            .gap_4()
            .p_4()
            .child(key_numbers)
            .child(
                v_flex()
                    .w_full()
                    .gap_2()
                    .child(section_title("Talk time", cx))
                    .child(speakers),
            )
            .when(stats.untimed_segments > 0, |this| {
                this.child(
                    div().text_xs().text_color(cx.theme().muted_foreground).child(format!(
                        "{} of {} segments have no timestamps, so they add no talk time.",
                        stats.untimed_segments, stats.segment_count
                    )),
                )
            })
            .into_any_element()
    }

    fn render_speaker(&self, ix: usize, s: &SpeakerStats, color: Hsla, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let mut details: Vec<String> = vec![plural(s.turns, "turn")];
        if let Some(wpm) = s.words_per_minute {
            details.push(format!("{} wpm", wpm.round() as i64));
        }
        if s.questions > 0 {
            details.push(plural(s.questions, "question"));
        }
        if s.interruptions > 0 {
            details.push(plural(s.interruptions, "interruption"));
        }

        let bar = div()
            .w_full()
            .h(px(8.))
            .rounded_full()
            .bg(theme.muted)
            .child(
                div()
                    .h_full()
                    .rounded_full()
                    .bg(color)
                    // Keep a sliver visible for a speaker who barely spoke.
                    .w(relative(if s.share > 0.0 { s.share.max(0.01) as f32 } else { 0.0 })),
            );

        let entity = cx.entity().downgrade();
        let monologue = s.longest_monologue.filter(|m| m.duration_secs >= 1.0).map(|m| {
            Button::new(SharedString::from(format!("insights-monologue-{ix}")))
                .ghost()
                .xsmall()
                .label(format!("longest {} at {}", format_span(m.duration_secs), format_clock(m.start_secs)))
                .tooltip("Jump to this moment in the transcript")
                .on_click(move |_, _, cx| {
                    let _ = entity.update(cx, |_, cx| cx.emit(InsightsPanelEvent::JumpTo(m.start_secs)));
                })
        });

        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_2()
                            .items_center()
                            .child(div().size(px(8.)).rounded_full().bg(color))
                            .child(div().text_sm().font_medium().truncate().child(s.name.clone()))
                            .when(s.is_me && s.name != "Me", |this| {
                                this.child(div().text_xs().text_color(theme.muted_foreground).child("(you)"))
                            }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_sm()
                            .child(format!("{} · {}", percent(s.share), format_span(s.talk_secs))),
                    ),
            )
            .child(bar)
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_1()
                    .items_center()
                    .child(div().text_xs().text_color(theme.muted_foreground).child(details.join(" · ")))
                    .children(monologue),
            )
            .into_any_element()
    }
}

fn section_title(text: &'static str, cx: &App) -> impl IntoElement {
    div().text_xs().font_semibold().text_color(cx.theme().muted_foreground).child(text.to_uppercase())
}

fn tile_frame(cx: &App) -> Div {
    v_flex()
        .min_w(px(120.))
        .flex_1()
        .gap_0p5()
        .px_3()
        .py_2()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(cx.theme().border)
}

fn tile(label: &'static str, value: String, sub: Option<String>, cx: &App) -> AnyElement {
    tile_frame(cx)
        .child(div().text_xs().text_color(cx.theme().muted_foreground).child(label))
        .child(div().text_lg().font_semibold().child(value))
        .when_some(sub, |this, sub| this.child(div().text_xs().text_color(cx.theme().muted_foreground).child(sub)))
        .into_any_element()
}

fn monologue_tile(name: &str, m: Monologue, entity: WeakEntity<InsightsPanel>, cx: &App) -> AnyElement {
    tile_frame(cx)
        .id("insights-longest-monologue")
        .cursor_pointer()
        .hover(|this| this.bg(cx.theme().muted))
        .child(div().text_xs().text_color(cx.theme().muted_foreground).child("Longest monologue"))
        .child(div().text_lg().font_semibold().child(format_span(m.duration_secs)))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().primary)
                .truncate()
                .child(format!("{name} at {}", format_clock(m.start_secs))),
        )
        .on_click(move |_, _, cx| {
            let _ = entity.update(cx, |_, cx| cx.emit(InsightsPanelEvent::JumpTo(m.start_secs)));
        })
        .into_any_element()
}

fn percent(fraction: f64) -> String {
    format!("{}%", (fraction * 100.0).round() as i64)
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}
