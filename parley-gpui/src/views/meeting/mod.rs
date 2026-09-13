//! Meeting page: editable header (title + delete), a virtualized transcript
//! panel, and a summary panel with generate/regenerate, WYSIWYG editing,
//! copy/export and retranscription.
//!
//! Loading, generation and polling are all guarded by a `generation`
//! counter bumped on every [`MeetingView::load`] — async work started for an
//! older meeting id checks it before touching `self` so a fast
//! double-navigation can't clobber the currently-shown meeting with a
//! stale response (see `cx.spawn` closures below).

mod calendar_format;
mod format;
mod speaker_chip;

use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme, Disableable as _, IconName, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState},
    notification::Notification,
    text::{TextView, TextViewState},
    v_flex,
};
use gpui_kit::base::StyledExt as _;
use gpui_kit::component::IconNameExt as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use parley_core::calendar::models::CalendarEvent;
use parley_core::calendar::repository::CalendarRepository;
use parley_core::calendar::service::link_meeting_with_snapshot;
use parley_core::database::models::{ActionItem, MeetingDetails, MeetingNote, MeetingTranscript, VoiceProfile};
use parley_core::database::repositories::action_item::{
    ActionItemsRepository, NewActionItem, SOURCE_MANUAL as ACTION_ITEM_SOURCE_MANUAL, STATUS_DONE, STATUS_OPEN,
};
use parley_core::database::repositories::meeting::MeetingsRepository;
use parley_core::database::repositories::meeting_note::{MeetingNotesRepository, SOURCE_MANUAL as NOTE_SOURCE_MANUAL};
use parley_core::database::repositories::setting::{SettingsRepository, KEY_UI_CONFIG};
use parley_core::database::repositories::summary::SummaryProcessesRepository;
use parley_core::database::repositories::voice_profile::VoiceProfilesRepository;
use parley_core::speaker_diarization::service::{merge_cluster_into_profile_core, promote_speaker_to_profile_core};
use parley_core::summary::markdown_export;
use parley_core::summary::service::SummaryService;
use zorite_editor::{EditorState, SyntaxStyle};

use crate::app_state::AppServices;
use crate::runtime::Io;
use crate::shell::{self, Route};

/// ±N days around the meeting's `created_at` when fetching nearby calendar
/// events for the link picker. Mirrors `CalendarEventPicker.tsx`'s
/// `WINDOW_DAYS`.
const CALENDAR_WINDOW_DAYS: i64 = 7;

/// One open speaker-edit panel: rename a named voice profile, or promote /
/// merge an unnamed "Speaker N" cluster. Mirrors
/// `EditableSpeakerChip.tsx`'s per-chip popover state, scoped to a single
/// segment at a time (see [`MeetingView::speaker_edit`]).
#[derive(Clone)]
struct SpeakerEditState {
    /// Uniquely identifies which chip this panel belongs to (segment id +
    /// label), so a stray async response for a since-closed/reopened panel
    /// is dropped instead of applied.
    key: String,
    speaker: String,
    voice_profile_id: Option<String>,
    name_input: Entity<InputState>,
    email_input: Entity<InputState>,
    profiles: Vec<VoiceProfile>,
    profiles_loading: bool,
    /// `Some(profile_id)` = fold this cluster into that existing profile;
    /// `None` = create/rename via `name_input`/`email_input`. Only
    /// meaningful for an unnamed cluster (named-profile edits always rename).
    merge_target: Option<String>,
    saving: bool,
    error: Option<String>,
    /// Where the chip was clicked, in window coordinates — the popover's
    /// anchor position (see [`render_floating_speaker_edit`]).
    click_position: Point<Pixels>,
    /// This meeting's linked-calendar-event attendees, offered as one-click
    /// name/email fills — mirrors `EditableSpeakerChip.tsx`'s `attendees`
    /// state. Computed once when the panel opens from
    /// [`MeetingView::calendar_event`], which is already loaded for the
    /// calendar row.
    attendees: Vec<speaker_chip::AttendeeSuggestion>,
}

/// The full Lucide catalog (as opposed to `gpui_kit::component::IconName`,
/// which only carries a curated default subset — most of the icons this
/// page needs aren't in it).
type Lucide = gpui_kit::assets::IconName;

/// How far along the current meeting's summary is. Distinct from "there is
/// no summary yet", which is `Idle` with empty `summary_state` text.
#[derive(Clone, PartialEq)]
enum SummaryPhase {
    Idle,
    /// Initial `summary_processes` fetch for a newly-loaded meeting.
    Loading,
    /// A generation is in flight (started here, or resumed because the row
    /// was already "processing" when the page loaded).
    Generating,
    Error(String),
}

pub struct MeetingView {
    meeting_id: Option<String>,
    /// Bumped on every `load()`; async completions compare it to the
    /// current value before applying, so a stale in-flight fetch for a
    /// meeting the user has since navigated away from is dropped.
    generation: u64,
    title: String,
    title_input: Entity<InputState>,
    editing_title: bool,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Recording folder on disk, if any (needed for retranscription; absent
    /// when auto_save was off, or the meeting predates folder tracking).
    meeting_folder_path: Option<String>,
    transcripts: Vec<MeetingTranscript>,
    transcripts_loading: bool,
    load_error: Option<String>,
    summary_state: Entity<TextViewState>,
    /// Source-of-truth markdown for the summary: what `summary_state`
    /// renders, what "Edit" seeds the editor with, and what "Copy summary"
    /// and export read. `TextViewState` doesn't expose a text getter.
    summary_markdown: String,
    summary_has_content: bool,
    summary_phase: SummaryPhase,
    editing_summary: bool,
    summary_editor: Entity<EditorState>,
    saving_summary: bool,
    _poll_task: Option<Task<()>>,

    /// Available summary templates (id, name, description), loaded once at
    /// startup — same list `list_templates` returns for the Tauri UI.
    templates: Vec<(String, String, String)>,
    selected_template: String,
    /// Set once the user manually cycles the template picker, so the async
    /// default-setting fetch (`load_default_template`) doesn't clobber their
    /// choice if it resolves afterwards.
    template_user_selected: bool,
    custom_prompt: Entity<InputState>,
    show_custom_prompt: bool,

    retranscribe_open: bool,
    retranscribe_language: Entity<InputState>,
    /// Downloaded (`Available`) whisper models, fetched lazily the first
    /// time the retranscribe panel opens.
    retranscribe_models: Vec<String>,
    retranscribe_model_index: Option<usize>,
    retranscribe_in_progress: bool,
    retranscribe_progress_pct: u32,
    retranscribe_progress_message: String,
    retranscribe_error: Option<String>,

    // -- Calendar event link --------------------------------------------
    calendar_event: Option<CalendarEvent>,
    calendar_loading: bool,
    calendar_mutating: bool,
    calendar_picker_open: bool,
    /// `None` while the picker's event fetch is in flight.
    calendar_picker_events: Option<Vec<CalendarEvent>>,
    calendar_picker_query: Entity<InputState>,
    calendar_picker_error: Option<String>,

    // -- Per-segment speaker chip edit -----------------------------------
    speaker_edit: Option<SpeakerEditState>,

    // -- Per-segment audio playback ---------------------------------------
    /// Segment id currently playing, if any.
    playing_segment: Option<String>,
    /// Segment id whose clip is being extracted (between click and playback
    /// actually starting), if any.
    loading_segment: Option<String>,

    // -- Confidence indicator ---------------------------------------------
    /// `ui_config.showConfidenceIndicator`, read once per `load()` — mirrors
    /// `ConfidenceIndicator.tsx`'s `showIndicator` gate.
    show_confidence: bool,

    // -- Meeting notes ------------------------------------------------------
    // Mirrors `MeetingNotesPanel.tsx`: append-only (no edit; delete+re-add).
    notes: Vec<MeetingNote>,
    notes_composing: bool,
    notes_draft: Entity<InputState>,
    notes_saving: bool,

    // -- Per-meeting action items --------------------------------------------
    // Mirrors `ActionItemsPanel.tsx`.
    action_items: Vec<ActionItem>,
    action_items_loading: bool,
    /// Item currently shown with an editable text field, if any.
    ai_editing_id: Option<String>,
    ai_edit_input: Entity<InputState>,
    ai_adding: bool,
    ai_add_input: Entity<InputState>,
    ai_extracting: bool,

    // -- Summary typewriter reveal --------------------------------------------
    // `on_summary_stream` buffers incoming deltas here instead of pushing
    // them straight to `summary_state`; `_summary_reveal_task` drains the
    // buffer at `format::SUMMARY_REVEAL_INTERVAL_MS`, mirroring
    // `useTranscriptStreaming.ts`'s pacing (see `format.rs`).
    summary_reveal_pending: String,
    _summary_reveal_task: Option<Task<()>>,

    _subscriptions: Vec<Subscription>,
}

impl MeetingView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let title_input = cx.new(|cx| InputState::new(window, cx).placeholder("Meeting title"));
        let retranscribe_language =
            cx.new(|cx| InputState::new(window, cx).placeholder("auto"));
        let summary_state = cx.new(|cx| TextViewState::markdown("", cx));
        let summary_editor = cx.new(|cx| {
            let mut editor = EditorState::new(window, cx).with_placeholder("No summary yet…");
            editor.set_markdown_style(syntax_style(cx), cx);
            editor
        });

        let core_events = AppServices::global(cx).core_events.clone();
        let subscriptions = vec![cx.subscribe(&core_events, |this, _, event, cx| {
            match event.name.as_str() {
                "summary-stream" => this.on_summary_stream(event, cx),
                "retranscription-progress" => this.on_retranscription_progress(event, cx),
                "retranscription-complete" => this.on_retranscription_complete(event, cx),
                "retranscription-error" => this.on_retranscription_error(event, cx),
                "action-items-extracted" => this.on_action_items_extracted(event, cx),
                parley_core::audio::playback::PLAYBACK_ENDED_EVENT => this.on_segment_playback_ended(cx),
                _ => {}
            }
        })];

        let templates = parley_core::summary::templates::list_templates();
        let template_ids: Vec<String> = templates.iter().map(|(id, _, _)| id.clone()).collect();
        let selected_template = format::resolve_default_template(None, &template_ids);
        let custom_prompt =
            cx.new(|cx| InputState::new(window, cx).placeholder("Custom instructions (optional)…"));
        let calendar_picker_query =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search events…"));
        let notes_draft = cx.new(|cx| InputState::new(window, cx).placeholder("Add a note…"));
        let ai_edit_input = cx.new(|cx| InputState::new(window, cx).placeholder("Action item text"));
        let ai_add_input = cx.new(|cx| InputState::new(window, cx).placeholder("Add an action item…"));

