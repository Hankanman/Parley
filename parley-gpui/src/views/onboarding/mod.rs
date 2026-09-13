//! First-launch onboarding flow: welcome -> setup overview (creates the
//! database) -> model download progress -> done. Mirrors the React flow
//! (`frontend/src/components/onboarding/**`,
//! `frontend/src/contexts/OnboardingContext.tsx`) but talks to
//! `parley-core` directly instead of through Tauri `invoke()` — every
//! function called here is the same Tauri-free core function the Tauri
//! commands (`onboarding_commands.rs`,
//! `commands/database/commands.rs::initialize_fresh_database`) wrap.
//!
//! `RootView` (`parley-gpui/src/root.rs`) decides whether to show this at
//! all — this module only implements the flow once shown.

mod logic;

use std::sync::Arc;

use gpui_kit::component::{
    ActiveTheme, Disableable as _, StyledExt as _, h_flex, v_flex,
    button::{Button, ButtonVariants as _},
    label::Label,
    progress::Progress,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tokio::sync::Mutex;

use parley_core::bootstrap::ModelManagerSlot;
use parley_core::config::DEFAULT_WHISPER_MODEL;
use parley_core::database::manager::DatabaseManager;
use parley_core::database::repositories::setting::SettingsRepository;
use parley_core::onboarding::{self, ModelStatus as OnboardingModelStatus};
use parley_core::summary::summary_engine::service as summary_service;
use parley_core::whisper_engine;

use crate::app_state::AppServices;
use crate::core_events::CoreEvent;

pub use logic::Step;

/// Emitted once onboarding has saved `completed: true` — `RootView`
/// subscribes to this and swaps in the normal shell.
pub enum OnboardingEvent {
    Completed,
}

#[derive(Debug, Clone, Default)]
struct DownloadState {
    downloaded: bool,
    in_progress: bool,
    progress: u32,
    downloaded_mb: f64,
    total_mb: f64,
    speed_mbps: f64,
    error: Option<String>,
}

pub struct OnboardingView {
    step: Step,
    /// True on a genuine first launch (no database at all yet) — the setup
    /// step must create one before continuing. False when resuming
    /// incomplete onboarding against an already-open database (see
    /// `root.rs`), in which case this is a no-op.
    needs_db_creation: bool,
    db_ready: bool,
    db_error: Option<String>,
    creating_db: bool,
    downloads_started: bool,
    whisper: DownloadState,
    summary: DownloadState,
    recommended_model: String,
    completing: bool,
    complete_error: Option<String>,
    model_manager: ModelManagerSlot,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<OnboardingEvent> for OnboardingView {}

impl OnboardingView {
    pub fn new(
        needs_db_creation: bool,
        start_step: Step,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let core_events = AppServices::global(cx).core_events.clone();
        let subscriptions = vec![cx.subscribe(&core_events, |this, _, event: &CoreEvent, cx| {
            this.on_core_event(event, cx);
        })];

        let recommended_model =
            summary_service::get_recommended_model().unwrap_or_else(|_| "gemma3:1b".to_string());

        let mut this = Self {
            step: start_step,
            needs_db_creation,
            db_ready: !needs_db_creation,
            db_error: None,
            creating_db: false,
            downloads_started: false,
            whisper: DownloadState::default(),
            summary: DownloadState::default(),
            recommended_model,
            completing: false,
            complete_error: None,
            model_manager: Arc::new(Mutex::new(None)),
            _subscriptions: subscriptions,
        };

        if needs_db_creation {
            this.start_db_creation(cx);
        }
        if this.step == Step::DownloadProgress {
            this.start_downloads(cx);
        }
        this
    }

    /// Mirrors `initialize_fresh_database` (Tauri):
    /// `DatabaseManager::new_default()` + the same default model config
    /// writes, then installs the result into `AppServices` so `pool()` /
    /// `recording_context()` work for the rest of the process without a
    /// restart.
    fn start_db_creation(&mut self, cx: &mut Context<Self>) {
        if self.creating_db || self.db_ready {
            return;
        }
        self.creating_db = true;
        let io = AppServices::global(cx).io.clone();
        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move {
                    let db = DatabaseManager::new_default().await?;
                    let pool = db.pool().clone();
                    if let Err(e) = SettingsRepository::save_model_config(
                        &pool,
                        "builtin-ai",
                        "gemma3:1b",
                        "large-v3",
                        None,
                    )
                    .await
                    {
                        log::error!("Failed to set default summary model config: {}", e);
                    }
                    if let Err(e) = SettingsRepository::save_transcript_config(
                        &pool,
                        "localWhisper",
                        DEFAULT_WHISPER_MODEL,
                    )
                    .await
                    {
                        log::error!("Failed to set default transcription model config: {}", e);
                    }
                    anyhow::Ok(db)
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.creating_db = false;
                match result {
                    Ok(Ok(db)) => {
                        AppServices::global(cx).set_db(db);
                        this.db_ready = true;
                        log::info!("Onboarding: fresh database initialized");
                    }
                    Ok(Err(e)) => {
                        log::error!("Onboarding: failed to initialize database: {}", e);
                        this.db_error = Some(e.to_string());
                    }
                    Err(e) => {
                        log::error!("Onboarding: database init task panicked: {}", e);
                        this.db_error = Some(e.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn go_next(&mut self, cx: &mut Context<Self>) {
        self.step = logic::next_step(self.step);
        if self.step == Step::DownloadProgress {
            self.start_downloads(cx);
        }
        cx.notify();
    }

    fn go_previous(&mut self, cx: &mut Context<Self>) {
        self.step = logic::previous_step(self.step);
        cx.notify();
    }

    /// Kick off both downloads once, same as `startBackgroundDownloads` in
    /// `OnboardingContext.tsx` (always both models — no separate opt-out UI
    /// on Linux).
    fn start_downloads(&mut self, cx: &mut Context<Self>) {
        if self.downloads_started {
            return;
        }
        self.downloads_started = true;

        let io = AppServices::global(cx).io.clone();

        // Whisper (speech recognition — always required).
        {
            let sink = AppServices::global(cx).sink.clone();
            self.whisper.in_progress = true;
            cx.spawn(async move |_this, _cx| {
                let _ = io
                    .spawn(async move {
                        whisper_engine::whisper_init().await.ok();
                        whisper_engine::download_model_with_progress(
                            sink,
                            logic::ONBOARDING_WHISPER_MODEL.to_string(),
                        )
                        .await
                    })
                    .await;
            })
            .detach();
        }

        // Summary model (built-in AI, recommended by RAM tier).
        {
            let io = AppServices::global(cx).io.clone();
            let sink = AppServices::global(cx).sink.clone();
            let model_manager = self.model_manager.clone();
            let model_name = self.recommended_model.clone();
            self.summary.in_progress = true;
            cx.spawn(async move |_this, _cx| {
                let _ = io
                    .spawn(async move {
                        match summary_service::ensure_manager(&model_manager).await {
                            Ok(manager) => {
                                let _ = summary_service::download_builtin_ai_model(
                                    manager, model_name, sink,
                                )
                                .await;
                            }
                            Err(e) => log::error!("Onboarding: model manager init failed: {}", e),
                        }
                    })
                    .await;
            })
            .detach();
        }
    }

    fn on_core_event(&mut self, event: &CoreEvent, cx: &mut Context<Self>) {
        match event.name.as_str() {
            "model-download-progress" => {
                if let Some(p) = event.decode::<WhisperProgress>() {
                    if p.model_name == logic::ONBOARDING_WHISPER_MODEL {
                        self.whisper.in_progress = true;
                        self.whisper.progress = p.progress as u32;
                        cx.notify();
                    }
                }
            }
            "model-download-complete" => {
                if let Some(p) = event.decode::<WhisperComplete>() {
                    if p.model_name == logic::ONBOARDING_WHISPER_MODEL {
                        self.whisper.downloaded = true;
                        self.whisper.in_progress = false;
                        self.whisper.progress = 100;
                        cx.notify();
                    }
                }
            }
            "model-download-error" => {
                if let Some(p) = event.decode::<WhisperError>() {
                    if p.model_name == logic::ONBOARDING_WHISPER_MODEL {
                        self.whisper.in_progress = false;
                        self.whisper.error = Some(p.error);
                        cx.notify();
                    }
                }
            }
            "builtin-ai-download-progress" => {
                if let Some(p) = event.decode::<SummaryProgress>() {
                    if p.model == self.recommended_model
                        || p.model == "gemma3:1b"
                        || p.model == "gemma3:4b"
                    {
                        self.summary.progress = p.progress as u32;
                        self.summary.downloaded_mb = p.downloaded_mb;
                        self.summary.total_mb = p.total_mb;
                        self.summary.speed_mbps = p.speed_mbps;
                        match p.status.as_str() {
                            "completed" => {
                                self.summary.downloaded = true;
                                self.summary.in_progress = false;
                            }
                            "error" => {
                                self.summary.in_progress = false;
                                self.summary.error = p.error;
                            }
                            _ => self.summary.in_progress = true,
                        }
                        cx.notify();
                    }
                }
            }
            _ => {}
        }
    }

    /// Mirrors the Tauri `complete_onboarding` command: persist the chosen
    /// model config, then mark onboarding status `completed`. Emits
    /// [`OnboardingEvent::Completed`] on success so `RootView` swaps in the
    /// normal shell — no restart.
    fn complete(&mut self, cx: &mut Context<Self>) {
        if self.completing {
            return;
        }
        if !logic::can_continue(self.whisper.downloaded, self.completing) {
            return;
        }
        self.completing = true;
        self.complete_error = None;
        cx.notify();

        let Some(pool) = AppServices::global(cx).pool() else {
            self.completing = false;
            self.complete_error = Some("Database not ready yet".to_string());
            cx.notify();
            return;
        };
        let io = AppServices::global(cx).io.clone();
        let model = self.recommended_model.clone();

        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move {
                    SettingsRepository::save_model_config(&pool, "builtin-ai", &model, "large-v3", None)
                        .await
                        .map_err(|e| e.to_string())?;
                    SettingsRepository::save_transcript_config(
                        &pool,
                        "localWhisper",
                        DEFAULT_WHISPER_MODEL,
                    )
                    .await
                    .map_err(|e| e.to_string())?;

                    let mut status = onboarding::load_onboarding_status(Some(pool.clone()))
                        .await
                        .unwrap_or_default();
                    status.completed = true;
                    status.current_step = Step::DownloadProgress.number();
                    status.model_status = OnboardingModelStatus {
                        transcription: "downloaded".to_string(),
                        summary: "downloaded".to_string(),
                    };
                    onboarding::save_onboarding_status(Some(pool), &status)
                        .await
                        .map_err(|e| e.to_string())
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.completing = false;
                match result {
                    Ok(Ok(())) => {
                        cx.emit(OnboardingEvent::Completed);
                    }
                    Ok(Err(e)) => {
                        this.complete_error = Some(e);
                    }
                    Err(e) => {
                        this.complete_error = Some(format!("Setup task panicked: {}", e));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

#[derive(serde::Deserialize)]
struct WhisperProgress {
    #[serde(rename = "modelName")]
    model_name: String,
    progress: u8,
}

#[derive(serde::Deserialize)]
struct WhisperComplete {
    #[serde(rename = "modelName")]
    model_name: String,
}

#[derive(serde::Deserialize)]
struct WhisperError {
    #[serde(rename = "modelName")]
    model_name: String,
    error: String,
}

#[derive(serde::Deserialize)]
struct SummaryProgress {
    model: String,
    progress: u8,
    downloaded_mb: f64,
    total_mb: f64,
    speed_mbps: f64,
    status: String,
    error: Option<String>,
}

impl Render for OnboardingView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content: AnyElement = match self.step {
            Step::Welcome => self.render_welcome(cx).into_any_element(),
            Step::SetupOverview => self.render_setup_overview(cx).into_any_element(),
            Step::DownloadProgress => self.render_download_progress(cx).into_any_element(),
        };

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                v_flex()
                    .w(px(520.))
                    .gap_6()
                    .p_8()
                    .child(step_indicator(self.step, cx))
                    .child(content),
            )
    }
}

fn step_indicator(step: Step, cx: &App) -> impl IntoElement {
    let active_color = cx.theme().foreground;
    let inactive_color = cx.theme().border;
    h_flex().gap_2().justify_center().children((1..=3).map(move |n| {
        let active = n <= step.number();
        div()
            .h(px(4.))
            .w(px(48.))
            .rounded_full()
            .bg(if active { active_color } else { inactive_color })
    }))
}

impl OnboardingView {
    fn render_welcome(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        v_flex()
            .gap_4()
            .items_center()
            .text_center()
            .child(Label::new("Welcome to Parley").text_xl().font_semibold())
            .child(Label::new(
                "A privacy-first meeting assistant that transcribes and summarizes \
                 entirely on your machine — nothing leaves your computer.",
            ))
            .child(
                Button::new("onboarding-welcome-next")
                    .label("Get started")
                    .primary()
                    .on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| this.go_next(cx));
                    }),
            )
    }

    fn render_setup_overview(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        let continue_disabled = self.needs_db_creation && (!self.db_ready || self.creating_db);
        let status_line = if let Some(err) = &self.db_error {
            format!("Setup failed: {}", err)
        } else if continue_disabled {
            "Setting up your local database…".to_string()
        } else {
            "Ready.".to_string()
        };

        v_flex()
            .gap_4()
            .child(Label::new("Setting things up").text_lg().font_semibold())
            .child(Label::new(
                "Parley will create a local database for your meetings and, on the next \
                 step, download a speech-recognition model and a small on-device \
                 summarization model. Everything stays on this machine.",
            ))
            .child(Label::new(status_line).text_sm())
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("onboarding-setup-back").label("Back").on_click({
                            let this = this.clone();
                            move |_, _, cx| {
                                this.update(cx, |this, cx| this.go_previous(cx));
                            }
                        }),
                    )
                    .child(
                        Button::new("onboarding-setup-next")
                            .label("Continue")
                            .primary()
                            .disabled(continue_disabled)
                            .on_click(move |_, _, cx| {
                                this.update(cx, |this, cx| this.go_next(cx));
                            }),
                    ),
            )
    }

    fn render_download_progress(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        let can_continue = logic::can_continue(self.whisper.downloaded, self.completing);

        v_flex()
            .gap_4()
            .child(Label::new("Getting things ready").text_lg().font_semibold())
            .child(Label::new(
                "You can start using Parley once the transcription engine finishes \
                 downloading; the summary model keeps downloading in the background.",
            ))
            .child(download_card(
                "Transcription engine",
                "~547 MB",
                &self.whisper,
                cx,
            ))
            .child(download_card(
                "Summary engine",
                logic::summary_model_size_label(&self.recommended_model),
                &self.summary,
                cx,
            ))
            .when_some(self.complete_error.clone(), |el, err| {
                el.child(Label::new(format!("Failed to finish setup: {}", err)).text_sm())
            })
            .child(
                Button::new("onboarding-download-continue")
                    .label(if self.completing { "Finishing…" } else { "Continue" })
                    .primary()
                    .disabled(!can_continue)
                    .on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| this.complete(cx));
                    }),
            )
    }
}

fn download_card(
    title: &'static str,
    size_label: &'static str,
    state: &DownloadState,
    cx: &App,
) -> impl IntoElement {
    let status = if state.downloaded {
        "Done".to_string()
    } else if state.error.is_some() {
        "Failed".to_string()
    } else if state.in_progress {
        format!("{}%", state.progress)
    } else {
        "Waiting…".to_string()
    };

    v_flex()
        .gap_2()
        .p_4()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().border)
        .child(
            h_flex()
                .justify_between()
                .child(
                    v_flex()
                        .child(Label::new(title).font_semibold())
                        .child(Label::new(size_label).text_sm()),
                )
                .child(Label::new(status)),
        )
        .child(
            Progress::new(SharedString::from(title))
                .value((state.progress as f32 / 100.0).clamp(0.0, 1.0)),
        )
        .when_some(state.error.clone(), |el, err| {
            el.child(Label::new(err).text_sm())
        })
}
