//! "About" settings page: version and project links, opened via
//! `cx.open_url`.

use gpui_kit::component::{
    button::Button,
    h_flex, v_flex,
    label::Label,
    setting::{SettingGroup, SettingItem, SettingPage},
    ActiveTheme, Icon,
};
use gpui_kit::*;

use super::SettingsView;

const REPO_URL: &str = "https://github.com/Hankanman/Parley";

pub fn page(cx: &mut Context<SettingsView>) -> SettingPage {
    let _ = cx;

    SettingPage::new("About")
        .icon(Icon::new(gpui_kit::assets::IconName::Info))
        .group(
            SettingGroup::new().items(vec![
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
