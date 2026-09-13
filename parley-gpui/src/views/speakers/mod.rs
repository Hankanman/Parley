//! Speakers page: the stored voice profiles list from the frontend's
//! `SpeakerSettings.tsx` — rename, merge, or delete a saved speaker — plus
//! the "Your voice" self-enrollment section from
//! `SelfVoiceEnrollment.tsx`, which records a short sample from the
//! microphone so the local user's own voice is recognised in transcripts
//! instead of being clustered as "Speaker N". The self-enrolled profile
//! (`VoiceProfile::is_self`) is excluded from the saved-speakers list below
//! — same as the frontend, which shows it in this separate section instead.
//!
//! Enrollment never starts on its own — it only runs from an explicit
//! "Record my voice" / "Re-record" click, so opening this page (including
//! in an automated/smoke-test run) never touches the microphone.

mod logic;

use gpui_kit::component::{
    ActiveTheme, Disableable as _, Icon, IconName, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    notification::{Notification, NotificationType},
    v_flex,
};
use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::base::StyledExt as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use parley_core::audio::recording_preferences::{self, RecordingPreferences};
use parley_core::database::models::VoiceProfile;
use parley_core::database::repositories::voice_profile::VoiceProfilesRepository;
use parley_core::speaker_diarization::enrollment::{
    self, EnrollmentProgress, SelfVoiceStatus,
};
use parley_core::speaker_diarization::service::merge_voice_profiles_core;

use crate::app_state::AppServices;
use crate::core_events::CoreEvent;
use crate::runtime::Io;

/// Where the "Your voice" section is in its enrollment flow. Mirrors the
/// React component's `Mode` union.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelfVoiceMode {
    Idle,
    Recording,
    Saving,
}

pub struct SpeakersView {
    profiles: Vec<VoiceProfile>,
    loading: bool,
    error: Option<String>,
    /// Profile currently shown with editable name/email fields, if any.
    editing_id: Option<String>,
    name_input: Entity<InputState>,
    email_input: Entity<InputState>,
    /// Profile currently showing a "merge into…" candidate list, if any.
    merging_id: Option<String>,

    // -- Self-voice enrollment ("Your voice" section) --
    self_status: Option<SelfVoiceStatus>,
    self_mode: SelfVoiceMode,
    self_progress: Option<EnrollmentProgress>,
    self_error: Option<String>,
    self_name_input: Entity<InputState>,
    /// Guards against double-saving: the progress listener fires ~10x/sec
    /// and would otherwise call `save` repeatedly once past the target.
    self_finishing: bool,

    pending_toasts: Vec<(NotificationType, String)>,
}

