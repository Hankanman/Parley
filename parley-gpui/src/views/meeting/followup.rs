//! "Draft follow-up email" dialog (issue #84).
//!
//! Opened from the Summary tab's toolbar. Shows the meeting's stored draft
//! (`parley_core::followup::email`), or drafts one straight away when there
//! is none. The user picks a tone (concise / formal / friendly), can
//! regenerate, and edits the subject and body in plain-text fields; edits
//! are saved back as the meeting's latest draft after a short pause.
//!
//! The draft is model output, so it's only ever plain text in an editable
//! field. Copy subject / Copy body / Copy all are the only exports: Parley
//! never sends email and never opens a `mailto:` link built from the draft.

use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme, Disableable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::{Dialog, DialogContent},
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    label::Label,
    notification::Notification,
    v_flex,
};
use gpui_kit::component::Sizable as _;
use gpui_kit::base::StyledExt as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use parley_core::followup::email::{self, DraftSource, EmailDraft, FollowupDraftRepository, Tone};

use crate::app_state::AppServices;
use crate::runtime::Io;

type Lucide = gpui_kit::assets::IconName;

/// Pause after the last keystroke before an edit is saved.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(700);

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Loading,
    /// Nothing drafted yet and no generation running.
    Empty,
    Generating,
    Ready,
    Error(String),
}

