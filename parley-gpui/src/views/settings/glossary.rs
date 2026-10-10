//! "Custom vocabulary" group on the Transcription settings page (issue
//! #83): add / edit / remove glossary terms (each with an optional
//! "sounds like" hint), plus the two opt-in toggles (include speaker and
//! attendee names; spelling correction) and a live preview of the
//! sentence whisper will be prompted with.
//!
//! Backed by `parley_core::glossary` (one JSON blob in `app_settings`).
//! Kept in its own `Global` rather than `SettingsCache` so the glossary
//! loads/saves independently of the big settings batch in `state.rs`.

use std::sync::atomic::{AtomicU64, Ordering};

use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    label::Label,
    setting::{SettingField, SettingGroup, SettingItem},
    v_flex, ActiveTheme, Sizable as _,
};
use gpui_kit::*;

use parley_core::glossary::{self, prompt, Glossary, GlossaryTerm};

use crate::app_state::AppServices;
use crate::runtime::Io;

use super::SettingsView;

#[derive(Clone, Default)]
pub struct GlossaryCache {
    pub glossary: Glossary,
}

impl Global for GlossaryCache {}

impl GlossaryCache {
    fn get(cx: &App) -> &Glossary {
        &cx.global::<GlossaryCache>().glossary
    }
}

/// Load the saved glossary into the `GlossaryCache` global and redraw.
pub fn load(view: Entity<SettingsView>, cx: &mut App) {
    if !cx.has_global::<GlossaryCache>() {
        cx.set_global(GlossaryCache::default());
    }
    let Some(pool) = AppServices::global(cx).pool() else {
        return;
    };
    let io = Io::global(cx);
    cx.spawn(async move |cx| {
        let Ok(loaded) = io.spawn(async move { glossary::load(&pool).await }).await else {
            log::warn!("settings: glossary load task panicked");
            return;
        };
        let _ = cx.update(|cx| {
            cx.global_mut::<GlossaryCache>().glossary = loaded;
        });
        let _ = view.update(cx, |_, cx| cx.notify());
    })
    .detach();
}

/// Monotonic save counter: every edit (including each keystroke in a term
/// field) fires a save, and those run as independent tasks. Each save
/// carries its generation and is skipped if a newer one was issued by the
/// time it gets the lock, so the last edit always wins.
static SAVE_GENERATION: AtomicU64 = AtomicU64::new(0);
static SAVE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Apply `mutate` to the cached glossary, redraw, and persist.
fn update(cx: &mut App, view: &Entity<SettingsView>, mutate: impl FnOnce(&mut Glossary)) {
    let snapshot = {
        let cache = cx.global_mut::<GlossaryCache>();
        mutate(&mut cache.glossary);
        cache.glossary.clone()
    };
    let _ = view.update(cx, |_, cx| cx.notify());

    let Some(pool) = AppServices::global(cx).pool() else {
        log::warn!("settings: no DB pool yet, glossary not saved");
        return;
    };
    let generation = SAVE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    Io::global(cx).spawn(async move {
        let _guard = SAVE_LOCK.lock().await;
        if SAVE_GENERATION.load(Ordering::SeqCst) != generation {
            return; // a newer edit will save
        }
        if let Err(e) = glossary::save(&pool, &snapshot).await {
            log::warn!("settings: failed to save glossary: {}", e);
        }
    });
}

pub fn group(view: &Entity<SettingsView>, cx: &App) -> SettingGroup {
    let ids: Vec<String> = cx
        .try_global::<GlossaryCache>()
        .map(|c| c.glossary.terms.iter().map(|t| t.id.clone()).collect())
        .unwrap_or_default();

    let mut items = vec![add_term_item(view)];
    for id in ids {
        items.push(term_row_item(view, id));
    }
    items.push(preview_item());
    items.push(
        SettingItem::new(
            "Include speaker and attendee names",
            SettingField::switch(
                |cx: &App| {
                    cx.try_global::<GlossaryCache>()
                        .map(|c| c.glossary.include_known_names)
                        .unwrap_or(false)
                },
                {
                    let view = view.clone();
                    move |val: bool, cx: &mut App| {
                        update(cx, &view, |g| g.include_known_names = val);
                    }
                },
            ),
        )
        .description(
            "Also prompt Whisper with saved voice-profile names and, for meetings linked to a calendar event, its attendees. Added after your own terms.",
        )
        .keywords(["glossary", "names", "attendees", "speakers"]),
    );
    items.push(
        SettingItem::new(
            "Correct near-miss spellings",
            SettingField::switch(
                |cx: &App| {
                    cx.try_global::<GlossaryCache>()
                        .map(|c| c.glossary.correct_spellings)
                        .unwrap_or(false)
                },
                {
                    let view = view.clone();
                    move |val: bool, cx: &mut App| {
                        update(cx, &view, |g| g.correct_spellings = val);
                    }
                },
            ),
        )
        .description(
            "After transcribing, rewrite close misspellings of your terms (and their \"sounds like\" hints) to the exact spelling. Runs locally; never changes other words.",
        )
        .keywords(["glossary", "spelling", "correction"]),
    );

    SettingGroup::new()
        .title("Custom vocabulary")
        .description(
            "Names, products and acronyms Whisper should spell your way. Applied when a meeting is refined after recording, retranscribed, or imported — not to the live transcript. Whisper only.",
        )
        .items(items)
}

