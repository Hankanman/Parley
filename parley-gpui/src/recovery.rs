//! Interrupted-meeting recovery: on startup, once the database is ready,
//! offer to recover any meeting an unclean shutdown left `"recording"`
//! (marked `"interrupted"` by the startup sweep in
//! `database::setup::prepare_database_on_startup`, or by a fatal recording
//! error). Mirrors `frontend/src/components/TranscriptRecovery/**` +
//! `frontend/src/hooks/useTranscriptRecovery.ts` + the wiring in
//! `frontend/src/app/page.tsx`, calling the same Tauri-free core functions
//! the Tauri `list_interrupted_meetings` / `recover_meeting` commands
//! (`commands/audio/recovery_commands.rs`) wrap directly.

use gpui_kit::component::{
    ActiveTheme, Disableable as _, StyledExt as _, WindowExt as _, h_flex, v_flex,
    button::{Button, ButtonVariants as _},
    dialog::Dialog,
    label::Label,
};
use gpui_kit::*;

use parley_core::audio::incremental_saver::{
    cleanup_checkpoints, has_audio_checkpoints, recover_audio_from_checkpoints,
};
use parley_core::database::models::MeetingTranscript;
use parley_core::database::repositories::meeting::{InterruptedMeetingRow, MeetingsRepository};

use crate::app_state::AppServices;
use crate::shell;

/// Fixed max height of the scrollable, virtualized transcript preview list.
const PREVIEW_LIST_HEIGHT: Pixels = px(320.);

/// Entry point: call once the shell is showing (normal launch with an
/// already-completed onboarding, or right after onboarding finishes). A
/// no-op if there's no database yet or nothing is interrupted.
pub fn check_on_startup(window: &mut Window, cx: &mut App) {
    let Some(pool) = AppServices::global(cx).pool() else {
        return;
    };
    let io = AppServices::global(cx).io.clone();
    let window_handle = window.window_handle();

    cx.spawn(async move |cx| {
        let rows = io
            .spawn(async move { MeetingsRepository::list_interrupted_meetings(&pool).await })
            .await;
        let rows: Vec<InterruptedMeetingRow> = match rows {
            Ok(Ok(rows)) if !rows.is_empty() => rows,
            Ok(Ok(_)) => return,
            Ok(Err(e)) => {
                log::warn!("Failed to list interrupted meetings: {}", e);
                return;
            }
            Err(e) => {
                log::warn!("Interrupted-meetings lookup task panicked: {}", e);
                return;
            }
        };

        let _ = cx.update_window(window_handle, |_, window, cx| {
            open_dialog(rows, window, cx);
        });
    })
    .detach();
}

struct RecoveryState {
    rows: Vec<InterruptedMeetingRow>,
    busy: Option<String>,
    error: Option<String>,
    /// Meeting id whose transcript preview is currently expanded, if any —
    /// mirrors `TranscriptRecovery.tsx`'s "select a meeting to preview" list.
    preview_id: Option<String>,
    preview_loading: bool,
    preview_transcripts: Vec<MeetingTranscript>,
    preview_error: Option<String>,
}

