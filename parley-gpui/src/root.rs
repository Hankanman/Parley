//! Top-level view `main.rs` opens instead of `AppShell` directly: gates on
//! onboarding status (mirrors `frontend/src/components/layout/AppShell.tsx`
//! choosing between `OnboardingFlow` and the normal app based on
//! `OnboardingContext`'s `completed` flag) and, once the shell is shown,
//! runs interrupted-meeting recovery (`recovery.rs`).
//!
//! Three states:
//! - `Loading` — briefly, while an existing database's onboarding status is
//!   being read (only on a normal launch; a first launch skips straight to
//!   `Onboarding` since there's no status to read yet).
//! - `Onboarding` — a first launch, or a database that exists but never
//!   finished onboarding (resumed at its persisted step).
//! - `Shell` — the normal app.

use gpui_kit::component::label::Label;
use gpui_kit::*;
use parley_core::onboarding;

use crate::app_state::AppServices;
use crate::recovery;
use crate::shell::AppShell;
use crate::views::onboarding::{OnboardingEvent, OnboardingView, Step};

enum State {
    Loading,
    Onboarding(Entity<OnboardingView>),
    Shell(Entity<AppShell>),
}

pub struct RootView {
    state: State,
    _subscription: Option<Subscription>,
}

impl RootView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        if !AppServices::global(cx).has_db() {
            // Genuine first launch: no database to check a status against —
            // go straight to onboarding, which creates one.
            let onboarding = cx.new(|cx| OnboardingView::new(true, Step::Welcome, window, cx));
            let subscription = Self::subscribe_onboarding(&onboarding, window, cx);
            return Self {
                state: State::Onboarding(onboarding),
                _subscription: Some(subscription),
            };
        }

        // A database already exists (normal launch): read its onboarding
        // status before deciding — an existing-but-incomplete status means a
        // previous run quit mid-onboarding, so resume it instead of opening
        // the shell (mirrors `OnboardingContext`'s `completed` gate in
        // `AppShell.tsx`).
        let pool = AppServices::global(cx).pool();
        let io = AppServices::global(cx).io.clone();
        cx.spawn_in(window, async move |this, cx| {
            let status = match pool {
                Some(pool) => io
                    .spawn(async move { onboarding::load_onboarding_status(Some(pool)).await })
                    .await
                    .ok()
                    .and_then(|r| r.ok()),
                None => None,
            };

            let _ = this.update_in(cx, |this, window, cx| {
                match status {
                    Some(status) if !status.completed => {
                        let step = Step::clamp(status.current_step);
                        let onboarding_view =
                            cx.new(|cx| OnboardingView::new(false, step, window, cx));
                        this._subscription =
                            Some(Self::subscribe_onboarding(&onboarding_view, window, cx));
                        this.state = State::Onboarding(onboarding_view);
                    }
                    _ => {
                        let shell = cx.new(|cx| AppShell::new(window, cx));
                        this.state = State::Shell(shell);
                        recovery::check_on_startup(window, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();

        Self {
            state: State::Loading,
            _subscription: None,
        }
    }

    fn subscribe_onboarding(
        onboarding: &Entity<OnboardingView>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(onboarding, window, |this, _, event, window, cx| {
            let OnboardingEvent::Completed = event;
            let shell = cx.new(|cx| AppShell::new(window, cx));
            this.state = State::Shell(shell);
            this._subscription = None;
            recovery::check_on_startup(window, cx);
            cx.notify();
        })
    }
}

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match &self.state {
            State::Loading => div()
                .size_full()
                .items_center()
                .justify_center()
                .flex()
                .child(Label::new("Loading…"))
                .into_any_element(),
            State::Onboarding(view) => view.clone().into_any_element(),
            State::Shell(view) => view.clone().into_any_element(),
        }
    }
}