impl SpeakersView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("Name"));
        let email_input = cx.new(|cx| InputState::new(window, cx).placeholder("Email (optional)"));
        let self_name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Me").default_value("Me"));

        let mut this = Self {
            profiles: Vec::new(),
            loading: true,
            error: None,
            editing_id: None,
            name_input,
            email_input,
            merging_id: None,
            self_status: None,
            self_mode: SelfVoiceMode::Idle,
            self_progress: None,
            self_error: None,
            self_name_input,
            self_finishing: false,
            pending_toasts: Vec::new(),
        };
        this.refresh(cx);
        this.refresh_self_voice(cx);
        this.subscribe_to_core_events(cx);
        this
    }

    // ------------------------------------------------------------------
    // Self-voice enrollment ("Your voice")
    // ------------------------------------------------------------------

    fn subscribe_to_core_events(&mut self, cx: &mut Context<Self>) {
        let core_events = AppServices::global(cx).core_events.clone();
        cx.subscribe(&core_events, |this, _, event: &CoreEvent, cx| {
            if event.name == "self-voice-enrollment-progress" {
                if let Some(progress) = event.decode::<EnrollmentProgress>() {
                    this.on_self_voice_progress(progress, cx);
                }
            }
        })
        .detach();
    }

    fn on_self_voice_progress(&mut self, progress: EnrollmentProgress, cx: &mut Context<Self>) {
        // A late tick from a session that's already been cancelled/saved —
        // ignore it rather than resurrecting the recording UI.
        if self.self_mode != SelfVoiceMode::Recording {
            return;
        }
        // Stop on our own once they've talked long enough, so the happy
        // path needs one click, not two.
        if !self.self_finishing && logic::self_voice_should_auto_save(&progress) {
            self.self_finishing = true;
            self.self_progress = Some(progress);
            cx.notify();
            self.save_self_voice(cx);
            return;
        }
        self.self_progress = Some(progress);
        cx.notify();
    }

    fn refresh_self_voice(&mut self, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let result = io.spawn(async move { enrollment::self_voice_status_with_pool(&pool).await }).await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(Ok(status)) => this.apply_self_status(status, window, cx),
                Ok(Err(e)) => this.self_error = Some(e),
                Err(e) => this.self_error = Some(format!("Self-voice status task panicked: {e}")),
            });
        })
        .detach();
    }

    fn apply_self_status(
        &mut self,
        status: SelfVoiceStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = status.name.clone().unwrap_or_else(|| "Me".to_string());
        self.self_name_input.update(cx, |state, cx| {
            state.set_value(label, window, cx);
        });
        self.self_status = Some(status);
        cx.notify();
    }

    fn start_self_voice_record(&mut self, cx: &mut Context<Self>) {
        self.self_error = None;
        self.self_progress = None;
        self.self_finishing = false;

        let services = AppServices::global(cx);
        let sink = services.sink.clone();
        let io = services.io.clone();
        let pool = services.pool();

        cx.spawn(async move |this, cx| {
            // Enroll through whichever mic the user records meetings with —
            // a profile built on a different device generalises worse.
            let handle = io.spawn(async move {
                let prefs = recording_preferences::load_recording_preferences(pool)
                    .await
                    .unwrap_or_else(|_| RecordingPreferences::default());
                let mic_device = prefs.preferred_mic_device;
                enrollment::start_self_voice_enrollment_with_sink(sink, mic_device).await
            });
            let result = handle.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(Ok(())) => {
                    this.self_mode = SelfVoiceMode::Recording;
                    cx.notify();
                }
                Ok(Err(e)) => this.toast(NotificationType::Error, e),
                Err(e) => this.toast(NotificationType::Error, format!("Enrollment task panicked: {e}")),
            });
        })
        .detach();
    }

    fn save_self_voice(&mut self, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        self.self_mode = SelfVoiceMode::Saving;
        cx.notify();

        let name = self.self_name_input.read(cx).value().to_string();
        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let handle = io.spawn(async move {
                enrollment::finish_self_voice_enrollment_with_pool(&pool, Some(name)).await
            });
            let result = handle.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.self_mode = SelfVoiceMode::Idle;
                this.self_progress = None;
                this.self_finishing = false;
                match result {
                    Ok(Ok(status)) => this.apply_self_status(status, window, cx),
                    Ok(Err(e)) => this.toast(NotificationType::Error, e),
                    Err(e) => this.toast(NotificationType::Error, format!("Save task panicked: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn cancel_self_voice_record(&mut self, cx: &mut Context<Self>) {
        self.self_mode = SelfVoiceMode::Idle;
        self.self_progress = None;
        self.self_finishing = false;
        cx.notify();

        let io = Io::global(cx);
        io.spawn(async move {
            let _ = enrollment::cancel_self_voice_enrollment().await;
        });
    }

    /// True while self-voice enrollment is actively recording from the mic —
    /// the shell checks this before letting the user navigate away, so the
    /// microphone doesn't stay open on an abandoned enrollment session.
    pub fn is_enrollment_recording(&self) -> bool {
        self.self_mode == SelfVoiceMode::Recording
    }

    /// Called by the shell when navigating away from this page while
    /// enrollment is recording: cancels the in-flight enrollment exactly
    /// like the "Cancel" button, without requiring the page to still be on
    /// screen. A no-op otherwise.
    pub fn on_leave(&mut self, cx: &mut Context<Self>) {
        if self.is_enrollment_recording() {
            self.cancel_self_voice_record(cx);
        }
    }

    fn save_self_voice_name(&mut self, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let name = self.self_name_input.read(cx).value().to_string();
        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let handle = io.spawn(async move {
                enrollment::rename_self_voice_profile_with_pool(&pool, name).await
            });
            let result = handle.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(Ok(status)) => this.apply_self_status(status, window, cx),
                Ok(Err(e)) => this.toast(NotificationType::Error, e),
                Err(e) => this.toast(NotificationType::Error, format!("Rename task panicked: {e}")),
            });
        })
        .detach();
    }

    fn confirm_remove_self_voice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            alert
                .title("Remove your voice profile")
                .description(
                    "Deletes your self-enrolled voice. Past transcripts keep their \"Me\" \
                     labels, but future meetings won't auto-tag your voice — it'll be \
                     clustered as \"Speaker N\" again until you re-enroll.",
                )
                .show_cancel(true)
                .on_ok(move |_, _, cx| {
                    this.update(cx, |this, cx| this.remove_self_voice(cx));
                    true
                })
        });
    }

    fn remove_self_voice(&mut self, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let handle = io.spawn(async move { enrollment::delete_self_voice_profile_with_pool(&pool).await });
            let result = handle.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(_)) => this.refresh_self_voice(cx),
                    Ok(Err(e)) => this.toast(NotificationType::Error, e),
                    Err(e) => this.toast(NotificationType::Error, format!("Delete task panicked: {e}")),
                }
            });
        })
        .detach();
    }

    fn toast(&mut self, kind: NotificationType, message: String) {
        self.pending_toasts.push((kind, message));
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
            let result = io.spawn(async move { VoiceProfilesRepository::list_all(&pool).await }).await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(Ok(profiles)) => this.profiles = profiles,
                    Ok(Err(e)) => this.error = Some(format!("Failed to load speakers: {e}")),
                    Err(e) => this.error = Some(format!("Failed to load speakers: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn start_edit(&mut self, profile: &VoiceProfile, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_id = Some(profile.id.clone());
        self.merging_id = None;
        let name = profile.name.clone();
        let email = profile.email.clone().unwrap_or_default();
        self.name_input.update(cx, |state, cx| state.set_value(name, window, cx));
        self.email_input.update(cx, |state, cx| state.set_value(email, window, cx));
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
        let name = self.name_input.read(cx).value().to_string();
        if name.trim().is_empty() {
            return;
        }
        let email = self.email_input.read(cx).value().to_string();
        let email = if email.trim().is_empty() { None } else { Some(email) };
        cx.notify();

        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move { VoiceProfilesRepository::update_profile(&pool, &id, &name, email.as_deref()).await })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(Ok(_)) => this.refresh(cx),
                Ok(Err(e)) => log::error!("Failed to update speaker: {e}"),
                Err(e) => log::error!("Update-speaker task panicked: {e}"),
            });
        })
        .detach();
    }

    fn confirm_delete(&mut self, profile: &VoiceProfile, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        let id = profile.id.clone();
        let name = profile.name.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            let id = id.clone();
            alert
                .title("Delete speaker")
                .description(format!(
                    "Removes the voice profile for \"{name}\". Past transcripts keep the \
                     displayed name but stop being linked to a profile, and future meetings \
                     won't auto-tag this voice."
                ))
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
        self.profiles.retain(|p| p.id != id);
        cx.notify();

        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let result = io.spawn(async move { VoiceProfilesRepository::delete(&pool, &id).await }).await;
            if !matches!(result, Ok(Ok(true))) {
                let _ = this.update(cx, |this, cx| this.refresh(cx));
            }
        })
        .detach();
    }

    fn start_merge(&mut self, loser_id: String, cx: &mut Context<Self>) {
        self.editing_id = None;
        self.merging_id = Some(loser_id);
        cx.notify();
    }

    fn cancel_merge(&mut self, cx: &mut Context<Self>) {
        self.merging_id = None;
        cx.notify();
    }

    fn confirm_merge(
        &mut self,
        loser: &VoiceProfile,
        winner: &VoiceProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity();
        let winner_id = winner.id.clone();
        let loser_id = loser.id.clone();
        let winner_name = winner.name.clone();
        let loser_name = loser.name.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            let winner_id = winner_id.clone();
            let loser_id = loser_id.clone();
            alert
                .title("Merge speaker")
                .description(format!(
                    "Folds \"{loser_name}\" into \"{winner_name}\". The chosen speaker keeps its \
                     name and email; samples from both profiles are combined and every \
                     transcript currently linked to \"{loser_name}\" is re-pointed at \
                     \"{winner_name}\". This profile is then deleted."
                ))
                .show_cancel(true)
                .on_ok(move |_, _, cx| {
                    this.update(cx, |this, cx| this.merge(winner_id.clone(), loser_id.clone(), cx));
                    true
                })
        });
    }

    fn merge(&mut self, winner_id: String, loser_id: String, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        self.merging_id = None;
        cx.notify();

        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move { merge_voice_profiles_core(&pool, &winner_id, &loser_id).await })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(Ok(_)) => this.refresh(cx),
                Ok(Err(e)) => log::error!("Failed to merge speakers: {e}"),
                Err(e) => log::error!("Merge-speakers task panicked: {e}"),
            });
        })
        .detach();
    }
}

