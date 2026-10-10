//! "Export folder" group on the Integrations settings page: pick a folder
//! (e.g. inside an Obsidian vault) and toggle writing a front-mattered
//! Markdown note there after each summary. Backed by
//! `parley_core::export::folder_sync`'s settings row.
//!
//! Kept out of `SettingsCache` on purpose: it's its own small global,
//! loaded the first time the page renders, so this feature doesn't touch
//! the shared settings state.

use gpui_kit::component::{
    ActiveTheme,
    button::{Button, ButtonVariants as _},
    h_flex,
    setting::{SettingField, SettingGroup, SettingItem},
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use parley_core::export::folder_sync::{self, ExportFolderSettings};

use super::SettingsView;
use crate::app_state::AppServices;
use crate::runtime::Io;

#[derive(Default)]
struct ExportFolderCache {
    settings: ExportFolderSettings,
}

impl Global for ExportFolderCache {}

pub fn group(view: &Entity<SettingsView>, cx: &mut Context<SettingsView>) -> SettingGroup {
    if !cx.has_global::<ExportFolderCache>() {
        cx.set_global(ExportFolderCache::default());
        load(view.clone(), cx);
    }

    SettingGroup::new()
        .title("Export folder")
        .description(
            "Write each meeting as a Markdown note with YAML front-matter (title, date, duration, \
             attendees, tags, meeting id) into a folder — for example a folder inside your \
             Obsidian vault. Notes are named \"YYYY-MM-DD Title.md\"; re-summarising updates the \
             same note, and existing notes for other meetings are never overwritten.",
        )
        .items(vec![
            SettingItem::new(
                "Write a note after each summary",
                SettingField::switch(
                    |cx: &App| cx.global::<ExportFolderCache>().settings.enabled,
                    {
                        let view = view.clone();
                        move |val: bool, cx: &mut App| {
                            update(cx, &view, |s| s.enabled = val);
                        }
                    },
                ),
            )
            .description("Also refreshed when you save an edited summary.")
            .keywords(["obsidian", "export", "markdown", "vault", "notes"]),
            SettingItem::new(
                "Folder",
                SettingField::render({
                    let view = view.clone();
                    move |_options, _window, cx| {
                        let folder = cx.global::<ExportFolderCache>().settings.folder.clone();
                        let label = folder
                            .clone()
                            .unwrap_or_else(|| "No folder chosen".to_string());
                        let muted = cx.theme().muted_foreground;
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_sm()
                                    .when(folder.is_none(), |d| d.text_color(muted))
                                    .child(label),
                            )
                            .child(
                                Button::new("choose-export-folder")
                                    .outline()
                                    .label("Choose…")
                                    .on_click({
                                        let view = view.clone();
                                        move |_, _, cx| choose_folder(view.clone(), cx)
                                    }),
                            )
                            .when_some(folder, |row, folder| {
                                row.child(
                                    Button::new("open-export-folder")
                                        .ghost()
                                        .label("Open")
                                        .on_click(move |_, _, _cx| {
                                            let _ = std::process::Command::new("xdg-open")
                                                .arg(&folder)
                                                .spawn();
                                        }),
                                )
                            })
                    }
                }),
            )
            .keywords(["obsidian", "export folder", "vault"]),
        ])
}

fn load(view: Entity<SettingsView>, cx: &mut App) {
    let Some(pool) = AppServices::global(cx).pool() else {
        return;
    };
    let io = Io::global(cx);
    cx.spawn(async move |cx| {
        let loaded = io
            .spawn(async move { folder_sync::load_settings(&pool).await })
            .await;
        let _ = cx.update(|cx| {
            if let Ok(settings) = loaded {
                cx.global_mut::<ExportFolderCache>().settings = settings;
            }
            let _ = view.update(cx, |_, cx| cx.notify());
        });
    })
    .detach();
}

/// Mutate the cached settings, redraw, and persist the whole struct.
fn update(
    cx: &mut App,
    view: &Entity<SettingsView>,
    mutate: impl FnOnce(&mut ExportFolderSettings),
) {
    let settings = {
        let cache = cx.global_mut::<ExportFolderCache>();
        mutate(&mut cache.settings);
        cache.settings.clone()
    };
    let _ = view.update(cx, |_, cx| cx.notify());

    let Some(pool) = AppServices::global(cx).pool() else {
        log::warn!("settings: no DB pool yet, export folder settings not saved");
        return;
    };
    Io::global(cx).spawn(async move {
        if let Err(e) = folder_sync::save_settings(&pool, &settings).await {
            log::warn!("settings: {e}");
        }
    });
}

fn choose_folder(view: Entity<SettingsView>, cx: &mut App) {
    let rx = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Choose export folder".into()),
    });
    cx.spawn(async move |cx| {
        if let Ok(Ok(Some(mut paths))) = rx.await {
            if let Some(path) = paths.drain(..).next() {
                let _ = cx.update(|cx| {
                    update(cx, &view, |s| {
                        s.folder = Some(path.to_string_lossy().to_string());
                        // Picking a folder is a clear signal of intent.
                        s.enabled = true;
                    });
                });
            }
        }
    })
    .detach();
}