impl RecoveryState {
    fn new(rows: Vec<InterruptedMeetingRow>, _window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            rows,
            busy: None,
            error: None,
            preview_id: None,
            preview_loading: false,
            preview_transcripts: Vec::new(),
            preview_error: None,
        }
    }

    /// Toggle the transcript preview for one interrupted meeting. Loads via
    /// `MeetingsRepository::get_meeting` — the same read a saved meeting's
    /// transcripts come from, since an interrupted meeting's segments were
    /// already upserted live to SQLite (see `audio::transcript_db_writer`).
    /// Loads the full transcript (not just a slice) — the preview list
    /// itself is virtualized (`uniform_list`) so a long transcript doesn't
    /// cost anything to render.
    fn toggle_preview(&mut self, meeting_id: String, cx: &mut Context<Self>) {
        if self.preview_id.as_deref() == Some(meeting_id.as_str()) {
            self.preview_id = None;
            self.preview_transcripts.clear();
            self.preview_error = None;
            cx.notify();
            return;
        }

        self.preview_id = Some(meeting_id.clone());
        self.preview_loading = true;
        self.preview_transcripts.clear();
        self.preview_error = None;
        cx.notify();

        let Some(pool) = AppServices::global(cx).pool() else {
            self.preview_loading = false;
            self.preview_error = Some("Database not ready".to_string());
            cx.notify();
            return;
        };
        let io = AppServices::global(cx).io.clone();
        let id_for_task = meeting_id.clone();

        cx.spawn(async move |this, cx| {
            let result = io.spawn(async move { MeetingsRepository::get_meeting(&pool, &id_for_task).await }).await;
            let _ = this.update(cx, |this, cx| {
                if this.preview_id.as_deref() != Some(meeting_id.as_str()) {
                    return;
                }
                this.preview_loading = false;
                match result {
                    Ok(Ok(Some(mut details))) => {
                        details.transcripts.sort_by(|a, b| {
                            a.audio_start_time
                                .partial_cmp(&b.audio_start_time)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        });
                        this.preview_transcripts = details.transcripts;
                    }
                    Ok(Ok(None)) => this.preview_error = Some("Meeting not found".to_string()),
                    Ok(Err(e)) => this.preview_error = Some(e.to_string()),
                    Err(e) => this.preview_error = Some(format!("Preview task panicked: {}", e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Mirrors the Tauri `recover_meeting` command: merge any leftover
    /// `.checkpoints/` audio into `audio.mp4` (best-effort — transcripts
    /// recover regardless), then mark the row `"completed"`.
    fn recover(&mut self, meeting_id: String, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(meeting_id.clone());
        self.error = None;
        cx.notify();

        let Some(pool) = AppServices::global(cx).pool() else {
            self.busy = None;
            self.error = Some("Database not ready".to_string());
            cx.notify();
            return;
        };
        let io = AppServices::global(cx).io.clone();
        let id_for_task = meeting_id.clone();

        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move {
                    let meeting = MeetingsRepository::get_meeting_metadata(&pool, &id_for_task)
                        .await
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| "Meeting not found".to_string())?;

                    let mut duration_seconds = None;
                    let mut audio_path = None;
                    if let Some(folder) = meeting.folder_path.clone() {
                        if has_audio_checkpoints(folder.clone()).await.unwrap_or(false) {
                            if let Ok(status) =
                                recover_audio_from_checkpoints(folder.clone(), 48000).await
                            {
                                if status.status == "success" {
                                    let _ = cleanup_checkpoints(folder.clone()).await;
                                }
                                duration_seconds = Some(status.estimated_duration_seconds);
                                audio_path = status.audio_file_path;
                            }
                        }
                    }

                    MeetingsRepository::mark_meeting_completed(
                        &pool,
                        &id_for_task,
                        duration_seconds,
                        audio_path.as_deref(),
                    )
                    .await
                    .map_err(|e| e.to_string())
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(Ok(_)) => {
                        this.rows.retain(|r| r.meeting_id != meeting_id);
                        shell::refresh_meetings(cx);
                        log::info!("Recovered interrupted meeting {}", meeting_id);
                    }
                    Ok(Err(e)) => this.error = Some(e),
                    Err(e) => this.error = Some(format!("Recovery task panicked: {}", e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Discard an interrupted meeting outright — mirrors
    /// `deleteRecoverableMeeting` (`api_delete_meeting`).
    fn delete(&mut self, meeting_id: String, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(meeting_id.clone());
        self.error = None;
        cx.notify();

        let Some(pool) = AppServices::global(cx).pool() else {
            self.busy = None;
            self.error = Some("Database not ready".to_string());
            cx.notify();
            return;
        };
        let io = AppServices::global(cx).io.clone();
        let id_for_task = meeting_id.clone();

        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move { MeetingsRepository::delete_meeting(&pool, &id_for_task).await })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(Ok(true)) => {
                        this.rows.retain(|r| r.meeting_id != meeting_id);
                        shell::refresh_meetings(cx);
                    }
                    Ok(Ok(false)) => this.error = Some("Meeting not found".to_string()),
                    Ok(Err(e)) => this.error = Some(e.to_string()),
                    Err(e) => this.error = Some(format!("Delete task panicked: {}", e)),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// "12:34" total captured duration, spanning the earliest segment's start to
/// the latest segment's end — `None` if no segment carries audio-relative
/// timing (e.g. an import predating that column).
fn preview_duration_label(transcripts: &[MeetingTranscript]) -> Option<String> {
    let start = transcripts.iter().filter_map(|t| t.audio_start_time).fold(f64::INFINITY, f64::min);
    let end = transcripts.iter().filter_map(|t| t.audio_end_time.or(t.audio_start_time)).fold(f64::NEG_INFINITY, f64::max);
    if !start.is_finite() || !end.is_finite() || end < start {
        return None;
    }
    let total = (end - start).max(0.0) as u64;
    Some(format!("{:02}:{:02}", total / 60, total % 60))
}

#[cfg(test)]
mod tests {
    // Deliberately not `use super::*` — that would also pull in this
    // module's `use gpui_kit::*` glob (re-exporting the entirety of GPUI),
    // and combining that huge glob-imported namespace with a struct literal
    // here blows up rustc's macro/trait-resolution recursion limit (verified
    // by bisection: this exact test module reproduces it, an equivalent one
    // with named imports does not). Import only what the tests need instead.
    use super::preview_duration_label;
    use parley_core::database::models::MeetingTranscript;

    fn transcript(start: Option<f64>, end: Option<f64>) -> MeetingTranscript {
        MeetingTranscript {
            id: "t".into(),
            text: "hello".into(),
            timestamp: String::new(),
            audio_start_time: start,
            audio_end_time: end,
            duration: None,
            speaker: None,
            voice_profile_id: None,
            source: None,
            confidence: None,
        }
    }

    #[test]
    fn duration_spans_earliest_start_to_latest_end() {
        let transcripts = vec![transcript(Some(5.0), Some(10.0)), transcript(Some(60.0), Some(94.0))];
        assert_eq!(preview_duration_label(&transcripts), Some("01:29".to_string()));
    }

    #[test]
    fn duration_falls_back_to_start_when_end_missing() {
        let transcripts = vec![transcript(Some(0.0), None), transcript(Some(30.0), None)];
        assert_eq!(preview_duration_label(&transcripts), Some("00:30".to_string()));
    }

    #[test]
    fn duration_is_none_without_any_audio_timing() {
        let transcripts = vec![transcript(None, None)];
        assert_eq!(preview_duration_label(&transcripts), None);
    }

    #[test]
    fn duration_is_none_for_empty_transcript_list() {
        assert_eq!(preview_duration_label(&[]), None);
    }
}

fn open_dialog(rows: Vec<InterruptedMeetingRow>, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    let state = cx.new(|cx| RecoveryState::new(rows, window, cx));
    window.open_dialog(cx, move |dialog, window, cx| {
        render_dialog(dialog, state.clone(), window, cx)
    });
}

fn render_dialog(
    dialog: Dialog,
    state: Entity<RecoveryState>,
    _window: &mut Window,
    cx: &mut App,
) -> Dialog {
    let n = state.read(cx).rows.len();
    let title = if n == 1 {
        "1 interrupted meeting found".to_string()
    } else {
        format!("{} interrupted meetings found", n)
    };

    dialog
        .title(Label::new(title).text_lg().font_semibold())
        .width(px(520.))
        .overlay_closable(true)
        .keyboard(true)
        .content(move |content, window, cx| render_body(content, &state, window, cx))
}

fn render_body<'a>(
    content: gpui_kit::component::dialog::DialogContent,
    state: &Entity<RecoveryState>,
    _window: &mut Window,
    cx: &mut App,
) -> gpui_kit::component::dialog::DialogContent {
    let s = state.read(cx);
    let rows = s.rows.clone();
    let busy = s.busy.clone();
    let error = s.error.clone();
    let preview_id = s.preview_id.clone();
    let preview_loading = s.preview_loading;
    let preview_transcripts = s.preview_transcripts.clone();
    let preview_error = s.preview_error.clone();

    let mut body = v_flex().gap_3().w_full().child(Label::new(
        "A previous run of Parley closed unexpectedly while one of these meetings \
         was recording. You can recover what was captured, or discard it.",
    ));

    if let Some(err) = error {
        body = body.child(Label::new(format!("Error: {}", err)).text_sm());
    }

    for row in rows {
        let id = row.meeting_id.clone();
        let recover_id = id.clone();
        let delete_id = id.clone();
        let preview_toggle_id = id.clone();
        let is_busy = busy.as_deref() == Some(id.as_str());
        let is_previewing = preview_id.as_deref() == Some(id.as_str());
        let recover_state = state.clone();
        let delete_state = state.clone();
        let preview_state = state.clone();

        let mut card = v_flex().w_full().gap_2().p_2().rounded_md().border_1().border_color(cx.theme().border).child(
            h_flex()
                .w_full()
                .justify_between()
                .items_center()
                .child(
                    v_flex()
                        .child(Label::new(if row.title.trim().is_empty() {
                            "Untitled meeting".to_string()
                        } else {
                            row.title.clone()
                        }))
                        .child(
                            Label::new(format!("{} segment(s)", row.segment_count)).text_sm(),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new(SharedString::from(format!("recovery-preview-{}", id)))
                                .ghost()
                                .label(if is_previewing { "Hide preview" } else { "Preview" })
                                .on_click(move |_, _, cx| {
                                    preview_state.update(cx, |s, cx| s.toggle_preview(preview_toggle_id.clone(), cx));
                                }),
                        )
                        .child(
                            Button::new(SharedString::from(format!("recovery-delete-{}", id)))
                                .label("Delete")
                                .disabled(is_busy)
                                .on_click(move |_, _, cx| {
                                    delete_state.update(cx, |s, cx| s.delete(delete_id.clone(), cx));
                                }),
                        )
                        .child(
                            Button::new(SharedString::from(format!("recovery-recover-{}", id)))
                                .label(if is_busy { "Working…" } else { "Recover" })
                                .primary()
                                .disabled(is_busy)
                                .on_click(move |_, _, cx| {
                                    recover_state
                                        .update(cx, |s, cx| s.recover(recover_id.clone(), cx));
                                }),
                        ),
                ),
        );

        if is_previewing {
            let mut preview = v_flex()
                .w_full()
                .gap_1()
                .p_2()
                .rounded_md()
                .bg(cx.theme().muted.opacity(0.3));

            if preview_loading {
                preview = preview.child(Label::new("Loading preview…").text_sm());
            } else if let Some(err) = &preview_error {
                preview = preview.child(Label::new(format!("Error: {}", err)).text_sm());
            } else if preview_transcripts.is_empty() {
                preview = preview.child(Label::new("No transcript segments captured yet.").text_sm());
            } else {
                let count = preview_transcripts.len();
                let duration_label = preview_duration_label(&preview_transcripts);
                let last_updated = preview_transcripts.last().map(|t| t.timestamp.clone()).filter(|s| !s.is_empty());

                let mut meta = h_flex().gap_3().text_xs().text_color(cx.theme().muted_foreground).child(format!(
                    "{} segment{}",
                    count,
                    if count == 1 { "" } else { "s" }
                ));
                if let Some(d) = duration_label {
                    meta = meta.child(format!("· {d}"));
                }
                if let Some(last) = last_updated {
                    meta = meta.child(format!("· last updated {last}"));
                }
                preview = preview.child(meta);

                let list_id = SharedString::from(format!("recovery-preview-list-{}", row.meeting_id));
                let transcripts = preview_transcripts.clone();
                preview = preview.child(
                    uniform_list(list_id, count, move |range, _window, cx| {
                        range
                            .map(|ix| {
                                let t = &transcripts[ix];
                                let time = t
                                    .audio_start_time
                                    .map(|s| format!("[{:02}:{:02}]", (s as u64) / 60, (s as u64) % 60))
                                    .unwrap_or_else(|| t.timestamp.clone());
                                let speaker = t.speaker.clone().unwrap_or_default();
                                let label = if speaker.is_empty() { time } else { format!("{time} {speaker}:") };
                                h_flex()
                                    .w_full()
                                    .gap_2()
                                    .py_1()
                                    .text_xs()
                                    .child(div().text_color(cx.theme().muted_foreground).child(label))
                                    .child(div().flex_1().min_w_0().text_sm().child(t.text.clone()))
                                    .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    })
                    .w_full()
                    .h(PREVIEW_LIST_HEIGHT),
                );
            }

            card = card.child(preview);
        }

        body = body.child(card);
    }

    content.child(body)
}