impl Render for SpeakersView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for (kind, message) in self.pending_toasts.drain(..) {
            window.push_notification(Notification::new().message(message).with_type(kind), cx);
        }

        v_flex()
            .size_full()
            .child(self.render_header(cx))
            .child(self.render_self_voice_section(cx))
            .child(self.render_body(cx))
    }
}

impl SpeakersView {
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w_full()
            .gap_1()
            .p_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(IconName::User.view(cx))
                    .child(div().text_lg().font_semibold().child("Speakers")),
            )
            .child(
                div().text_xs().text_color(cx.theme().muted_foreground).child(
                    "Voice profiles saved from your transcripts. Rename, merge duplicates, or \
                     remove one you no longer need.",
                ),
            )
    }

    fn render_self_voice_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w_full()
            .gap_2()
            .p_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().text_sm().font_medium().child("Your voice"))
            .child(
                div().text_xs().text_color(cx.theme().muted_foreground).child(
                    "Record a short sample of yourself speaking so meetings can label your \
                     voice with the name below, instead of grouping you in with everyone else \
                     the microphone picks up. Optional — skip it and nothing changes.",
                ),
            )
            .child(
                div()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().border)
                    .p_4()
                    .child(if self.self_mode == SelfVoiceMode::Recording {
                        self.render_self_voice_recording(cx).into_any_element()
                    } else {
                        self.render_self_voice_idle(cx).into_any_element()
                    }),
            )
            .when_some(self.self_error.clone(), |el, err| {
                el.child(div().text_xs().text_color(cx.theme().danger).child(err))
            })
    }

    fn render_self_voice_idle(&self, cx: &mut Context<Self>) -> AnyElement {
        let saving = self.self_mode == SelfVoiceMode::Saving;
        let enrolled = self.self_status.as_ref().is_some_and(|s| s.enrolled);
        let model_ready = self.self_status.as_ref().is_some_and(|s| s.model_ready);
        let name_value = self.self_name_input.read(cx).value().to_string();
        let stored_name = self.self_status.as_ref().and_then(|s| s.name.as_deref());
        let name_changed = logic::self_voice_name_changed(enrolled, &name_value, stored_name);

        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_1p5()
                    .child(div().text_sm().font_medium().child("Name"))
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(div().w_64().child(Input::new(&self.self_name_input)))
                            .when(name_changed, |el| {
                                el.child(
                                    Button::new("self-voice-save-name")
                                        .primary()
                                        .label("Save")
                                        .on_click(cx.listener(|this, _, _, cx| this.save_self_voice_name(cx))),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Shown in transcripts wherever your voice is recognised."),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(if enrolled {
                        v_flex()
                            .gap_1()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_1p5()
                                    .child(Icon::new(AssetIcon::CircleCheck).text_color(cx.theme().success))
                                    .child(div().text_sm().font_medium().child("Enrolled")),
                            )
                            .child(
                                div().text_xs().text_color(cx.theme().muted_foreground).child(
                                    logic::self_voice_enrolled_subtitle(
                                        self.self_status.as_ref().and_then(|s| s.sample_count),
                                        self.self_status.as_ref().and_then(|s| s.updated_at.as_deref()),
                                    ),
                                ),
                            )
                            .into_any_element()
                    } else {
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().font_medium().child("Not enrolled"))
                            .child(
                                div().text_xs().text_color(cx.theme().muted_foreground).child(
                                    if self.self_status.is_some() && !model_ready {
                                        "Download the speaker model first — it's what recognises voices."
                                    } else {
                                        "Takes about 20 seconds."
                                    },
                                ),
                            )
                            .into_any_element()
                    })
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("self-voice-record")
                                    .when(enrolled, |b| b.outline())
                                    .when(!enrolled, |b| b.primary())
                                    .icon(AssetIcon::Mic)
                                    .label(if saving {
                                        "Saving…"
                                    } else if enrolled {
                                        "Re-record"
                                    } else {
                                        "Record my voice"
                                    })
                                    .disabled(saving || !model_ready)
                                    .on_click(cx.listener(|this, _, _, cx| this.start_self_voice_record(cx))),
                            )
                            .when(enrolled, |el| {
                                el.child(
                                    Button::new("self-voice-remove")
                                        .ghost()
                                        .icon(IconName::Delete)
                                        .tooltip("Remove your voice profile")
                                        .disabled(saving)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.confirm_remove_self_voice(window, cx);
                                        })),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_self_voice_recording(&self, cx: &mut Context<Self>) -> AnyElement {
        let remaining = self
            .self_progress
            .as_ref()
            .map(logic::self_voice_remaining_secs)
            .unwrap_or(20);
        let can_save = self.self_progress.as_ref().is_some_and(|p| p.can_save);
        let level = self
            .self_progress
            .as_ref()
            .map(|p| p.rms_level.max(p.peak_level))
            .unwrap_or(0.0);

        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(div().text_sm().font_medium().child("Read this aloud until the timer runs out"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{remaining}s")),
                    ),
            )
            .child(
                div()
                    .rounded_md()
                    .border_l_2()
                    .border_color(cx.theme().primary)
                    .bg(cx.theme().muted.opacity(0.5))
                    .px_3()
                    .py_2()
                    .text_sm()
                    .child(logic::SELF_VOICE_READING_PASSAGE),
            )
            .child(
                div().text_xs().text_color(cx.theme().muted_foreground).child(
                    "Speak at a normal, steady pace. Anything works if you'd rather not read — \
                     just keep talking.",
                ),
            )
            .child(self.self_voice_level_bar(level))
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("self-voice-save")
                            .primary()
                            .label("Save")
                            .disabled(!can_save)
                            .on_click(cx.listener(|this, _, _, cx| this.save_self_voice(cx))),
                    )
                    .child(
                        Button::new("self-voice-cancel")
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_self_voice_record(cx))),
                    )
                    .when(!can_save, |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("Keep going — we need a few more seconds."),
                        )
                    }),
            )
            .into_any_element()
    }

    fn self_voice_level_bar(&self, level: f32) -> impl IntoElement {
        let level = level.clamp(0.0, 1.0);
        div()
            .w_full()
            .h_2()
            .rounded_full()
            .bg(hsla(0., 0., 0.5, 0.15))
            .overflow_hidden()
            .child(
                div()
                    .h_full()
                    .rounded_full()
                    .w(relative(level))
                    .bg(hsla(0.38 - 0.1 * level, 0.65, 0.5, 1.)),
            )
    }

    fn render_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.loading {
            return div().size_full().p_6().child("Loading speakers…").into_any_element();
        }
        if let Some(err) = &self.error {
            return div()
                .size_full()
                .p_6()
                .text_color(cx.theme().danger)
                .child(err.clone())
                .into_any_element();
        }

        let visible = logic::visible_profiles(&self.profiles);
        if visible.is_empty() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .p_6()
                .text_color(cx.theme().muted_foreground)
                .child("No saved speakers yet.")
                .child(
                    div()
                        .text_xs()
                        .child("Name a \"Speaker N\" chip on a transcript to save a voice profile here."),
                )
                .into_any_element();
        }

        div()
            .id("speakers-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .w_full()
                    .gap_2()
                    .p_4()
                    .children(visible.iter().map(|p| self.render_row(p, &visible, cx))),
            )
            .into_any_element()
    }

    fn render_row(
        &self,
        profile: &VoiceProfile,
        visible: &[&VoiceProfile],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = profile.id.clone();

        if self.editing_id.as_deref() == Some(profile.id.as_str()) {
            return v_flex()
                .w_full()
                .gap_2()
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().border)
                .child(Input::new(&self.name_input))
                .child(Input::new(&self.email_input))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new(format!("save-speaker-{id}"))
                                .primary()
                                .label("Save")
                                .on_click(cx.listener(|this, _, _, cx| this.save_edit(cx))),
                        )
                        .child(
                            Button::new(format!("cancel-speaker-{id}"))
                                .ghost()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_edit(cx))),
                        ),
                )
                .into_any_element();
        }

        let id_for_edit = id.clone();
        let id_for_merge = id.clone();
        let id_for_delete = id.clone();

        let row = h_flex()
            .w_full()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(div().text_sm().font_medium().child(profile.name.clone()))
                    .child(
                        h_flex()
                            .gap_3()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(profile.email.clone().unwrap_or_else(|| "—".to_string()))
                            .child(format!(
                                "{} sample{}",
                                profile.sample_count,
                                if profile.sample_count == 1 { "" } else { "s" }
                            ))
                            .child(format!("Updated {}", logic::format_updated(&profile.updated_at))),
                    ),
            )
            .child(
                Button::new(format!("edit-speaker-{id}"))
                    .ghost()
                    .icon(AssetIcon::Pencil)
                    .tooltip("Rename")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(profile) = this.profiles.iter().find(|p| p.id == id_for_edit).cloned() {
                            this.start_edit(&profile, window, cx);
                        }
                    })),
            )
            .child(
                Button::new(format!("merge-speaker-{id}"))
                    .ghost()
                    .icon(AssetIcon::Merge)
                    .tooltip("Merge into another speaker")
                    .disabled(visible.len() < 2)
                    .on_click(cx.listener(move |this, _, _, cx| this.start_merge(id_for_merge.clone(), cx))),
            )
            .child(
                Button::new(format!("delete-speaker-{id}"))
                    .ghost()
                    .icon(IconName::Delete)
                    .tooltip("Delete")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(profile) = this.profiles.iter().find(|p| p.id == id_for_delete).cloned() {
                            this.confirm_delete(&profile, window, cx);
                        }
                    })),
            );

        if self.merging_id.as_deref() != Some(profile.id.as_str()) {
            return row.into_any_element();
        }

        let candidates = logic::merge_candidates(visible, &profile.id);
        v_flex()
            .w_full()
            .gap_2()
            .child(row)
            .child(
                v_flex()
                    .gap_1()
                    .pl_3()
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("Merge into:"))
                    .children(candidates.iter().map(|winner| {
                        let winner_id = winner.id.clone();
                        let loser_id = profile.id.clone();
                        Button::new(format!("merge-into-{}-{}", profile.id, winner.id))
                            .ghost()
                            .label(winner.name.clone())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let loser = this.profiles.iter().find(|p| p.id == loser_id).cloned();
                                let winner = this.profiles.iter().find(|p| p.id == winner_id).cloned();
                                if let (Some(loser), Some(winner)) = (loser, winner) {
                                    this.confirm_merge(&loser, &winner, window, cx);
                                }
                            }))
                    }))
                    .child(
                        Button::new(format!("cancel-merge-{}", profile.id))
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_merge(cx))),
                    ),
            )
            .into_any_element()
    }
}