fn add_term_item(view: &Entity<SettingsView>) -> SettingItem {
    let view = view.clone();
    SettingItem::render(move |_options, window, cx| {
        struct AddState {
            term: Entity<InputState>,
            sounds_like: Entity<InputState>,
        }

        let state = window.use_keyed_state(SharedString::from("glossary-add-term"), cx, |window, cx| {
            AddState {
                term: cx.new(|cx| InputState::new(window, cx).placeholder("Term, e.g. Kubernetes")),
                sounds_like: cx.new(|cx| {
                    InputState::new(window, cx).placeholder("Sounds like (optional), e.g. cooper netties")
                }),
            }
        });
        let term_input = state.read(cx).term.clone();
        let sounds_input = state.read(cx).sounds_like.clone();

        let view_for_click = view.clone();
        let term_for_click = term_input.clone();
        let sounds_for_click = sounds_input.clone();

        h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .child(Input::new(&term_input).small().w_48())
            .child(Input::new(&sounds_input).small().flex_1())
            .child(Button::new("glossary-add").outline().small().label("Add").on_click(
                move |_, window, cx| {
                    let term = term_for_click.read(cx).value().trim().to_string();
                    let sounds_like = sounds_for_click.read(cx).value().trim().to_string();
                    if term.is_empty() {
                        return;
                    }
                    term_for_click.update(cx, |input, cx| input.set_value("", window, cx));
                    sounds_for_click.update(cx, |input, cx| input.set_value("", window, cx));
                    update(cx, &view_for_click, |g| {
                        let exists = g.terms.iter().any(|t| t.term.eq_ignore_ascii_case(&term));
                        if !exists {
                            g.terms.push(GlossaryTerm::new(term, sounds_like));
                        }
                    });
                },
            ))
            .into_any_element()
    })
    .keywords(["glossary", "vocabulary", "custom words", "add term"])
}

/// One editable term row: term + "sounds like" inputs (saved as you type)
/// and a remove button. Keyed by the term's stable id so the inputs keep
/// their state when other rows are removed.
fn term_row_item(view: &Entity<SettingsView>, id: String) -> SettingItem {
    let view = view.clone();
    SettingItem::render(move |_options, window, cx| {
        let Some(term) = cx
            .try_global::<GlossaryCache>()
            .and_then(|c| c.glossary.terms.iter().find(|t| t.id == id).cloned())
        else {
            return div().into_any_element();
        };

        struct RowState {
            term: Entity<InputState>,
            sounds_like: Entity<InputState>,
            _subscriptions: [Subscription; 2],
        }

        let state = window.use_keyed_state(
            SharedString::from(format!("glossary-term-{}", id)),
            cx,
            {
                let view = view.clone();
                let id = id.clone();
                move |window, cx| {
                    let term_input = cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder("Term")
                            .default_value(term.term.clone())
                    });
                    let sounds_input = cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder("Sounds like (optional)")
                            .default_value(term.sounds_like.clone())
                    });
                    let term_sub = cx.subscribe(&term_input, {
                        let view = view.clone();
                        let id = id.clone();
                        move |_, input, event: &InputEvent, cx| {
                            if matches!(event, InputEvent::Change) {
                                let value = input.read(cx).value().to_string();
                                update(cx, &view, |g| {
                                    if let Some(t) = g.terms.iter_mut().find(|t| t.id == id) {
                                        t.term = value;
                                    }
                                });
                            }
                        }
                    });
                    let sounds_sub = cx.subscribe(&sounds_input, {
                        let view = view.clone();
                        let id = id.clone();
                        move |_, input, event: &InputEvent, cx| {
                            if matches!(event, InputEvent::Change) {
                                let value = input.read(cx).value().to_string();
                                update(cx, &view, |g| {
                                    if let Some(t) = g.terms.iter_mut().find(|t| t.id == id) {
                                        t.sounds_like = value;
                                    }
                                });
                            }
                        }
                    });
                    RowState {
                        term: term_input,
                        sounds_like: sounds_input,
                        _subscriptions: [term_sub, sounds_sub],
                    }
                }
            },
        );
        let term_input = state.read(cx).term.clone();
        let sounds_input = state.read(cx).sounds_like.clone();

        let remove_view = view.clone();
        let remove_id = id.clone();

        h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .child(Input::new(&term_input).small().w_48())
            .child(Input::new(&sounds_input).small().flex_1())
            .child(
                Button::new(SharedString::from(format!("glossary-remove-{}", id)))
                    .ghost()
                    .small()
                    .icon(gpui_kit::assets::IconName::Trash)
                    .on_click(move |_, _, cx| {
                        let remove_id = remove_id.clone();
                        update(cx, &remove_view, |g| g.terms.retain(|t| t.id != remove_id));
                    }),
            )
            .into_any_element()
    })
}

/// Shows the exact sentence whisper will see (manual terms only — names
/// are resolved per meeting) and how many terms didn't fit the budget.
fn preview_item() -> SettingItem {
    SettingItem::render(move |_options, _window, cx| {
        let terms = cx
            .try_global::<GlossaryCache>()
            .map(|_| GlossaryCache::get(cx).term_strings())
            .unwrap_or_default();
        let muted = cx.theme().muted_foreground;

        let (text, note) = match prompt::build_glossary_prompt(&terms, prompt::GLOSSARY_TOKEN_BUDGET) {
            None => ("No terms yet.".to_string(), None),
            Some(built) if built.dropped.is_empty() => (built.text, None),
            Some(built) => (
                built.text,
                Some(format!(
                    "{} term(s) don't fit Whisper's prompt (spelling correction still covers them): {}",
                    built.dropped.len(),
                    built.dropped.join(", ")
                )),
            ),
        };

        let mut col = v_flex()
            .w_full()
            .gap_1()
            .child(Label::new("Whisper prompt preview").text_xs().text_color(muted))
            .child(Label::new(text).text_sm());
        if let Some(note) = note {
            col = col.child(Label::new(note).text_xs().text_color(cx.theme().warning));
        }
        col.into_any_element()
    })
}