        let view = Self {
            meeting_id: None,
            generation: 0,
            title: String::new(),
            title_input,
            editing_title: false,
            created_at: None,
            meeting_folder_path: None,
            transcripts: Vec::new(),
            transcripts_loading: false,
            load_error: None,
            summary_state,
            summary_markdown: String::new(),
            summary_has_content: false,
            summary_phase: SummaryPhase::Idle,
            editing_summary: false,
            summary_editor,
            saving_summary: false,
            _poll_task: None,
            templates,
            selected_template,
            template_user_selected: false,
            custom_prompt,
            show_custom_prompt: false,
            retranscribe_open: false,
            retranscribe_language,
            retranscribe_models: Vec::new(),
            retranscribe_model_index: None,
            retranscribe_in_progress: false,
            retranscribe_progress_pct: 0,
            retranscribe_progress_message: String::new(),
            retranscribe_error: None,
            calendar_event: None,
            calendar_loading: false,
            calendar_mutating: false,
            calendar_picker_open: false,
            calendar_picker_events: None,
            calendar_picker_query,
            calendar_picker_error: None,
            speaker_edit: None,
            playing_segment: None,
            loading_segment: None,
            show_confidence: false,
            notes: Vec::new(),
            notes_composing: false,
            notes_draft,
            notes_saving: false,
            action_items: Vec::new(),
            action_items_loading: false,
            ai_editing_id: None,
            ai_edit_input,
            ai_adding: false,
            ai_add_input,
            ai_extracting: false,
            summary_reveal_pending: String::new(),
            _summary_reveal_task: None,
            _subscriptions: subscriptions,
        };
        view.load_default_template(cx);
        view
    }

    /// Fetch the stored default-template setting (if the Summary settings
    /// page has written one — see `views/settings/summary.rs`) and apply it
    /// as the picker's initial selection. A no-op if there's no pool yet or
    /// no such setting; the picker already has a sane default from
    /// `resolve_default_template(None, ..)`.
    fn load_default_template(&self, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let io = Io::global(cx);
        let template_ids: Vec<String> = self.templates.iter().map(|(id, _, _)| id.clone()).collect();
        cx.spawn(async move |this, cx| {
            let configured = io
                .spawn(async move {
                    SettingsRepository::get_setting::<String>(
                        &pool,
                        parley_core::database::repositories::setting::KEY_DEFAULT_SUMMARY_TEMPLATE,
                    ).await
                })
                .await;
            let default = match configured {
                Ok(Ok(Some(id))) => format::resolve_default_template(Some(&id), &template_ids),
                _ => format::resolve_default_template(None, &template_ids),
            };
            let _ = this.update(cx, |this, cx| {
                // Don't clobber a selection the user already made while this
                // was in flight.
                if !this.template_user_selected {
                    this.selected_template = default;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Show the meeting with `id` (called by the shell on navigation).
    pub fn load(&mut self, id: String, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;

        self.meeting_id = Some(id.clone());
        self.title = String::new();
        self.editing_title = false;
        self.created_at = None;
        self.meeting_folder_path = None;
        self.transcripts = Vec::new();
        self.transcripts_loading = true;
        self.load_error = None;
        self.summary_phase = SummaryPhase::Loading;
        self.summary_has_content = false;
        self.summary_markdown = String::new();
        self.editing_summary = false;
        self.saving_summary = false;
        self._poll_task = None;
        self.retranscribe_open = false;
        self.retranscribe_in_progress = false;
        self.retranscribe_error = None;
        self.retranscribe_progress_pct = 0;
        self.retranscribe_progress_message = String::new();
        self.calendar_event = None;
        self.calendar_loading = true;
        self.calendar_picker_open = false;
        self.calendar_picker_events = None;
        self.calendar_picker_error = None;
        self.speaker_edit = None;
        self.playing_segment = None;
        self.loading_segment = None;
        self.notes = Vec::new();
        self.notes_composing = false;
        self.notes_saving = false;
        self.action_items = Vec::new();
        self.action_items_loading = true;
        self.ai_editing_id = None;
        self.ai_adding = false;
        self.ai_extracting = false;
        self.summary_reveal_pending.clear();
        self._summary_reveal_task = None;
        self.summary_state.update(cx, |state, cx| state.set_text("", cx));
        cx.notify();

        let Some(pool) = AppServices::global(cx).pool() else {
            self.load_error = Some("No database — complete onboarding in the Tauri app first.".into());
            self.transcripts_loading = false;
            return;
        };
        let io = Io::global(cx);

        // Meeting + transcripts + folder path (for retranscription).
        {
            let pool = pool.clone();
            let id = id.clone();
            let io = io.clone();
            cx.spawn(async move |this, cx| {
                let pool_meta = pool.clone();
                let id_meta = id.clone();
                let result = io
                    .spawn(async move { MeetingsRepository::get_meeting(&pool, &id).await })
                    .await;
                let metadata = io
                    .spawn(async move { MeetingsRepository::get_meeting_metadata(&pool_meta, &id_meta).await })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    this.transcripts_loading = false;
                    match result {
                        Ok(Ok(Some(details))) => this.apply_meeting_details(details, cx),
                        Ok(Ok(None)) => this.load_error = Some("Meeting not found".to_string()),
                        Ok(Err(e)) => this.load_error = Some(format!("Failed to load meeting: {e}")),
                        Err(e) => this.load_error = Some(format!("Failed to load meeting: {e}")),
                    }
                    if let Ok(Ok(Some(meta))) = metadata {
                        this.meeting_folder_path = meta.folder_path;
                    }
                    cx.notify();
                });
            })
            .detach();
        }

        // Existing summary (if any), and resume polling if one is already
        // in flight (e.g. started from the Tauri UI).
        self.refresh_summary(generation, cx);
        self.load_calendar_event(generation, cx);
        self.load_notes(generation, cx);
        self.load_action_items(generation, cx);
        self.load_confidence_setting(generation, cx);

        // A clip from the previous meeting shouldn't keep playing once we've
        // navigated away from its transcript.
        parley_core::audio::playback::stop();
    }

    fn apply_meeting_details(&mut self, details: MeetingDetails, _cx: &mut Context<Self>) {
        self.title = details.title;
        self.created_at = chrono::DateTime::parse_from_rfc3339(&details.created_at)
            .ok()
            .map(|dt| dt.with_timezone(&chrono::Utc));
        self.transcripts = details.transcripts;
    }

    fn refresh_summary(&mut self, generation: u64, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move { SummaryProcessesRepository::get_summary_data_for_meeting(&pool, &id).await })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(Ok(Some(process))) => {
                        let markdown = process
                            .result
                            .as_deref()
                            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                            .and_then(|v| v.get("markdown").and_then(|m| m.as_str()).map(str::to_string));
                        if let Some(markdown) = markdown {
                            this.summary_has_content = !markdown.is_empty();
                            this.summary_markdown = markdown.clone();
                            this.summary_state.update(cx, |state, cx| state.set_text(&markdown, cx));
                        }
                        match process.status.to_lowercase().as_str() {
                            "processing" | "pending" | "summarizing" => {
                                this.summary_phase = SummaryPhase::Generating;
                                this.start_poll(generation, cx);
                            }
                            "failed" | "error" => {
                                this.summary_phase =
                                    SummaryPhase::Error(process.error.unwrap_or_else(|| "Summary generation failed".into()));
                            }
                            _ => this.summary_phase = SummaryPhase::Idle,
                        }
                    }
                    Ok(Ok(None)) => this.summary_phase = SummaryPhase::Idle,
                    Ok(Err(e)) => this.summary_phase = SummaryPhase::Error(format!("Failed to load summary: {e}")),
                    Err(e) => this.summary_phase = SummaryPhase::Error(format!("Failed to load summary: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn save_title(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let new_title = self.title_input.read(cx).value().to_string();
        if new_title.trim().is_empty() {
            return;
        }
        self.title = new_title.clone();
        self.editing_title = false;
        cx.notify();

        let io = Io::global(cx);
        cx.spawn(async move |_this, cx| {
            let _ = io
                .spawn(async move { MeetingsRepository::update_meeting_title(&pool, &id, &new_title).await })
                .await;
            let _ = cx.update(|cx| shell::refresh_meetings(cx));
        })
        .detach();
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            alert
                .title("Delete meeting")
                .description("This permanently deletes the meeting, its transcript, and its summary. This cannot be undone.")
                .show_cancel(true)
                .on_ok(move |_, _, cx| {
                    this.update(cx, |this, cx| this.delete(cx));
                    true
                })
        });
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let io = Io::global(cx);
        cx.spawn(async move |_this, cx| {
            let result = io
                .spawn(async move { MeetingsRepository::delete_meeting(&pool, &id).await })
                .await;
            let _ = cx.update(|cx| {
                match result {
                    Ok(Ok(true)) => {
                        shell::refresh_meetings(cx);
                        shell::navigate(Route::Recording, cx);
                    }
                    Ok(Ok(false)) => log::warn!("Meeting already deleted"),
                    Ok(Err(e)) => log::error!("Failed to delete meeting: {e}"),
                    Err(e) => log::error!("Failed to delete meeting: {e}"),
                }
            });
        })
        .detach();
    }

    fn cycle_template(&mut self, cx: &mut Context<Self>) {
        if self.templates.is_empty() {
            return;
        }
        let ids: Vec<String> = self.templates.iter().map(|(id, _, _)| id.clone()).collect();
        let current = ids.iter().position(|id| *id == self.selected_template).unwrap_or(0);
        let next = (current + 1) % ids.len();
        self.selected_template = ids[next].clone();
        self.template_user_selected = true;
        cx.notify();
    }

    fn toggle_custom_prompt(&mut self, cx: &mut Context<Self>) {
        self.show_custom_prompt = !self.show_custom_prompt;
        cx.notify();
    }

    fn generate_summary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        if self.transcripts.is_empty() {
            window.push_notification(Notification::error("No transcript available for this meeting."), cx);
            return;
        }
        let text = format::build_transcript_text(&self.transcripts);
        let sink = AppServices::global(cx).sink.clone();
        let io = Io::global(cx);
        let generation = self.generation;
        let template_id = self.selected_template.clone();
        let custom_prompt = self.custom_prompt.read(cx).value().to_string();

        self.summary_phase = SummaryPhase::Generating;
        self.summary_has_content = false;
        self.summary_markdown = String::new();
        self.summary_state.update(cx, |state, cx| state.set_text("", cx));
        cx.notify();

        let pool_for_config = pool.clone();
        cx.spawn(async move |this, cx| {
            let config = io
                .spawn(async move { SettingsRepository::get_model_config(&pool_for_config).await })
                .await;
            let Ok(Ok(Some(config))) = config else {
                let _ = this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    this.summary_phase =
                        SummaryPhase::Error("No model configured — set one up in Settings first.".into());
                    cx.notify();
                });
                return;
            };

            let _ = io.spawn(SummaryService::process_transcript_background(
                sink,
                pool,
                id,
                text,
                config.provider,
                config.model,
                custom_prompt,
                template_id,
            ));

            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.start_poll(generation, cx);
            });
        })
        .detach();
    }

    fn start_poll(&mut self, generation: u64, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(1500)).await;
                let still_relevant = this
                    .update(cx, |this, cx| {
                        if this.generation != generation {
                            return false;
                        }
                        this.refresh_summary(generation, cx);
                        !matches!(this.summary_phase, SummaryPhase::Generating)
                    })
                    .unwrap_or(true);
                if still_relevant {
                    break;
                }
            }
        });
        self._poll_task = Some(task);
    }

    /// Buffers the incoming delta instead of pushing it straight to
    /// `summary_state` — [`Self::start_summary_reveal_task`] drains the
    /// buffer at a typewriter pace (see `format::summary_reveal_chars_per_tick`).
    /// `summary_markdown` (the source-of-truth for save/copy/export) still
    /// gets the delta immediately: only the *rendered* `summary_state` text
    /// lags behind.
    fn on_summary_stream(&mut self, event: &crate::core_events::CoreEvent, cx: &mut Context<Self>) {
        #[derive(serde::Deserialize)]
        struct Delta {
            meeting_id: String,
            delta: String,
        }
        let Some(payload) = event.decode::<Delta>() else {
            return;
        };
        if self.meeting_id.as_deref() != Some(payload.meeting_id.as_str()) {
            return;
        }
        if payload.delta.is_empty() {
            return;
        }
        self.summary_has_content = true;
        self.summary_markdown.push_str(&payload.delta);

        if cx.reduce_motion() {
            // Mirrors `useTranscriptStreaming.ts`'s reduced-motion path:
            // skip the reveal animation and show text immediately.
            self.summary_state.update(cx, |state, cx| state.push_str(&payload.delta, cx));
            return;
        }

        self.summary_reveal_pending.push_str(&payload.delta);
        if self._summary_reveal_task.is_none() {
            self.start_summary_reveal_task(cx);
        }
    }

    /// Drains `summary_reveal_pending` into `summary_state` a few characters
    /// at a time on a repeating timer, mirroring `useTranscriptStreaming.ts`'s
    /// pacing. Stops itself once the buffer empties (a later delta restarts
    /// it via `on_summary_stream`), so at most one timer runs at a time.
    fn start_summary_reveal_task(&mut self, cx: &mut Context<Self>) {
        let generation = self.generation;
        let task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(format::SUMMARY_REVEAL_INTERVAL_MS))
                    .await;
                let done = this
                    .update(cx, |this, cx| {
                        if this.generation != generation || this.summary_reveal_pending.is_empty() {
                            return true;
                        }
                        let n = format::summary_reveal_chars_per_tick(this.summary_reveal_pending.len())
                            .min(this.summary_reveal_pending.len());
                        // Drain on a char boundary — `n` counts bytes of a
                        // UTF-8 buffer, which may land mid-codepoint.
                        let mut boundary = n;
                        while boundary < this.summary_reveal_pending.len()
                            && !this.summary_reveal_pending.is_char_boundary(boundary)
                        {
                            boundary += 1;
                        }
                        let chunk: String = this.summary_reveal_pending.drain(..boundary).collect();
                        this.summary_state.update(cx, |state, cx| state.push_str(&chunk, cx));
                        this.summary_reveal_pending.is_empty()
                    })
                    .unwrap_or(true);
                if done {
                    break;
                }
            }
            let _ = this.update(cx, |this, cx| {
                if this.generation == generation {
                    this._summary_reveal_task = None;
                    cx.notify();
                }
            });
        });
        self._summary_reveal_task = Some(task);
    }

    // ---- Summary editing (zorite) ----------------------------------------

    fn start_edit_summary(&mut self, cx: &mut Context<Self>) {
        let markdown = self.summary_markdown.clone();
        self.summary_editor.update(cx, |editor, cx| editor.set_text(markdown, cx));
        self.editing_summary = true;
        cx.notify();
    }

    fn cancel_edit_summary(&mut self, cx: &mut Context<Self>) {
        self.editing_summary = false;
        cx.notify();
    }

    /// Whether the summary editor is open with edits that differ from the
    /// saved markdown — the trigger for the navigation-away guard (see
    /// [`format::has_unsaved_summary_edits`] and `shell::AppShell::guard_navigate`).
    pub fn has_unsaved_summary_edits(&self, cx: &App) -> bool {
        format::has_unsaved_summary_edits(
            self.editing_summary,
            &self.summary_editor.read(cx).text(),
            &self.summary_markdown,
        )
    }

    /// Discard the in-progress summary edit without saving. Used by the
    /// "Discard" choice of the unsaved-changes dialog.
    pub fn discard_summary_edits(&mut self, cx: &mut Context<Self>) {
        self.cancel_edit_summary(cx);
    }

    /// Save the in-progress summary edit. Used by the "Save" choice of the
    /// unsaved-changes dialog — fire-and-forget, like the regular Save
    /// button: the save task keeps running (and updates this entity) even
    /// after the shell has already navigated away, guarded by `generation`.
    pub fn save_summary_and_leave(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_summary(window, cx);
    }

    fn save_summary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let markdown = self.summary_editor.read(cx).text().to_string();
        self.saving_summary = true;
        cx.notify();

        let io = Io::global(cx);
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let value = serde_json::json!({ "markdown": markdown.clone() });
            let pool_for_md = pool.clone();
            let id_for_md = id.clone();
            let value_for_md = value.clone();
            let saved = io
                .spawn(async move { SummaryProcessesRepository::update_meeting_summary(&pool, &id, &value).await })
                .await;
            // Best-effort sidecar write, same as the Tauri save command —
            // failures here don't fail the save, the DB is the source of truth.
            let _ = io
                .spawn(async move { markdown_export::write_summary_md(&pool_for_md, &id_for_md, &value_for_md).await })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.saving_summary = false;
                if this.generation != generation {
                    return;
                }
                match saved {
                    Ok(Ok(true)) => {
                        this.summary_markdown = markdown.clone();
                        this.summary_has_content = !markdown.is_empty();
                        this.summary_state.update(cx, |state, cx| state.set_text(&markdown, cx));
                        this.editing_summary = false;
                    }
                    _ => {
                        log::error!("meeting: failed to save edited summary");
                    }
                }
                cx.notify();
            });
        })
        .detach();

        window.push_notification(Notification::info("Saving summary…"), cx);
    }

    // ---- Copy / export -----------------------------------------------------

    fn copy_transcript(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.transcripts.is_empty() {
            window.push_notification(Notification::error("No transcripts available to copy"), cx);
            return;
        }
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let text = format::copy_transcript_text(&id, &self.title, self.created_at, &self.transcripts);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        window.push_notification(Notification::success("Transcript copied to clipboard"), cx);
    }

    fn copy_summary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.summary_markdown.trim().is_empty() {
            window.push_notification(Notification::error("No summary content available to copy"), cx);
            return;
        }
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let text = format::copy_summary_text(&id, &self.title, self.created_at, &self.summary_markdown);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        window.push_notification(Notification::success("Summary copied to clipboard"), cx);
    }

    /// `format` is `"markdown"` or `"json"`, matching `export_meeting`'s
    /// core function.
    fn export_copy(&mut self, format: &'static str, cx: &mut Context<Self>) {
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let io = Io::global(cx);
        cx.spawn(async move |_this, cx| {
            let result = io
                .spawn(async move { parley_core::export::build_export(&pool, &id, format).await })
                .await;
            let _ = cx.update(|cx| match result {
                Ok(Ok(export)) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(export.content));
                    let label = if format == "json" { "JSON" } else { "Markdown" };
                    notify(cx, Notification::success(format!("Meeting copied as {label}")));
                }
                _ => notify(cx, Notification::error("Failed to export meeting")),
            });
        })
        .detach();
    }

    /// Export, then a native save-file dialog, mirroring
    /// `export_meeting_to_file`'s Tauri command (minus the Tauri dialog
    /// plugin — GPUI's own `prompt_for_new_path`).
    fn export_save(&mut self, format: &'static str, cx: &mut Context<Self>) {
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let io = Io::global(cx);
        cx.spawn(async move |_this, cx| {
            let result = io
                .spawn(async move { parley_core::export::build_export(&pool, &id, format).await })
                .await;
            let export = match result {
                Ok(Ok(export)) => export,
                _ => {
                    let _ = cx.update(|cx| notify(cx, Notification::error("Failed to export meeting")));
                    return;
                }
            };

            let home = std::env::var("HOME").map(std::path::PathBuf::from).unwrap_or_else(|_| std::path::PathBuf::from("/"));
            let rx = cx.update(|cx| cx.prompt_for_new_path(&home, Some(&export.filename)));
            match rx.await {
                Ok(Ok(Some(path))) => {
                    let content = export.content;
                    let write_result = io.spawn(async move { std::fs::write(&path, content) }).await;
                    let _ = cx.update(|cx| match write_result {
                        Ok(Ok(())) => notify(cx, Notification::success("Meeting exported")),
                        _ => notify(cx, Notification::error("Failed to write export")),
                    });
                }
                Ok(Ok(None)) => {
                    // Cancelled the save dialog — not an error, no toast.
                }
                _ => {
                    let _ = cx.update(|cx| notify(cx, Notification::error("Failed to export meeting")));
                }
            }
        })
        .detach();
    }

    // ---- Retranscription -----------------------------------------------------

    fn open_retranscribe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.meeting_folder_path.is_none() {
            window.push_notification(Notification::error("This meeting has no saved audio to retranscribe."), cx);
            return;
        }
        self.retranscribe_open = true;
        self.retranscribe_error = None;
        cx.notify();

        if !self.retranscribe_models.is_empty() {
            return;
        }
        let io = Io::global(cx);
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let models = io
                .spawn(async move { parley_core::whisper_engine::whisper_get_available_models().await })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                if let Ok(Ok(models)) = models {
                    this.retranscribe_models = models
                        .into_iter()
                        .filter(|m| matches!(m.status, parley_core::whisper_engine::ModelStatus::Available))
                        .map(|m| m.name)
                        .collect();
                    if this.retranscribe_model_index.is_none() && !this.retranscribe_models.is_empty() {
                        this.retranscribe_model_index = Some(0);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn close_retranscribe(&mut self, cx: &mut Context<Self>) {
        self.retranscribe_open = false;
        cx.notify();
    }

    fn cycle_retranscribe_model(&mut self, cx: &mut Context<Self>) {
        if self.retranscribe_models.is_empty() {
            return;
        }
        let next = self.retranscribe_model_index.map(|i| (i + 1) % self.retranscribe_models.len()).unwrap_or(0);
        self.retranscribe_model_index = Some(next);
        cx.notify();
    }

    fn start_retranscribe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let Some(folder) = self.meeting_folder_path.clone() else {
            return;
        };
        if parley_core::audio::retranscription::is_retranscription_in_progress() {
            window.push_notification(Notification::error("Retranscription already in progress"), cx);
            return;
        }

        let language = self.retranscribe_language.read(cx).value().to_string();
        let language = {
            let trimmed = language.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
                None
            } else {
                Some(trimmed.to_string())
            }
        };
        let model = self
            .retranscribe_model_index
            .and_then(|i| self.retranscribe_models.get(i).cloned());
        let provider = model.as_ref().map(|_| "localWhisper".to_string());

        self.retranscribe_in_progress = true;
        self.retranscribe_error = None;
        self.retranscribe_progress_pct = 0;
        self.retranscribe_progress_message = "Starting…".to_string();
        cx.notify();

        let sink = AppServices::global(cx).sink.clone();
        let pool = AppServices::global(cx).pool();
        let io = Io::global(cx);
        io.spawn(async move {
            if let Err(e) = parley_core::audio::retranscription::start_retranscription_with(
                sink, pool, id, folder, language, model, provider,
            )
            .await
            {
                log::error!("meeting: retranscription failed: {e}");
            }
        });
    }

    fn cancel_retranscribe(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        parley_core::audio::retranscription::cancel_retranscription();
    }

    fn on_retranscription_progress(&mut self, event: &crate::core_events::CoreEvent, cx: &mut Context<Self>) {
        let Some(payload) = event.decode::<parley_core::audio::retranscription::RetranscriptionProgress>() else {
            return;
        };
        if self.meeting_id.as_deref() != Some(payload.meeting_id.as_str()) {
            return;
        }
        self.retranscribe_progress_pct = payload.progress_percentage;
        self.retranscribe_progress_message = payload.message;
        cx.notify();
    }

    fn on_retranscription_complete(&mut self, event: &crate::core_events::CoreEvent, cx: &mut Context<Self>) {
        let Some(payload) = event.decode::<parley_core::audio::retranscription::RetranscriptionResult>() else {
            return;
        };
        if self.meeting_id.as_deref() != Some(payload.meeting_id.as_str()) {
            return;
        }
        self.retranscribe_in_progress = false;
        self.retranscribe_open = false;
        cx.notify();
        notify(cx, Notification::success(format!("Retranscription complete: {} segments", payload.segments_count)));
        self.reload_transcript(cx);
        shell::refresh_meetings(cx);
    }

    fn on_retranscription_error(&mut self, event: &crate::core_events::CoreEvent, cx: &mut Context<Self>) {
        let Some(payload) = event.decode::<parley_core::audio::retranscription::RetranscriptionError>() else {
            return;
        };
        if self.meeting_id.as_deref() != Some(payload.meeting_id.as_str()) {
            return;
        }
        self.retranscribe_in_progress = false;
        self.retranscribe_error = Some(payload.error);
        cx.notify();
    }

    /// Re-fetch just the transcript after a retranscription completes.
    fn reload_transcript(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let io = Io::global(cx);
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = io.spawn(async move { MeetingsRepository::get_meeting(&pool, &id).await }).await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                if let Ok(Ok(Some(details))) = result {
                    this.transcripts = details.transcripts;
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---- Calendar event link -------------------------------------------
    // Mirrors `CalendarEventPanel.tsx` / `CalendarEventPicker.tsx`, calling
    // the same core fns the Tauri `calendar_get_event_for_meeting` /
    // `calendar_list_events` / `calendar_link_meeting` commands wrap.

    fn load_calendar_event(&mut self, generation: u64, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            self.calendar_loading = false;
            return;
        };
        let Some(id) = self.meeting_id.clone() else {
            self.calendar_loading = false;
            return;
        };
        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move { CalendarRepository::get_event_for_meeting(&pool, &id).await })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.calendar_loading = false;
                match result {
                    Ok(Ok(event)) => this.calendar_event = event,
                    Ok(Err(e)) => log::warn!("meeting: get_event_for_meeting failed: {e}"),
                    Err(e) => log::warn!("meeting: get_event_for_meeting task panicked: {e}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Open the "link a calendar event" picker, fetching events within
    /// `±CALENDAR_WINDOW_DAYS` of the meeting's `created_at` (falling back
    /// to "now" if unknown) — mirrors the picker's default anchor/window.
    fn open_calendar_picker(&mut self, cx: &mut Context<Self>) {
        self.calendar_picker_open = true;
        self.calendar_picker_events = None;
        self.calendar_picker_error = None;
        cx.notify();

        let Some(pool) = AppServices::global(cx).pool() else {
            self.calendar_picker_error = Some("No database — complete onboarding first.".into());
            return;
        };
        let anchor = self.created_at.unwrap_or_else(chrono::Utc::now);
        let window = chrono::Duration::days(CALENDAR_WINDOW_DAYS);
        let from = anchor - window;
        let to = anchor + window;
        let io = Io::global(cx);
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move { CalendarRepository::list_events_in_range(&pool, from, to).await })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation || !this.calendar_picker_open {
                    return;
                }
                match result {
                    Ok(Ok(mut events)) => {
                        calendar_format::sort_by_proximity(&mut events, anchor, |e| e.start_at);
                        this.calendar_picker_events = Some(events);
                    }
                    Ok(Err(e)) => this.calendar_picker_error = Some(e.to_string()),
                    Err(e) => this.calendar_picker_error = Some(format!("Task panicked: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn close_calendar_picker(&mut self, cx: &mut Context<Self>) {
        self.calendar_picker_open = false;
        cx.notify();
    }

    /// Link (or, with `event_id: None`, unlink) the meeting to a calendar
    /// event — mirrors `linkMeetingToCalendarEvent`.
    fn pick_calendar_event(&mut self, event_id: Option<String>, cx: &mut Context<Self>) {
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        self.calendar_mutating = true;
        cx.notify();

        let io = Io::global(cx);
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let event_id_for_task = event_id.clone();
            let result = io
                .spawn(async move {
                    link_meeting_with_snapshot(&pool, &id, event_id_for_task.as_deref()).await
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.calendar_mutating = false;
                match result {
                    Ok(Ok(true)) => {
                        this.calendar_picker_open = false;
                        this.load_calendar_event(generation, cx);
                    }
                    Ok(Ok(false)) => log::warn!("meeting: calendar link no-op (meeting not found)"),
                    Ok(Err(e)) => notify(cx, Notification::error(format!("Couldn't link that calendar event: {e}"))),
                    Err(e) => notify(cx, Notification::error(format!("Calendar link task panicked: {e}"))),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn unlink_calendar_event(&mut self, cx: &mut Context<Self>) {
        self.pick_calendar_event(None, cx);
    }

    // ---- Per-segment speaker chip edit -----------------------------------
    // Mirrors `EditableSpeakerChip.tsx`: rename a named voice profile across
    // meetings, or (for an unnamed "Speaker N" cluster) either promote it to
    // a brand-new profile scoped to this meeting's rename, or merge it into
    // an existing profile.

    /// Toggle the edit panel for one transcript segment's speaker chip.
    /// Clicking the already-open chip's own label closes it.
    fn toggle_speaker_edit(
        &mut self,
        segment_id: String,
        speaker: String,
        voice_profile_id: Option<String>,
        click_position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = format!("{segment_id}:{speaker}");
        if self.speaker_edit.as_ref().is_some_and(|s| s.key == key) {
            self.speaker_edit = None;
            cx.notify();
            return;
        }

        let name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("e.g. Alice Smith").default_value(speaker.clone()));
        let email_input = cx.new(|cx| InputState::new(window, cx).placeholder("Email (optional)"));
        window.focus(&name_input.read(cx).focus_handle(cx), cx);
        let attendees =
            self.calendar_event.as_ref().map(|e| speaker_chip::attendee_suggestions(&e.attendees)).unwrap_or_default();
        self.speaker_edit = Some(SpeakerEditState {
            key: key.clone(),
            speaker: speaker.clone(),
            voice_profile_id: voice_profile_id.clone(),
            name_input,
            email_input,
            profiles: Vec::new(),
            profiles_loading: true,
            merge_target: None,
            saving: false,
            error: None,
            click_position,
            attendees,
        });
        cx.notify();

        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let io = Io::global(cx);
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = io.spawn(async move { VoiceProfilesRepository::list_all(&pool).await }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                let Some(state) = this.speaker_edit.as_mut() else {
                    return;
                };
                if state.key != key {
                    return;
                }
                state.profiles_loading = false;
                if let Ok(Ok(profiles)) = result {
                    // Named-profile edit: prefill the email field from the
                    // profile's stored value (name is already prefilled from
                    // the displayed label).
                    if let Some(vp_id) = state.voice_profile_id.clone() {
                        if let Some(me) = profiles.iter().find(|p| p.id == vp_id) {
                            let email = me.email.clone().unwrap_or_default();
                            state.email_input.update(cx, |s, cx| s.set_value(email, window, cx));
                        }
                    }
                    state.profiles = profiles;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn close_speaker_edit(&mut self, cx: &mut Context<Self>) {
        self.speaker_edit = None;
        cx.notify();
    }

    /// Fill the rename form from a clicked calendar-attendee suggestion —
    /// mirrors the attendee chip's `onClick` in `EditableSpeakerChip.tsx`.
    fn apply_attendee_suggestion(&mut self, label: String, email: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.speaker_edit.as_ref() else {
            return;
        };
        let name_input = state.name_input.clone();
        let email_input = state.email_input.clone();
        name_input.update(cx, |s, cx| s.set_value(label, window, cx));
        email_input.update(cx, |s, cx| s.set_value(email.unwrap_or_default(), window, cx));
        cx.notify();
    }

    /// Cycle the unnamed-cluster panel's merge target through
    /// "Create new speaker…" then each existing profile, in order —
    /// gpui-kit has no dropdown-list widget in use elsewhere in this view,
    /// so this mirrors the same cycle-button pattern already used for the
    /// summary template picker.
    fn cycle_speaker_merge_target(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.speaker_edit.as_mut() else {
            return;
        };
        if state.profiles.is_empty() {
            return;
        }
        let options: Vec<Option<String>> =
            std::iter::once(None).chain(state.profiles.iter().map(|p| Some(p.id.clone()))).collect();
        let current = options.iter().position(|o| *o == state.merge_target).unwrap_or(0);
        state.merge_target = options[(current + 1) % options.len()].clone();
        cx.notify();
    }

    fn save_speaker_edit(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.speaker_edit.as_ref() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let Some(meeting_id) = self.meeting_id.clone() else {
            return;
        };
        let speaker = state.speaker.clone();
        let voice_profile_id = state.voice_profile_id.clone();
        let merge_target = state.merge_target.clone();
        let name = state.name_input.read(cx).value().trim().to_string();
        let email = {
            let raw = state.email_input.read(cx).value().trim().to_string();
            if raw.is_empty() { None } else { Some(raw) }
        };
        if !speaker_chip::can_save_speaker_edit(merge_target.as_deref(), &name) {
            return;
        }
        let key = state.key.clone();

        {
            let state = self.speaker_edit.as_mut().unwrap();
            state.saving = true;
            state.error = None;
        }
        cx.notify();

        let io = Io::global(cx);
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let outcome: Result<(), String> = if let Some(profile_id) = merge_target {
                match io
                    .spawn(async move { merge_cluster_into_profile_core(&pool, &meeting_id, &speaker, &profile_id).await })
                    .await
                {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(e)) => Err(e),
                    Err(e) => Err(format!("Merge task panicked: {e}")),
                }
            } else if let Some(vp_id) = voice_profile_id {
                match io
                    .spawn(async move { VoiceProfilesRepository::update_profile(&pool, &vp_id, &name, email.as_deref()).await })
                    .await
                {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(e) => Err(format!("Update task panicked: {e}")),
                }
            } else {
                match io
                    .spawn(async move {
                        promote_speaker_to_profile_core(&pool, &speaker, &name, email.as_deref(), &meeting_id).await
                    })
                    .await
                {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(e)) => Err(e),
                    Err(e) => Err(format!("Promote task panicked: {e}")),
                }
            };

            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                let still_open = this.speaker_edit.as_ref().is_some_and(|s| s.key == key);
                match outcome {
                    Ok(()) => {
                        this.speaker_edit = None;
                        this.reload_transcript(cx);
                    }
                    Err(e) if still_open => {
                        let state = this.speaker_edit.as_mut().unwrap();
                        state.saving = false;
                        state.error = Some(e);
                    }
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---- Per-segment audio playback --------------------------------------
    // Mirrors `SegmentAudioContext.tsx`: only one clip plays at a time,
    // played natively (not in-webview — moot here, there is no webview) via
    // the same core fns the Tauri `play_meeting_audio_clip` /
    // `stop_meeting_audio_clip` commands wrap.

    fn toggle_segment_playback(
        &mut self,
        segment_id: String,
        start_secs: f64,
        end_secs: f64,
        source: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.playing_segment.as_deref() == Some(segment_id.as_str())
            || self.loading_segment.as_deref() == Some(segment_id.as_str())
        {
            self.stop_segment_playback(cx);
            return;
        }
        self.play_segment(segment_id, start_secs, end_secs, source, cx);
    }

    fn play_segment(
        &mut self,
        segment_id: String,
        start_secs: f64,
        end_secs: f64,
        source: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(meeting_id) = self.meeting_id.clone() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        self.playing_segment = None;
        self.loading_segment = Some(segment_id.clone());
        cx.notify();

        let sink = AppServices::global(cx).sink.clone();
        let io = Io::global(cx);
        let generation = self.generation;
        let segment_for_task = segment_id.clone();
        cx.spawn(async move |this, cx| {
            let result: Result<(), String> = match io
                .spawn(async move {
                    let bytes = parley_core::audio::clip::extract_clip_wav(
                        &pool,
                        &meeting_id,
                        start_secs,
                        end_secs,
                        source.as_deref(),
                    )
                    .await?;
                    let (samples, sample_rate, channels) = parley_core::audio::clip::parse_wav_pcm16(&bytes)?;
                    parley_core::audio::playback::play_pcm_i16(&sink, samples, sample_rate, channels)
                })
                .await
            {
                Ok(r) => r,
                Err(e) => Err(format!("Clip playback task panicked: {e}")),
            };

            let _ = this.update(cx, |this, cx| {
                if this.generation != generation || this.loading_segment.as_deref() != Some(segment_for_task.as_str())
                {
                    return;
                }
                this.loading_segment = None;
                match result {
                    Ok(()) => this.playing_segment = Some(segment_for_task),
                    Err(e) => notify(cx, Notification::error(e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn stop_segment_playback(&mut self, cx: &mut Context<Self>) {
        self.playing_segment = None;
        self.loading_segment = None;
        cx.notify();
        parley_core::audio::playback::stop();
    }

    fn on_segment_playback_ended(&mut self, cx: &mut Context<Self>) {
        self.playing_segment = None;
        self.loading_segment = None;
        cx.notify();
    }

    // ---- Confidence indicator ---------------------------------------------

    /// Read `ui_config.showConfidenceIndicator` (default `true`, matching
    /// `ConfigContext.tsx`'s `readStoredBool(.., true)`) once per `load()`.
    /// Read directly via `SettingsRepository` rather than `SettingsCache`
    /// (the Settings page's own global) — this view doesn't depend on the
    /// Settings page being visited first.
    fn load_confidence_setting(&mut self, generation: u64, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let result = io.spawn(async move { SettingsRepository::get_setting_json(&pool, KEY_UI_CONFIG).await }).await;
            let show = match result {
                Ok(Ok(Some(json))) => serde_json::from_str::<serde_json::Value>(&json)
                    .ok()
                    .and_then(|v| v.get("showConfidenceIndicator").and_then(|v| v.as_bool()))
                    .unwrap_or(true),
                _ => true,
            };
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.show_confidence = show;
                cx.notify();
            });
        })
        .detach();
    }

    // ---- Meeting notes ------------------------------------------------------
    // Mirrors `MeetingNotesPanel.tsx`: load silently (no loading/error UI),
    // oldest-first, append-only (delete + re-add instead of editing).

    fn load_notes(&mut self, generation: u64, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let result = io.spawn(async move { MeetingNotesRepository::list_by_meeting(&pool, &id).await }).await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                if let Ok(Ok(notes)) = result {
                    this.notes = notes;
                }
                // Silent on error, matching the React panel (console.error only).
                cx.notify();
            });
        })
        .detach();
    }

    fn open_note_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.notes_composing = true;
        self.notes_draft.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    fn cancel_note_composer(&mut self, cx: &mut Context<Self>) {
        self.notes_composing = false;
        cx.notify();
    }

    fn save_note(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let body = self.notes_draft.read(cx).value().trim().to_string();
        if body.is_empty() || self.notes_saving {
            return;
        }
        self.notes_saving = true;
        cx.notify();

        let io = Io::global(cx);
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move { MeetingNotesRepository::create(&pool, &id, &body, NOTE_SOURCE_MANUAL).await })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.notes_saving = false;
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(Ok(note)) => {
                        this.notes.push(note);
                        this.notes_composing = false;
                        this.notes_draft.update(cx, |s, cx| s.set_value("", window, cx));
                    }
                    Ok(Err(e)) => notify(cx, Notification::error(format!("Failed to add note: {e}"))),
                    Err(e) => notify(cx, Notification::error(format!("Failed to add note: {e}"))),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Optimistic removal, mirroring `MeetingNotesPanel.tsx`'s
    /// `handleDelete`: removed from view immediately, re-inserted
    /// (sorted back into place) on failure.
    fn delete_note(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let removed = self.notes.iter().position(|n| n.id == id).map(|ix| self.notes.remove(ix));
        cx.notify();

        let io = Io::global(cx);
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = io.spawn(async move { MeetingNotesRepository::delete(&pool, &id).await }).await;
            let ok = matches!(result, Ok(Ok(true)));
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                if !ok {
                    if let Some(note) = removed {
                        this.notes.push(note);
                        this.notes.sort_by(|a, b| a.created_at.cmp(&b.created_at));
                    }
                    notify(cx, Notification::error("Failed to delete note"));
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---- Per-meeting action items -------------------------------------------
    // Mirrors `ActionItemsPanel.tsx`: `list_by_meeting`'s order (open first,
    // oldest-first within each group) is used as-is, no client-side re-sort.

    fn load_action_items(&mut self, generation: u64, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            self.action_items_loading = false;
            return;
        };
        let Some(id) = self.meeting_id.clone() else {
            self.action_items_loading = false;
            return;
        };
        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let result = io.spawn(async move { ActionItemsRepository::list_by_meeting(&pool, &id).await }).await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.action_items_loading = false;
                if let Ok(Ok(items)) = result {
                    this.action_items = items;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn on_action_items_extracted(&mut self, event: &crate::core_events::CoreEvent, cx: &mut Context<Self>) {
        #[derive(serde::Deserialize)]
        struct Payload {
            meeting_id: String,
        }
        let Some(payload) = event.decode::<Payload>() else {
            return;
        };
        if self.meeting_id.as_deref() != Some(payload.meeting_id.as_str()) {
            return;
        }
        self.ai_extracting = false;
        self.load_action_items(self.generation, cx);
    }

    fn toggle_action_item_status(&mut self, id: String, currently_done: bool, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let next = if currently_done { STATUS_OPEN } else { STATUS_DONE };
        if let Some(item) = self.action_items.iter_mut().find(|i| i.id == id) {
            item.status = next.to_string();
        }
        cx.notify();

        let io = Io::global(cx);
        let generation = self.generation;
        let id_for_task = id.clone();
        cx.spawn(async move |this, cx| {
            let result = io.spawn(async move { ActionItemsRepository::set_status(&pool, &id_for_task, next).await }).await;
            if !matches!(result, Ok(Ok(Some(_)))) {
                let _ = this.update(cx, |this, cx| {
                    if this.generation == generation {
                        this.load_action_items(generation, cx);
                    }
                });
            }
        })
        .detach();
    }

    fn start_action_item_edit(&mut self, item_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.action_items.iter().find(|i| i.id == item_id) else {
            return;
        };
        let text = item.text.clone();
        self.ai_editing_id = Some(item_id);
        self.ai_edit_input.update(cx, |s, cx| s.set_value(text, window, cx));
        cx.notify();
    }

    fn cancel_action_item_edit(&mut self, cx: &mut Context<Self>) {
        self.ai_editing_id = None;
        cx.notify();
    }

    fn save_action_item_edit(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.ai_editing_id.take() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let text = self.ai_edit_input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        if let Some(item) = self.action_items.iter_mut().find(|i| i.id == id) {
            item.text = text.clone();
        }
        cx.notify();

        let io = Io::global(cx);
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = io.spawn(async move { ActionItemsRepository::update(&pool, &id, Some(&text), None, None).await }).await;
            if !matches!(result, Ok(Ok(Some(_)))) {
                let _ = this.update(cx, |this, cx| {
                    if this.generation == generation {
                        this.load_action_items(generation, cx);
                    }
                });
            }
        })
        .detach();
    }

    fn delete_action_item(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        self.action_items.retain(|i| i.id != id);
        cx.notify();

        let io = Io::global(cx);
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = io.spawn(async move { ActionItemsRepository::delete(&pool, &id).await }).await;
            if !matches!(result, Ok(Ok(true))) {
                let _ = this.update(cx, |this, cx| {
                    if this.generation == generation {
                        this.load_action_items(generation, cx);
                    }
                });
            }
        })
        .detach();
    }

    fn open_action_item_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ai_adding = true;
        self.ai_add_input.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    fn cancel_action_item_composer(&mut self, cx: &mut Context<Self>) {
        self.ai_adding = false;
        cx.notify();
    }

    fn save_new_action_item(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let text = self.ai_add_input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.ai_adding = false;
        cx.notify();

        let io = Io::global(cx);
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let item = NewActionItem { text, ..Default::default() };
            let result =
                io.spawn(async move { ActionItemsRepository::create(&pool, &id, &item, ACTION_ITEM_SOURCE_MANUAL).await }).await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(Ok(_)) => this.load_action_items(generation, cx),
                    Ok(Err(e)) => notify(cx, Notification::error(format!("Failed to add action item: {e}"))),
                    Err(e) => notify(cx, Notification::error(format!("Failed to add action item: {e}"))),
                }
            });
        })
        .detach();
    }

    /// "Extract from summary" / "Re-extract" — mirrors the Tauri
    /// `extract_action_items` command: try the transcript-grounded
    /// extractor first, falling back to the summary-markdown extractor if
    /// it errors (e.g. no transcript rows to window over).
    fn extract_action_items(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.meeting_id.clone() else {
            return;
        };
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        if self.ai_extracting {
            return;
        }
        self.ai_extracting = true;
        cx.notify();

        let sink = AppServices::global(cx).sink.clone();
        let summary_markdown = self.summary_markdown.clone();
        let io = Io::global(cx);
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let pool_for_config = pool.clone();
            let config = io.spawn(async move { SettingsRepository::get_model_config(&pool_for_config).await }).await;
            let Ok(Ok(Some(config))) = config else {
                let _ = this.update(cx, |this, cx| {
                    this.ai_extracting = false;
                    if this.generation == generation {
                        notify(cx, Notification::error("No model configured — set one up in Settings first."));
                    }
                    cx.notify();
                });
                return;
            };

            let provider = config.provider;
            let model = config.model;
            let pool_for_transcript = pool.clone();
            let sink_for_transcript = sink.clone();
            let id_for_transcript = id.clone();
            let provider_for_transcript = provider.clone();
            let model_for_transcript = model.clone();
            let transcript_result = io
                .spawn(async move {
                    parley_core::summary::transcript_action_items::extract_from_transcript(
                        sink_for_transcript.as_ref(),
                        &pool_for_transcript,
                        &id_for_transcript,
                        &provider_for_transcript,
                        &model_for_transcript,
                    )
                    .await
                })
                .await;

            let outcome = match transcript_result {
                Ok(Ok(count)) => Ok(count),
                _ => {
                    // Fall back to extracting from the stored summary markdown.
                    let id_for_summary = id.clone();
                    io.spawn(async move {
                        parley_core::summary::action_extraction::extract_for_meeting(
                            sink.as_ref(),
                            &pool,
                            &id_for_summary,
                            &summary_markdown,
                            &provider,
                            &model,
                        )
                        .await
                    })
                    .await
                    .unwrap_or_else(|e| Err(format!("Task panicked: {e}")))
                }
            };

            let _ = this.update(cx, |this, cx| {
                this.ai_extracting = false;
                if this.generation != generation {
                    return;
                }
                match outcome {
                    Ok(_) => this.load_action_items(generation, cx),
                    Err(e) => notify(cx, Notification::error(format!("Extraction failed: {e}"))),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// Show a notification on the main window from a context that doesn't carry
/// a `Window` (an async task after an `.await`, or a core-event
/// subscription callback) — via the `MainWindow` global stashed at startup.
fn notify(cx: &mut App, notification: Notification) {
    crate::ui::with_main_window(cx, |window, cx| window.push_notification(notification, cx));
}

/// The floating speaker-edit popover, rendered from the page root (outside
/// the virtualized `uniform_list`) as a `deferred(anchored()...)` positioned
/// at the chip's click point — mirrors `EditableSpeakerChip.tsx`'s popover.
/// gpui-kit's `Popover` doesn't anchor cleanly to a trigger living inside a
/// recycled `uniform_list` row, so this tracks the click position in
/// [`SpeakerEditState::click_position`] instead and positions the card
/// directly. Free function (not a method) because [`MeetingView::render`]
/// only has an `Entity<MeetingView>` for the closures it hands out, not a
/// second `&mut Context<Self>` borrow.
fn render_floating_speaker_edit(state: &SpeakerEditState, entity: Entity<MeetingView>, cx: &mut App) -> AnyElement {
    let is_named_profile = state.voice_profile_id.is_some();
    let merging = state.merge_target.is_some();
    let merge_target_profile = state.merge_target.as_ref().and_then(|id| state.profiles.iter().find(|p| &p.id == id));
    let name = state.name_input.read(cx).value().trim().to_string();
    let can_save = speaker_chip::can_save_speaker_edit(state.merge_target.as_deref(), &name) && !state.saving;

    let mut panel = v_flex()
        .id("speaker-edit-popover")
        .occlude()
        .w(px(288.))
        .gap_2()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(ActiveTheme::theme(cx).border)
        .bg(ActiveTheme::theme(cx).background)
        .shadow_lg()
        .on_mouse_down_out({
            let entity = entity.clone();
            move |_, _, cx| entity.update(cx, |this, cx| this.close_speaker_edit(cx))
        })
        .on_key_down({
            let entity = entity.clone();
            move |ev, _, cx| {
                if ev.keystroke.key == "escape" {
                    entity.update(cx, |this, cx| this.close_speaker_edit(cx));
                }
            }
        })
        .child(div().text_sm().font_semibold().child(speaker_chip::edit_panel_title(is_named_profile)));

    if !merging && !state.attendees.is_empty() {
        let suggestions = speaker_chip::filter_attendee_suggestions(&state.attendees, &name);
        if !suggestions.is_empty() {
            let mut chips = h_flex().gap_1().flex_wrap();
            for s in suggestions {
                let label = s.label.clone();
                let email = s.email.clone();
                let entity_for_pick = entity.clone();
                let selected = name == s.label;
                chips = chips.child(
                    Button::new(SharedString::from(format!("attendee-suggestion-{}", s.label)))
                        .xsmall()
                        .when(selected, |b| b.primary())
                        .when(!selected, |b| b.outline())
                        .label(s.label.clone())
                        .on_click(move |_, window, cx| {
                            entity_for_pick.update(cx, |this, cx| {
                                this.apply_attendee_suggestion(label.clone(), email.clone(), window, cx)
                            });
                        }),
                );
            }
            panel = panel
                .child(div().text_xs().text_color(ActiveTheme::theme(cx).muted_foreground).child("From this meeting's calendar"))
                .child(chips);
        }
    }

    if !is_named_profile && !state.profiles.is_empty() {
        let label = merge_target_profile.map(|p| p.name.clone()).unwrap_or_else(|| "Create new speaker…".to_string());
        let entity_for_cycle = entity.clone();
        panel = panel.child(
            h_flex()
                .gap_2()
                .items_center()
                .child(div().text_xs().text_color(ActiveTheme::theme(cx).muted_foreground).child("Target"))
                .child(
                    Button::new("cycle-speaker-merge-target")
                        .outline()
                        .label(label)
                        .on_click(move |_, _, cx| {
                            entity_for_cycle.update(cx, |this, cx| this.cycle_speaker_merge_target(cx));
                        }),
                ),
        );
    }

    if merging {
        if let Some(target) = merge_target_profile {
            panel = panel.child(
                div()
                    .text_xs()
                    .text_color(ActiveTheme::theme(cx).muted_foreground)
                    .child(format!(
                        "This cluster's samples will be folded into {}{}, and every transcript from this meeting \
                         will be relabelled.",
                        target.name,
                        target.email.as_deref().map(|e| format!(" ({e})")).unwrap_or_default(),
                    )),
            );
        }
    } else {
        panel = panel
            .child(div().w_full().child(Input::new(&state.name_input)))
            .child(div().w_full().child(Input::new(&state.email_input)));
    }

    if let Some(err) = &state.error {
        panel = panel.child(div().text_xs().text_color(ActiveTheme::theme(cx).danger).child(err.clone()));
    }

    let entity_cancel = entity.clone();
    let entity_save = entity.clone();
    panel.child(
        h_flex()
            .gap_2()
            .child(
                Button::new("save-speaker-edit")
                    .primary()
                    .label(if state.saving { "Saving…" } else if merging { "Merge" } else { "Save" })
                    .loading(state.saving)
                    .disabled(!can_save)
                    .on_click(move |_, _, cx| {
                        entity_save.update(cx, |this, cx| this.save_speaker_edit(cx));
                    }),
            )
            .child(
                Button::new("cancel-speaker-edit")
                    .ghost()
                    .label("Cancel")
                    .disabled(state.saving)
                    .on_click(move |_, _, cx| {
                        entity_cancel.update(cx, |this, cx| this.close_speaker_edit(cx));
                    }),
            ),
    )
    .into_any_element()
}

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
        mono: gpui_kit::font("monospace"),
        property_icon: None,
    }
}

impl Render for MeetingView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(_id) = self.meeting_id.clone() else {
            return v_flex().size_full().p_6().child("Select a meeting from the sidebar.");
        };

        v_flex()
            .size_full()
            .child(self.render_header(cx))
            .child(self.render_calendar_row(cx))
            .when(self.calendar_picker_open, |this| this.child(self.render_calendar_picker(cx)))
            .when(self.retranscribe_open, |this| this.child(self.render_retranscribe(cx)))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .border_r_1()
                            .border_color(cx.theme().border)
                            .child(self.render_transcript(cx)),
                    )
                    .child(div().flex_1().min_w_0().h_full().child(self.render_summary(cx))),
            )
            .when_some(self.speaker_edit.clone(), |this, state| {
                let entity = cx.entity();
                let content = render_floating_speaker_edit(&state, entity, cx);
                this.child(
                    deferred(anchored().position(state.click_position).snap_to_window().child(content))
                        .with_priority(50),
                )
            })
    }
}

impl MeetingView {
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let date = self
            .created_at
            .map(format::format_header_date)
            .unwrap_or_default();
        let can_retranscribe = self.meeting_folder_path.is_some();

        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .gap_3()
            .p_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(if self.editing_title {
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().min_w_0().child(Input::new(&self.title_input)))
                    .child(
                        Button::new("save-title")
                            .primary()
                            .label("Save")
                            .on_click(cx.listener(|this, _, _, cx| this.save_title(cx))),
                    )
                    .child(
                        Button::new("cancel-title")
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.editing_title = false;
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            } else {
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .text_lg()
                                    .font_semibold()
                                    .overflow_hidden()
                                    .child(if self.title.trim().is_empty() {
                                        "Untitled meeting".to_string()
                                    } else {
                                        self.title.clone()
                                    }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(date),
                            ),
                    )
                    .child(
                        Button::new("edit-title")
                            .ghost()
                            .label("Rename")
                            .on_click(cx.listener(|this, _, window, cx| {
                                let title = this.title.clone();
                                this.title_input.update(cx, |state, cx| state.set_value(title, window, cx));
                                this.editing_title = true;
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("retranscribe")
                            .ghost()
                            .icon(Lucide::RefreshCw)
                            .tooltip("Retranscribe audio")
                            .disabled(!can_retranscribe || self.retranscribe_in_progress)
                            .on_click(cx.listener(|this, _, window, cx| this.open_retranscribe(window, cx))),
                    )
                    .child(
                        Button::new("copy-summary-md")
                            .ghost()
                            .icon(IconName::Copy)
                            .tooltip("Copy meeting as Markdown")
                            .on_click(cx.listener(|this, _, _, cx| this.export_copy("markdown", cx))),
                    )
                    .child(
                        Button::new("copy-summary-json")
                            .ghost()
                            .icon(Lucide::Braces)
                            .tooltip("Copy meeting as JSON")
                            .on_click(cx.listener(|this, _, _, cx| this.export_copy("json", cx))),
                    )
                    .child(
                        Button::new("save-summary-md")
                            .ghost()
                            .icon(Lucide::Download)
                            .tooltip("Save meeting as Markdown…")
                            .on_click(cx.listener(|this, _, _, cx| this.export_save("markdown", cx))),
                    )
                    .child(
                        Button::new("save-summary-json")
                            .ghost()
                            .icon(Lucide::FileCode)
                            .tooltip("Save meeting as JSON…")
                            .on_click(cx.listener(|this, _, _, cx| this.export_save("json", cx))),
                    )
                    .child(
                        Button::new("delete-meeting")
                            .danger()
                            .icon(IconName::Delete)
                            .tooltip("Delete meeting")
                            .on_click(cx.listener(|this, _, window, cx| this.confirm_delete(window, cx))),
                    ),
            )
    }

    /// The calendar-event link row shown under the header — mirrors
    /// `CalendarEventPanel.tsx`.
    fn render_calendar_row(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.calendar_loading {
            return div().into_any_element();
        }
        let row = h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .gap_3()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border);

        let Some(event) = self.calendar_event.clone() else {
            return row
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_color(cx.theme().muted_foreground)
                        .text_sm()
                        .child(Lucide::Calendar.view(cx))
                        .child("Not linked to a calendar event."),
                )
                .child(
                    Button::new("link-calendar-event")
                        .ghost()
                        .icon(Lucide::Link2)
                        .label("Link event")
                        .disabled(self.calendar_mutating)
                        .on_click(cx.listener(|this, _, _, cx| this.open_calendar_picker(cx))),
                )
                .into_any_element();
        };

        let attendee_count = event.attendees.len();
        row.child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Lucide::Calendar.view(cx))
                        .child(
                            div()
                                .text_sm()
                                .font_medium()
                                .child(event.summary.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "(untitled event)".to_string())),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(calendar_format::format_time_range(event.start_at, event.end_at)),
                )
                .when_some(event.location.clone().filter(|l| !l.is_empty()), |el, loc| {
                    el.child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(Lucide::MapPin.view(cx))
                            .child(loc),
                    )
                })
                .when(attendee_count > 0, |el| {
                    el.child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(Lucide::Users.view(cx))
                            .child(format!("{attendee_count} attendee{}", if attendee_count == 1 { "" } else { "s" })),
                    )
                }),
        )
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Button::new("change-calendar-event")
                        .ghost()
                        .label("Change")
                        .disabled(self.calendar_mutating)
                        .on_click(cx.listener(|this, _, _, cx| this.open_calendar_picker(cx))),
                )
                .child(
                    Button::new("unlink-calendar-event")
                        .ghost()
                        .icon(Lucide::Link2Off)
                        .label("Unlink")
                        .disabled(self.calendar_mutating)
                        .on_click(cx.listener(|this, _, _, cx| this.unlink_calendar_event(cx))),
                ),
        )
        .into_any_element()
    }

    /// "Link calendar event" picker: events within `±CALENDAR_WINDOW_DAYS`
    /// of the meeting's `created_at`, closest-first — mirrors
    /// `CalendarEventPicker.tsx`.
    fn render_calendar_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let anchor = self.created_at.unwrap_or_else(chrono::Utc::now);
        let current_id = self.calendar_event.as_ref().map(|e| e.id.clone());

        let mut body = v_flex()
            .w_full()
            .gap_2()
            .p_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().muted.opacity(0.3))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(div().text_sm().font_semibold().child("Link calendar event"))
                    .child(
                        Button::new("close-calendar-picker")
                            .ghost()
                            .icon(Lucide::X)
                            .on_click(cx.listener(|this, _, _, cx| this.close_calendar_picker(cx))),
                    ),
            );

        if let Some(err) = &self.calendar_picker_error {
            body = body.child(div().text_xs().text_color(cx.theme().danger).child(err.clone()));
        }

        if self.calendar_picker_events.is_some() {
            body = body.child(div().w_full().child(Input::new(&self.calendar_picker_query)));
        }

        let query = self.calendar_picker_query.read(cx).value().trim().to_lowercase();
        let filtered: Option<Vec<CalendarEvent>> = self.calendar_picker_events.as_ref().map(|events| {
            if query.is_empty() {
                events.clone()
            } else {
                events
                    .iter()
                    .filter(|e| {
                        e.summary.as_deref().unwrap_or_default().to_lowercase().contains(&query)
                            || e.location.as_deref().unwrap_or_default().to_lowercase().contains(&query)
                    })
                    .cloned()
                    .collect()
            }
        });

        match &filtered {
            None => {
                body = body.child(div().text_xs().text_color(cx.theme().muted_foreground).child("Loading…"));
            }
            Some(events) if events.is_empty() => {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "No events within ±{CALENDAR_WINDOW_DAYS} days of the recording. Try refreshing your \
                             calendar in Settings."
                        )),
                );
            }
            Some(events) => {
                body = body.child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .max_h(px(280.))
                        .overflow_y_scrollbar()
                        .children(events.iter().take(30).map(|e| {
                            let is_current = current_id.as_deref() == Some(e.id.as_str());
                            let event_id = e.id.clone();
                            h_flex()
                                .id(SharedString::from(format!("calendar-event-{}", e.id)))
                                .w_full()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .when(is_current, |el| el.bg(cx.theme().accent.opacity(0.4)))
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_0p5()
                                        .child(
                                            div()
                                                .text_sm()
                                                .overflow_hidden()
                                                .child(e.summary.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "(untitled event)".to_string())),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(format!(
                                                    "{} · {}",
                                                    e.start_at.with_timezone(&chrono::Local).format("%a %b %-d, %-I:%M %p"),
                                                    calendar_format::format_offset(e.start_at, anchor),
                                                )),
                                        ),
                                )
                                .child(
                                    Button::new(SharedString::from(format!("pick-calendar-event-{}", e.id)))
                                        .when(is_current, |b| b.outline())
                                        .when(!is_current, |b| b.ghost())
                                        .label(if is_current { "Linked" } else { "Link" })
                                        .disabled(self.calendar_mutating || is_current)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.pick_calendar_event(Some(event_id.clone()), cx)
                                        })),
                                )
                                .into_any_element()
                        })),
                );
            }
        }

        body
    }

    fn render_retranscribe(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let model_label = self
            .retranscribe_model_index
            .and_then(|i| self.retranscribe_models.get(i))
            .cloned()
            .unwrap_or_else(|| "Default".to_string());

        v_flex()
            .w_full()
            .gap_2()
            .p_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().muted.opacity(0.3))
            .child(div().text_sm().font_semibold().child("Retranscribe meeting"))
            .when(!self.retranscribe_in_progress && self.retranscribe_error.is_none(), |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().text_xs().text_color(cx.theme().muted_foreground).w_20().child("Language"))
                        .child(div().w_32().child(Input::new(&self.retranscribe_language)))
                        .child(div().text_xs().text_color(cx.theme().muted_foreground).w_20().child("Model"))
                        .child(
                            Button::new("cycle-model")
                                .outline()
                                .label(model_label)
                                .disabled(self.retranscribe_models.is_empty())
                                .on_click(cx.listener(|this, _, _, cx| this.cycle_retranscribe_model(cx))),
                        )
                        .child(
                            Button::new("start-retranscribe")
                                .primary()
                                .label("Start")
                                .on_click(cx.listener(|this, _, window, cx| this.start_retranscribe(window, cx))),
                        )
                        .child(
                            Button::new("cancel-retranscribe-dialog")
                                .ghost()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| this.close_retranscribe(cx))),
                        ),
                )
            })
            .when(self.retranscribe_in_progress, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_xs()
                                .child(format!("{}% — {}", self.retranscribe_progress_pct, self.retranscribe_progress_message)),
                        )
                        .child(
                            Button::new("cancel-retranscribe")
                                .ghost()
                                .icon(Lucide::X)
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, window, cx| this.cancel_retranscribe(window, cx))),
                        ),
                )
            })
            .when_some(self.retranscribe_error.clone(), |this, err| {
                this.child(div().text_xs().text_color(cx.theme().danger).child(err)).child(
                    Button::new("close-retranscribe-error")
                        .ghost()
                        .label("Close")
                        .on_click(cx.listener(|this, _, _, cx| this.close_retranscribe(cx))),
                )
            })
    }

    fn render_transcript(&self, cx: &mut Context<Self>) -> AnyElement {
        let header = h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .gap_2()
            .p_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().text_sm().font_semibold().child("Transcript"))
            .child(
                Button::new("copy-transcript")
                    .ghost()
                    .icon(IconName::Copy)
                    .tooltip("Copy transcript")
                    .disabled(self.transcripts.is_empty())
                    .on_click(cx.listener(|this, _, window, cx| this.copy_transcript(window, cx))),
            );

        if self.transcripts_loading {
            return v_flex()
                .size_full()
                .child(header)
                .child(div().p_4().child("Loading transcript…"))
                .into_any_element();
        }
        if let Some(err) = &self.load_error {
            return v_flex()
                .size_full()
                .child(header)
                .child(div().p_4().text_color(cx.theme().danger).child(err.clone()))
                .into_any_element();
        }
        if self.transcripts.is_empty() {
            return v_flex()
                .size_full()
                .child(header)
                .child(
                    div()
                        .p_4()
                        .text_color(cx.theme().muted_foreground)
                        .child("No transcript yet."),
                )
                .into_any_element();
        }

        let transcripts = self.transcripts.clone();
        let count = transcripts.len();
        let entity = cx.entity();
        let meeting_id = self.meeting_id.clone();
        let speaker_edit = self.speaker_edit.clone();
        let playing_segment = self.playing_segment.clone();
        let loading_segment = self.loading_segment.clone();
        let show_confidence = self.show_confidence;

        v_flex()
            .size_full()
            .child(header)
            .child(
                uniform_list("meeting-transcript", count, move |range, _window, cx| {
                    range
                        .map(|ix| {
                            let t = &transcripts[ix];
                            let time = format::segment_timestamp(t.audio_start_time, &t.timestamp);
                            let speaker = t.speaker.clone().unwrap_or_else(|| "Speaker".to_string());
                            let editable = speaker_chip::can_edit_speaker(&speaker, t.voice_profile_id.as_deref());
                            let row_key = format!("{}:{speaker}", t.id);
                            let is_editing_this_row = speaker_edit.as_ref().is_some_and(|s| s.key == row_key);

                            let can_play = meeting_id.is_some()
                                && t.audio_end_time.is_some_and(|end| end > t.audio_start_time.unwrap_or(0.0));
                            let is_playing = playing_segment.as_deref() == Some(t.id.as_str());
                            let is_loading_clip = loading_segment.as_deref() == Some(t.id.as_str());

                            let mut header_row = h_flex().gap_2().items_center().text_xs().text_color(cx.theme().muted_foreground);

                            if can_play {
                                let segment_id = t.id.clone();
                                let start = t.audio_start_time.unwrap_or(0.0);
                                let end = t.audio_end_time.unwrap_or(start);
                                let source = t.source.clone();
                                let entity = entity.clone();
                                header_row = header_row.child(
                                    Button::new(SharedString::from(format!("play-segment-{}", t.id)))
                                        .ghost()
                                        .xsmall()
                                        .icon(if is_playing { Lucide::Square } else { Lucide::Play })
                                        .loading(is_loading_clip)
                                        .tooltip(if is_playing { "Stop" } else { "Play this segment" })
                                        .on_click(move |_, _, cx| {
                                            entity.update(cx, |this, cx| {
                                                this.toggle_segment_playback(
                                                    segment_id.clone(),
                                                    start,
                                                    end,
                                                    source.clone(),
                                                    cx,
                                                )
                                            });
                                        }),
                                );
                            }

                            if editable {
                                let segment_id = t.id.clone();
                                let speaker_for_click = speaker.clone();
                                let voice_profile_id = t.voice_profile_id.clone();
                                let entity = entity.clone();
                                header_row = header_row.child(
                                    Button::new(SharedString::from(format!("edit-speaker-{row_key}")))
                                        .ghost()
                                        .xsmall()
                                        .label(speaker.clone())
                                        .on_click(move |ev, window, cx| {
                                            let click_position = ev.position();
                                            entity.update(cx, |this, cx| {
                                                this.toggle_speaker_edit(
                                                    segment_id.clone(),
                                                    speaker_for_click.clone(),
                                                    voice_profile_id.clone(),
                                                    click_position,
                                                    window,
                                                    cx,
                                                )
                                            });
                                        }),
                                );
                            } else {
                                header_row = header_row.child(speaker.clone());
                            }
                            header_row = header_row.child(time);

                            if show_confidence {
                                if let Some(conf) = t.confidence {
                                    let color = match format::ConfidenceLevel::for_confidence(conf) {
                                        format::ConfidenceLevel::High => cx.theme().success,
                                        format::ConfidenceLevel::Good | format::ConfidenceLevel::Medium => {
                                            cx.theme().warning
                                        }
                                        format::ConfidenceLevel::Low => cx.theme().danger,
                                    };
                                    header_row = header_row.child(
                                        Button::new(SharedString::from(format!("confidence-{}", t.id)))
                                            .ghost()
                                            .xsmall()
                                            .tooltip(format::confidence_tooltip(conf))
                                            .child(div().size(px(8.)).rounded_full().bg(color)),
                                    );
                                }
                            }

                            let row = v_flex()
                                .w_full()
                                .gap_1()
                                .px_4()
                                .py_2()
                                .when(is_editing_this_row, |this| this.bg(cx.theme().muted.opacity(0.3)))
                                .child(header_row)
                                .child(div().text_sm().child(t.text.clone()));

                            row.into_any_element()
                        })
                        .collect::<Vec<_>>()
                })
                .flex_1()
                .size_full(),
            )
            .into_any_element()
    }

    fn render_summary(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (status_label, button_label, button_disabled) = match &self.summary_phase {
            SummaryPhase::Idle => (None, "Generate summary", false),
            SummaryPhase::Loading => (Some("Loading summary…".to_string()), "Generate summary", true),
            SummaryPhase::Generating => (Some("Generating…".to_string()), "Generating…", true),
            SummaryPhase::Error(e) => (Some(e.clone()), "Regenerate summary", false),
        };
        let has_summary = self.summary_has_content;
        let template_label = self
            .templates
            .iter()
            .find(|(id, _, _)| *id == self.selected_template)
            .map(|(_, name, _)| name.clone())
            .unwrap_or_else(|| "Template".to_string());
        let template_tooltip = self
            .templates
            .iter()
            .find(|(id, _, _)| *id == self.selected_template)
            .map(|(_, _, desc)| desc.clone())
            .unwrap_or_default();

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .p_4()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().font_semibold().child("Summary"))
                            .when_some(status_label, |this, label| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(label),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .when(!self.editing_summary, |this| {
                                this.child(
                                    Button::new("copy-summary")
                                        .ghost()
                                        .icon(IconName::Copy)
                                        .tooltip("Copy summary")
                                        .disabled(!has_summary)
                                        .on_click(cx.listener(|this, _, window, cx| this.copy_summary(window, cx))),
                                )
                                .child(
                                    Button::new("edit-summary")
                                        .ghost()
                                        .icon(Lucide::Pencil)
                                        .tooltip("Edit summary")
                                        .disabled(!has_summary || matches!(self.summary_phase, SummaryPhase::Generating))
                                        .on_click(cx.listener(|this, _, _, cx| this.start_edit_summary(cx))),
                                )
                                .child(
                                    Button::new("cycle-summary-template")
                                        .outline()
                                        .icon(Lucide::FileText)
                                        .label(template_label)
                                        .tooltip(if template_tooltip.is_empty() {
                                            "Summary template".to_string()
                                        } else {
                                            template_tooltip
                                        })
                                        .disabled(self.templates.is_empty() || matches!(self.summary_phase, SummaryPhase::Generating))
                                        .on_click(cx.listener(|this, _, _, cx| this.cycle_template(cx))),
                                )
                                .child(
                                    Button::new("toggle-custom-prompt")
                                        .ghost()
                                        .icon(Lucide::MessageSquare)
                                        .tooltip("Custom instructions")
                                        .disabled(matches!(self.summary_phase, SummaryPhase::Generating))
                                        .on_click(cx.listener(|this, _, _, cx| this.toggle_custom_prompt(cx))),
                                )
                                .child(
                                    Button::new("generate-summary")
                                        .primary()
                                        .label(if has_summary && button_label == "Generate summary" {
                                            "Regenerate summary"
                                        } else {
                                            button_label
                                        })
                                        .loading(matches!(self.summary_phase, SummaryPhase::Generating))
                                        .disabled(button_disabled)
                                        .on_click(cx.listener(|this, _, window, cx| this.generate_summary(window, cx))),
                                )
                            })
                            .when(self.editing_summary, |this| {
                                this.child(
                                    Button::new("cancel-edit-summary")
                                        .ghost()
                                        .label("Cancel")
                                        .disabled(self.saving_summary)
                                        .on_click(cx.listener(|this, _, _, cx| this.cancel_edit_summary(cx))),
                                )
                                .child(
                                    Button::new("save-summary")
                                        .primary()
                                        .icon(Lucide::Save)
                                        .label(if self.saving_summary { "Saving…" } else { "Save" })
                                        .loading(self.saving_summary)
                                        .disabled(self.saving_summary)
                                        .on_click(cx.listener(|this, _, window, cx| this.save_summary(window, cx))),
                                )
                            }),
                    ),
            )
            .when(self.show_custom_prompt && !self.editing_summary, |this| {
                this.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_center()
                        .px_4()
                        .py_2()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(div().flex_1().min_w_0().child(Input::new(&self.custom_prompt)))
                        .into_any_element(),
                )
            })
            .child(
                div()
                    .id("summary-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .child(
                        v_flex()
                            .w_full()
                            .gap_4()
                            .when(!self.editing_summary, |this| {
                                this.child(TextView::new(&self.summary_state).selectable(true))
                            })
                            .when(self.editing_summary, |this| this.child(self.summary_editor.clone()))
                            .child(self.render_action_items_panel(cx))
                            .child(self.render_notes_panel(cx)),
                    ),
            )
    }

    // ---- Per-meeting action items panel -------------------------------------
    // Mirrors `ActionItemsPanel.tsx`'s card layout: header (icon, title, open
    // count / "All done"), an "Extract from summary"/"Re-extract" button
    // (only once there's a summary), the item list, then an add row.

    fn render_action_items_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let open_count = self.action_items.iter().filter(|i| i.status != STATUS_DONE).count();
        let is_empty = self.action_items.is_empty();
        let has_summary = self.summary_has_content;

        let mut card = v_flex()
            .w_full()
            .gap_2()
            .p_3()
            .rounded_md()
            .bg(cx.theme().muted.opacity(0.3))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Lucide::ListChecks.view(cx))
                            .child(div().text_sm().font_semibold().child("Action Items"))
                            .when(open_count > 0, |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .px_2()
                                        .rounded_full()
                                        .bg(cx.theme().primary.opacity(0.15))
                                        .text_color(cx.theme().primary)
                                        .child(open_count.to_string()),
                                )
                            })
                            .when(!is_empty && open_count == 0, |this| {
                                this.child(
                                    div().text_xs().text_color(cx.theme().muted_foreground).child("All done"),
                                )
                            }),
                    )
                    .when(has_summary, |this| {
                        let label = if is_empty { "Extract from summary" } else { "Re-extract" };
                        this.child(
                            Button::new("extract-action-items")
                                .ghost()
                                .xsmall()
                                .icon(Lucide::Sparkles)
                                .label(label)
                                .loading(self.ai_extracting)
                                .disabled(self.ai_extracting)
                                .on_click(cx.listener(|this, _, _, cx| this.extract_action_items(cx))),
                        )
                    }),
            );

        if is_empty {
            let empty_text = if has_summary {
                "No action items yet. Extract them from the summary, or add one below."
            } else {
                "No action items yet. Add one below."
            };
            card = card.child(div().text_xs().text_color(cx.theme().muted_foreground).child(empty_text));
        } else {
            for item in &self.action_items {
                card = card.child(self.render_action_item_row(item, cx));
            }
        }

        card = card.child(self.render_action_item_composer(cx));
        card.into_any_element()
    }

    fn render_action_item_row(&self, item: &ActionItem, cx: &mut Context<Self>) -> AnyElement {
        let is_done = item.status == STATUS_DONE;
        let id = item.id.clone();

        if self.ai_editing_id.as_deref() == Some(item.id.as_str()) {
            return h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .child(div().flex_1().min_w_0().child(Input::new(&self.ai_edit_input)))
                .child(
                    Button::new(SharedString::from(format!("save-ai-{id}")))
                        .primary()
                        .xsmall()
                        .label("Save")
                        .on_click(cx.listener(|this, _, _, cx| this.save_action_item_edit(cx))),
                )
                .child(
                    Button::new(SharedString::from(format!("cancel-ai-{id}")))
                        .ghost()
                        .xsmall()
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_action_item_edit(cx))),
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
            .child(
                Checkbox::new(SharedString::from(format!("ai-done-{id}"))).checked(is_done).on_click(cx.listener(
                    move |this, _, _, cx| this.toggle_action_item_status(id_for_toggle.clone(), is_done, cx),
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
                            .when(is_done, |this| this.line_through().text_color(cx.theme().muted_foreground))
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
                Button::new(SharedString::from(format!("edit-ai-{id}")))
                    .ghost()
                    .xsmall()
                    .icon(Lucide::Pencil)
                    .tooltip("Edit")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_action_item_edit(id_for_edit.clone(), window, cx);
                    })),
            )
            .child(
                Button::new(SharedString::from(format!("delete-ai-{id}")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Delete)
                    .tooltip("Delete")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.delete_action_item(id_for_delete.clone(), cx);
                    })),
            )
            .into_any_element()
    }

    fn render_action_item_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.ai_adding {
            return h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .child(div().flex_1().min_w_0().child(Input::new(&self.ai_add_input)))
                .child(
                    Button::new("save-add-ai")
                        .primary()
                        .xsmall()
                        .label("Add")
                        .on_click(cx.listener(|this, _, _, cx| this.save_new_action_item(cx))),
                )
                .child(
                    Button::new("cancel-add-ai")
                        .ghost()
                        .xsmall()
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_action_item_composer(cx))),
                )
                .into_any_element();
        }

        Button::new("open-add-ai")
            .ghost()
            .xsmall()
            .icon(IconName::Plus)
            .label("Add item")
            .on_click(cx.listener(|this, _, window, cx| this.open_action_item_composer(window, cx)))
            .into_any_element()
    }

    // ---- Meeting notes panel -------------------------------------------------
    // Mirrors `MeetingNotesPanel.tsx`'s card layout: header (icon, title,
    // count badge, "Add note" button), a list of notes, no empty-state text.

    fn render_notes_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.notes.len();

        let mut card = v_flex()
            .w_full()
            .gap_2()
            .p_3()
            .rounded_md()
            .bg(cx.theme().muted.opacity(0.3))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Lucide::NotebookPen.view(cx))
                            .child(div().text_sm().font_semibold().child("Notes"))
                            .when(count > 0, |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .px_2()
                                        .rounded_full()
                                        .bg(cx.theme().primary.opacity(0.15))
                                        .text_color(cx.theme().primary)
                                        .child(count.to_string()),
                                )
                            }),
                    )
                    .when(!self.notes_composing, |this| {
                        this.child(
                            Button::new("open-note-composer")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Plus)
                                .label("Add note")
                                .on_click(cx.listener(|this, _, window, cx| this.open_note_composer(window, cx))),
                        )
                    }),
            );

        for note in &self.notes {
            let id = note.id.clone();
            let time = format::format_note_time(&note.created_at);
            let suffix = if note.source == "agent" { " · added by an agent" } else { "" };
            card = card.child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(div().text_sm().child(note.body.clone()))
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("{time}{suffix}")),
                            )
                            .child(
                                Button::new(SharedString::from(format!("delete-note-{id}")))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Delete)
                                    .tooltip("Delete note")
                                    .on_click(cx.listener(move |this, _, _, cx| this.delete_note(id.clone(), cx))),
                            ),
                    ),
            );
        }

        if self.notes_composing {
            card = card.child(
                v_flex()
                    .w_full()
                    .gap_2()
                    .child(div().w_full().child(Input::new(&self.notes_draft)))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("save-note")
                                    .primary()
                                    .xsmall()
                                    .label(if self.notes_saving { "Saving…" } else { "Save" })
                                    .loading(self.notes_saving)
                                    .disabled(self.notes_saving)
                                    .on_click(cx.listener(|this, _, window, cx| this.save_note(window, cx))),
                            )
                            .child(
                                Button::new("cancel-note")
                                    .ghost()
                                    .xsmall()
                                    .label("Cancel")
                                    .disabled(self.notes_saving)
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_note_composer(cx))),
                            ),
                    ),
            );
        }

        card.into_any_element()
    }
}
