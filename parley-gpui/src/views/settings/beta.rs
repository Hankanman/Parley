//! "Beta features" + "About" settings page.
//!
//! Beta toggles mirror `frontend/src/types/betaFeatures.ts`'s
//! `BetaFeatures` shape/defaults, persisted under `KEY_BETA_FEATURES` — a
//! new DB row, since React keeps this in `localStorage` only (see that
//! key's doc comment in `parley-core`). About links open via `cx.open_url`.

use gpui_kit::component::{
    button::Button,
    h_flex, v_flex,
    label::Label,
    setting::{SettingField, SettingGroup, SettingItem, SettingPage},
    ActiveTheme, Icon,
};
use gpui_kit::*;

use super::state::{BetaFeatures, SettingsCache};
use super::SettingsView;

const REPO_URL: &str = "https://github.com/Hankanman/Parley";

pub fn page(view: &Entity<SettingsView>, cx: &mut Context<SettingsView>) -> SettingPage {
    let _ = cx;
    let view = view.clone();

    SettingPage::new("Beta & About")
        .icon(Icon::new(gpui_kit::assets::IconName::Sparkles))
        .group(
            SettingGroup::new()
                .title("Beta features")
                .description("Experimental features that may change or be removed.")
                .items(vec![
                    beta_toggle(
                        &view,
                        "Import & retranscribe",
                        "Import an external audio file as a new meeting and retranscribe it.",
                        |f| f.import_and_retranscribe,
                        |f, v| f.import_and_retranscribe = v,
                    ),
                    beta_toggle(
                        &view,
                        "Live action items",
                        "Extract action items from the transcript while still recording.",
                        |f| f.live_action_items,
                        |f, v| f.live_action_items = v,
                    ),
                ]),
        )
        .group(
            SettingGroup::new().title("About").items(vec![
                SettingItem::render(|_options, _window, cx| {
                    v_flex()
                        .gap_1()
                        .child(Label::new(format!("Parley {}", env!("CARGO_PKG_VERSION"))))
                        .child(
                            Label::new("Privacy-first AI meeting assistant, running entirely on-device.")
                                .text_color(cx.theme().muted_foreground),
                        )
                        .into_any_element()
                }),
                SettingItem::render(|_options, _window, cx| {
                    let _ = cx;
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("about-repo")
                                .outline()
                                .label("Repository")
                                .on_click(|_, _, cx| cx.open_url(REPO_URL)),
                        )
                        .child(
                            Button::new("about-issues")
                                .outline()
                                .label("Report an issue")
                                .on_click(|_, _, cx| cx.open_url(&format!("{}/issues/new", REPO_URL))),
                        )
                        .into_any_element()
                }),
            ]),
        )
}

fn beta_toggle(
    view: &Entity<SettingsView>,
    title: &'static str,
    description: &'static str,
    get: fn(&BetaFeatures) -> bool,
    set: fn(&mut BetaFeatures, bool),
) -> SettingItem {
    let view = view.clone();
    SettingItem::new(
        title,
        SettingField::switch(
            move |cx: &App| get(&SettingsCache::global(cx).beta),
            move |val: bool, cx: &mut App| {
                let mut features = SettingsCache::global(cx).beta.clone();
                set(&mut features, val);
                super::state::save_beta(cx, &view, features);
            },
        ),
    )
    .description(description)
}
