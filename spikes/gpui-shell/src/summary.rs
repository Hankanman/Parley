//! Check 3: a `zorite-editor` view loading real-shaped summary markdown —
//! either the most recently completed meeting summary from the user's real
//! Parley SQLite DB (read-only), or the bundled fixture.

use gpui_kit::component::{ActiveTheme, StyledExt as _, h_flex, v_flex, button::Button, button::ButtonVariants};
use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;
use zorite_editor::{EditorState, SyntaxStyle};

use crate::db;

const FIXTURE: &str = include_str!("../fixtures/summary.md");

fn syntax_style(cx: &App) -> SyntaxStyle {
    let theme = ActiveTheme::theme(cx);
    SyntaxStyle {
        block_label: None,
        block_label_gen: 0,
        block_ref_count: None,
        marker: theme.muted_foreground.opacity(0.6),
        code: theme.foreground,
        code_bg: theme.muted.opacity(0.5),
        link: theme.primary,
        tag: theme.accent_foreground,
        quote: theme.muted_foreground,
        alert_note: theme.primary,
        alert_tip: theme.primary,
        alert_important: theme.primary,
        alert_warning: theme.primary,
        alert_caution: theme.danger,
        alert_icons: None,
        rule: theme.border,
        mark_bg: theme.primary.opacity(0.25),
        popover_bg: theme.popover,
        popover_border: theme.border,
        popover_fg: theme.popover_foreground,
        popover_hover: theme.accent,
        popover_divider: theme.border,
        popover_danger: theme.danger,
        mono: gpui_kit::gpui::font("monospace"),
        property_icon: None,
    }
}

/// Where the summary text came from — shown in the view's header.
pub enum SummarySource {
    Database(String),
    Fixture,
}

pub struct SummaryView {
    editor: Entity<EditorState>,
    source: SummarySource,
    save_status: Option<String>,
}

impl SummaryView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (source, text) = match db::read_latest_summary_markdown() {
            Ok((label, markdown)) => (SummarySource::Database(label), markdown),
            Err(err) => {
                log::info!("falling back to bundled summary fixture: {err:#}");
                (SummarySource::Fixture, FIXTURE.to_string())
            }
        };

        let editor = cx.new(|cx| {
            let mut editor = EditorState::new(window, cx)
                .with_placeholder("No summary yet…")
                .with_text(text);
            editor.set_markdown_style(syntax_style(cx), cx);
            editor
        });

        Self {
            editor,
            source,
            save_status: None,
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text().to_string();
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
        let path = std::path::Path::new(&dir).join("parley-gpui-spike-summary.md");
        match std::fs::write(&path, text) {
            Ok(()) => {
                self.save_status = Some(format!("Saved to {}", path.display()));
                log::info!("summary saved to {}", path.display());
            }
            Err(err) => {
                self.save_status = Some(format!("Save failed: {err}"));
                log::error!("summary save failed: {err:#}");
            }
        }
        cx.notify();
    }
}

impl Render for SummaryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let source_label = match &self.source {
            SummarySource::Database(label) => format!("Loaded from database: {label}"),
            SummarySource::Fixture => {
                "Loaded from bundled fixture (database unavailable)".to_string()
            }
        };

        v_flex()
            .size_full()
            .gap_3()
            .p_4()
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .child(
                        v_flex()
                            .child(div().text_sm().font_semibold().child("Summary"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(source_label),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .when_some(self.save_status.clone(), |this, status| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(status),
                                )
                            })
                            .child(
                                Button::new("save-summary")
                                    .primary()
                                    .label("Save")
                                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                            ),
            ),
            )
            .child(
                div()
                    .id("summary-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .p_4()
                    .child(self.editor.clone()),
            )
    }
}