pub struct FollowupState {
    meeting_id: String,
    tone: Tone,
    status: Status,
    /// Set once a draft exists (stored or generated); edits are only saved
    /// after that.
    source: Option<DraftSource>,
    edited: bool,
    subject: Entity<InputState>,
    body: Entity<TextareaState>,
    /// Bumped per generation, so a superseded result is dropped.
    generation: u64,
    /// Edited (subject, body) not saved yet.
    pending_edit: Option<(String, String)>,
    io: Io,
    pool: Option<sqlx::SqlitePool>,
    _save_debounce: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Drop for FollowupState {
    /// Closing the dialog mid-debounce still saves the last edit.
    fn drop(&mut self) {
        self.flush_edit();
    }
}

impl FollowupState {
    fn new(meeting_id: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subject = cx.new(|cx| InputState::new(window, cx).placeholder("Subject"));
        let body = cx.new(|cx| TextareaState::new(window, cx).rows(16).placeholder("Email body"));
        let subscriptions = vec![
            cx.subscribe(&subject, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.schedule_save(cx);
                }
            }),
            cx.subscribe(&body, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.schedule_save(cx);
                }
            }),
        ];
        Self {
            meeting_id,
            tone: Tone::default(),
            status: Status::Loading,
            source: None,
            edited: false,
            subject,
            body,
            generation: 0,
            pending_edit: None,
            io: Io::global(cx),
            pool: AppServices::global(cx).pool(),
            _save_debounce: None,
            _subscriptions: subscriptions,
        }
    }

    /// Load the stored draft; draft one if there is none.
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            self.status = Status::Error("No database — complete onboarding first.".into());
            cx.notify();
            return;
        };
        let io = Io::global(cx);
        let id = self.meeting_id.clone();
        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move { FollowupDraftRepository::get(&pool, &id).await })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(Ok(Some(draft))) => this.apply_draft(draft, window, cx),
                Ok(Ok(None)) => {
                    this.status = Status::Empty;
                    this.generate(window, cx);
                }
                Ok(Err(e)) => {
                    this.status = Status::Error(format!("Failed to load the draft: {e}"));
                    cx.notify();
                }
                Err(e) => {
                    this.status = Status::Error(format!("Failed to load the draft: {e}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn set_tone(&mut self, tone: Tone, cx: &mut Context<Self>) {
        self.tone = tone;
        cx.notify();
    }

    fn generate(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.status == Status::Generating {
            return;
        }
        let Some(pool) = AppServices::global(cx).pool() else {
            self.status = Status::Error("No database — complete onboarding first.".into());
            cx.notify();
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        self.status = Status::Generating;
        // The new draft replaces any unsaved edit.
        self._save_debounce = None;
        self.pending_edit = None;
        cx.notify();

        let io = Io::global(cx);
        let id = self.meeting_id.clone();
        let tone = self.tone;
        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(email::draft_followup_email(pool, id, tone))
                .await
                .unwrap_or_else(|e| Err(format!("Task failed: {e}")));
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(draft) => this.apply_draft(draft, window, cx),
                    Err(e) => {
                        this.status = Status::Error(e);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn apply_draft(&mut self, draft: EmailDraft, window: &mut Window, cx: &mut Context<Self>) {
        self.tone = draft.tone;
        self.source = Some(draft.source);
        self.edited = false;
        self.status = Status::Ready;
        // `set_value` doesn't emit `Change`, so this isn't saved back.
        self.subject.update(cx, |s, cx| s.set_value(draft.subject, window, cx));
        self.body.update(cx, |s, cx| s.set_value(draft.body, window, cx));
        cx.notify();
    }

    /// Remember the edited text and save it after [`SAVE_DEBOUNCE`] (or when
    /// the dialog goes away first — see `Drop`).
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        if self.source.is_none() || self.status == Status::Generating {
            return;
        }
        self.edited = true;
        self.pending_edit = Some(self.texts(cx));
        cx.notify();
        self._save_debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            let _ = this.update(cx, |this, _| this.flush_edit());
        }));
    }

    /// Save the pending edit, if any, on the io runtime.
    fn flush_edit(&mut self) {
        let (Some((subject, body)), Some(pool)) = (self.pending_edit.take(), self.pool.clone()) else {
            return;
        };
        let id = self.meeting_id.clone();
        self.io.spawn(async move {
            if let Err(e) = FollowupDraftRepository::update_text(&pool, &id, &subject, &body).await {
                log::warn!("failed to save follow-up draft edits: {e}");
            }
        });
    }

    fn texts(&self, cx: &App) -> (String, String) {
        (
            self.subject.read(cx).value().to_string(),
            self.body.read(cx).value().to_string(),
        )
    }

    fn copy(&self, what: CopyWhat, window: &mut Window, cx: &mut Context<Self>) {
        let (subject, body) = self.texts(cx);
        let (text, label) = match what {
            CopyWhat::Subject => (subject.trim().to_string(), "Subject copied"),
            CopyWhat::Body => (body.trim_end().to_string(), "Email body copied"),
            CopyWhat::All => (email::full_text(&subject, &body), "Email copied"),
        };
        if text.trim().is_empty() {
            window.push_notification(Notification::error("Nothing to copy yet"), cx);
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        window.push_notification(Notification::success(label), cx);
    }
}

#[derive(Clone, Copy)]
enum CopyWhat {
    Subject,
    Body,
    All,
}

/// Open the dialog for `meeting_id`.
pub fn open(meeting_id: String, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    let state = cx.new(|cx| FollowupState::new(meeting_id, window, cx));
    state.update(cx, |state, cx| state.load(cx));
    window.open_dialog(cx, move |dialog, window, cx| render_dialog(dialog, state.clone(), window, cx));
}

fn render_dialog(dialog: Dialog, state: Entity<FollowupState>, _window: &mut Window, cx: &mut App) -> Dialog {
    let busy = matches!(state.read(cx).status, Status::Generating | Status::Loading);
    let content_state = state.clone();
    dialog
        .title(Label::new("Follow-up email").text_lg().font_semibold())
        .width(px(680.))
        .overlay_closable(!busy)
        .content(move |content, window, cx| render_body(content, &content_state, window, cx))
        .footer(render_footer(state, busy))
}

fn render_body(content: DialogContent, state: &Entity<FollowupState>, _window: &mut Window, cx: &mut App) -> DialogContent {
    let s = state.read(cx);
    let theme = cx.theme();
    let generating = s.status == Status::Generating;
    let has_draft = s.source.is_some();

    let tone_buttons = Tone::ALL.into_iter().map(|tone| {
        let state = state.clone();
        let button = Button::new(SharedString::from(format!("followup-tone-{}", tone.as_str())))
            .small()
            .label(tone.label())
            .disabled(generating)
            .on_click(move |_, _, cx| state.update(cx, |s, cx| s.set_tone(tone, cx)));
        if tone == s.tone { button.primary() } else { button.outline() }
    });

    let status_line: Option<(String, bool)> = match &s.status {
        Status::Loading => Some(("Loading…".into(), false)),
        Status::Empty => None,
        Status::Generating => Some(("Drafting the email…".into(), false)),
        Status::Ready => s.source.map(|src| {
            let edited = if s.edited { " · edited" } else { "" };
            (format!("Drafted from {}{edited}. Review before sending.", src.describe()), false)
        }),
        Status::Error(e) => Some((e.clone(), true)),
    };

    let generate_state = state.clone();
    let body = v_flex()
        .w_full()
        .gap_3()
        .child(
            h_flex()
                .w_full()
                .flex_wrap()
                .gap_2()
                .items_center()
                .justify_between()
                .child(h_flex().gap_1().items_center().child(Label::new("Tone").text_sm()).children(tone_buttons))
                .child(
                    Button::new("followup-generate")
                        .small()
                        .primary()
                        .icon(if has_draft { Lucide::RefreshCw } else { Lucide::Sparkles })
                        .label(if has_draft { "Regenerate" } else { "Draft email" })
                        .loading(generating)
                        .disabled(generating || s.status == Status::Loading)
                        .on_click(move |_, window, cx| generate_state.update(cx, |s, cx| s.generate(window, cx))),
                ),
        )
        .when_some(status_line, |this, (text, is_error)| {
            this.child(
                div()
                    .text_xs()
                    .text_color(if is_error { theme.danger } else { theme.muted_foreground })
                    .child(text),
            )
        })
        .child(v_flex().gap_1().child(Label::new("Subject").text_xs()).child(Input::new(&s.subject).disabled(generating)))
        .child(
            v_flex()
                .gap_1()
                .child(Label::new("Body").text_xs())
                .child(Textarea::new(&s.body).h(px(340.)).disabled(generating)),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Parley never sends email. Copy the draft into your mail app."),
        );
    content.child(body)
}

fn render_footer(state: Entity<FollowupState>, busy: bool) -> AnyElement {
    let copy_button = |id: &'static str, label: &'static str, what: CopyWhat| {
        let state = state.clone();
        Button::new(id)
            .outline()
            .icon(gpui_kit::component::IconName::Copy)
            .label(label)
            .disabled(busy)
            .on_click(move |_, window, cx| state.update(cx, |s, cx| s.copy(what, window, cx)))
    };
    h_flex()
        .w_full()
        .flex_wrap()
        .justify_end()
        .gap_2()
        .child(copy_button("followup-copy-subject", "Copy subject", CopyWhat::Subject))
        .child(copy_button("followup-copy-body", "Copy body", CopyWhat::Body))
        .child(copy_button("followup-copy-all", "Copy all", CopyWhat::All))
        .child(
            Button::new("followup-close")
                .ghost()
                .label("Close")
                .on_click(|_, window, cx| window.close_dialog(cx)),
        )
        .into_any_element()
}
