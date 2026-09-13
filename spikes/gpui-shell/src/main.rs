//! `parley-gpui-spike` — a time-boxed spike evaluating a GPUI rewrite of
//! Parley UI. See `spikes/gpui-shell/README.md` for what
//! each check covers and the findings.
//!
//! Standalone crate: NOT a member of the root Cargo workspace (see this
//! crate's `Cargo.toml`, which has its own empty `[workspace]` table).

mod db;
mod recording;
mod summary;
mod tray;

use gpui_kit::assets::Assets;
use gpui_kit::component::{
    ActiveTheme, Icon, IconName, Root, StyledExt as _, Theme, ThemeMode, TitleBar, h_flex, v_flex,
    button::{Button, ButtonVariants},
    sidebar::{
        Sidebar, SidebarCollapsible, SidebarFooter, SidebarGroup, SidebarHeader, SidebarMenu,
        SidebarMenuItem, SidebarToggleButton,
    },
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use recording::RecordingView;
use summary::SummaryView;
use tray::SharedRecordingState;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Shell,
    Recording,
    Summary,
    Tray,
}

struct SpikeShell {
    active_tab: Tab,
    sidebar_collapsed: bool,
    recording: Entity<RecordingView>,
    summary: Entity<SummaryView>,
}

impl SpikeShell {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            active_tab: Tab::Shell,
            sidebar_collapsed: false,
            recording: cx.new(|cx| RecordingView::new(window, cx)),
            summary: cx.new(|cx| SummaryView::new(window, cx)),
        }
    }

}

impl Render for SpikeShell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let icon_collapsed = self.sidebar_collapsed;
        let theme_is_dark = cx.theme().mode.is_dark();
        let this = cx.entity();

        let menu = SidebarMenu::new().children([
            SidebarMenuItem::new("Shell")
                .icon(IconName::LayoutDashboard)
                .active(self.active_tab == Tab::Shell)
                .on_click({
                    let this = this.clone();
                    move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.active_tab = Tab::Shell;
                            cx.notify();
                        });
                    }
                }),
            SidebarMenuItem::new("Recording")
                .icon(IconName::Play)
                .active(self.active_tab == Tab::Recording)
                .on_click({
                    let this = this.clone();
                    move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.active_tab = Tab::Recording;
                            cx.notify();
                        });
                    }
                }),
            SidebarMenuItem::new("Summary")
                .icon(IconName::FileText)
                .active(self.active_tab == Tab::Summary)
                .on_click({
                    let this = this.clone();
                    move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.active_tab = Tab::Summary;
                            cx.notify();
                        });
                    }
                }),
            SidebarMenuItem::new("Tray")
                .icon(IconName::Settings)
                .active(self.active_tab == Tab::Tray)
                .on_click({
                    let this = this.clone();
                    move |_, _, cx| {
                        this.update(cx, |this, cx| {
                            this.active_tab = Tab::Tray;
                            cx.notify();
                        });
                    }
                }),
        ]);

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                TitleBar::new().child(
                    h_flex()
                        .w_full()
                        .pr_2()
                        .justify_between()
                        .items_center()
                        .child(div().text_sm().font_semibold().child("Parley — GPUI spike"))
                        .child(
                            Button::new("theme-toggle")
                                .ghost()
                                .icon(if theme_is_dark {
                                    IconName::Sun
                                } else {
                                    IconName::Moon
                                })
                                .tooltip("Toggle light/dark theme")
                                .on_click(move |_, window, cx| {
                                    let mode = if Theme::global(cx).mode.is_dark() {
                                        ThemeMode::Light
                                    } else {
                                        ThemeMode::Dark
                                    };
                                    Theme::change(mode, Some(window), cx);
                                }),
                        ),
                ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        Sidebar::new("spike-sidebar")
                            .collapsible(SidebarCollapsible::Icon)
                            .collapsed(self.sidebar_collapsed)
                            .w(px(220.))
                            .header(
                                SidebarHeader::new()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .size_8()
                                            .flex_shrink_0()
                                            .rounded(cx.theme().radius)
                                            .bg(cx.theme().sidebar_primary)
                                            .text_color(cx.theme().sidebar_primary_foreground)
                                            .child(Icon::new(IconName::GalleryVerticalEnd)),
                                    )
                                    .when(!icon_collapsed, |this| {
                                        this.child(
                                            v_flex()
                                                .flex_1()
                                                .overflow_hidden()
                                                .child("Parley")
                                                .child(div().text_xs().child("GPUI spike")),
                                        )
                                    }),
                            )
                            .child(SidebarGroup::new("Checks").child(menu))
                            .footer(
                                SidebarFooter::new().child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            SidebarToggleButton::new()
                                                .collapsed(icon_collapsed)
                                                .on_click({
                                                    let this = this.clone();
                                                    move |_, _, cx| {
                                                        this.update(cx, |this, cx| {
                                                            this.sidebar_collapsed =
                                                                !this.sidebar_collapsed;
                                                            cx.notify();
                                                        });
                                                    }
                                                }),
                                        )
                                        .when(!icon_collapsed, |this| {
                                            this.child(div().text_xs().child("Collapse"))
                                        }),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .child(match self.active_tab {
                                Tab::Shell => shell_check(cx).into_any_element(),
                                Tab::Recording => self.recording.clone().into_any_element(),
                                Tab::Summary => self.summary.clone().into_any_element(),
                                Tab::Tray => tray_check(cx).into_any_element(),
                            }),
                    ),
            )
    }
}

fn shell_check(cx: &mut Context<SpikeShell>) -> impl IntoElement {
    v_flex()
        .size_full()
        .gap_3()
        .p_6()
        .child(div().text_lg().font_semibold().child("Check 1: Shell"))
        .child(
            div()
                .max_w(px(560.))
                .text_color(cx.theme().muted_foreground)
                .child(
                    "Client-side-decorated window (WindowDecorations::Client), a gpui-kit \
                     TitleBar + Root + collapsible Sidebar, and a light/dark theme toggle \
                     (top-right of the title bar). This text panel and the sidebar use \
                     cx.theme() tokens, so the toggle re-themes them live.",
                ),
        )
        .child(
            div()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(cx.theme().border)
                .p_4()
                .child(format!(
                    "Current theme mode: {}",
                    if cx.theme().mode.is_dark() { "dark" } else { "light" }
                )),
        )
}

fn tray_check(cx: &mut Context<SpikeShell>) -> impl IntoElement {
    let recording = cx
        .try_global::<SharedRecordingState>()
        .map(|s| s.is_recording())
        .unwrap_or(false);

    v_flex()
        .size_full()
        .gap_3()
        .p_6()
        .child(div().text_lg().font_semibold().child("Check 4: Tray"))
        .child(
            div()
                .max_w(px(560.))
                .text_color(cx.theme().muted_foreground)
                .child(
                    "A gpui-tray StatusNotifierItem is running with Start/Stop recording, \
                     Show window, and Quit. This panel mirrors the state its menu toggles — \
                     use your desktop's tray/status area to drive it and watch this update.",
                ),
        )
        .child(
            div()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(cx.theme().border)
                .p_4()
                .child(format!(
                    "Recording (via tray): {}",
                    if recording { "ON" } else { "off" }
                )),
        )
        .child(
            h_flex().gap_2().child(
                Button::new("toggle-recording-from-ui")
                    .primary()
                    .label(if recording {
                        "Stop recording (UI)"
                    } else {
                        "Start recording (UI)"
                    })
                    .on_click(|_, _, cx| {
                        if let Some(state) = cx.try_global::<SharedRecordingState>() {
                            state.set(!state.is_recording());
                            tray::refresh(cx);
                        }
                    }),
            ),
        )
}

/// Round-trip check for `zorite-editor`: load the bundled fixture into an
/// `EditorState`, read its text back out, and report any diff. Zorite treats
/// markdown source as its own document model (it doesn't parse to an AST and
/// re-serialize), so a clean run means the source underwent no lossy
/// transformation for nested task lists / tables / etc.
fn run_roundtrip_check(window: &mut Window, cx: &mut App) -> bool {
    let fixture = include_str!("../fixtures/summary.md");
    let editor = cx.new(|cx| zorite_editor::EditorState::new(window, cx).with_text(fixture));
    let roundtripped = editor.read(cx).value().to_string();

    if roundtripped == fixture {
        println!("[roundtrip] OK: {} bytes, identical after EditorState round-trip", fixture.len());
        true
    } else {
        println!("[roundtrip] MISMATCH");
        println!("  fixture:      {} bytes", fixture.len());
        println!("  roundtripped: {} bytes", roundtripped.len());
        for (i, (a, b)) in fixture.lines().zip(roundtripped.lines()).enumerate() {
            if a != b {
                println!("  line {i}:");
                println!("    fixture:      {a:?}");
                println!("    roundtripped: {b:?}");
            }
        }
        if fixture.lines().count() != roundtripped.lines().count() {
            println!(
                "  line count differs: fixture={} roundtripped={}",
                fixture.lines().count(),
                roundtripped.lines().count()
            );
        }
        false
    }
}

fn main() {
    env_logger::init();

    let roundtrip_only = std::env::args().any(|a| a == "--roundtrip");

    let app = gpui_kit::application().with_assets(Assets);

    app.run(move |cx| {
        gpui_kit::init(cx);
        zorite_editor::bind_keys(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        cx.set_global(SharedRecordingState::new());

        let mut window_options = TitleBar::window_options();
        window_options.window_bounds = Some(WindowBounds::centered(size(px(1100.), px(720.)), cx));
        window_options.window_decorations = Some(WindowDecorations::Client);

        cx.spawn(async move |cx| {
            let window = cx
                .open_window(window_options, |window, cx| {
                    let view = cx.new(|cx| SpikeShell::new(window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect("failed to open window");

            if roundtrip_only {
                let ok = window
                    .update(cx, |_, window, cx| run_roundtrip_check(window, cx))
                    .unwrap_or(false);
                cx.update(|cx| cx.quit());
                std::process::exit(if ok { 0 } else { 1 });
            }

            cx.update(|cx| {
                let install = tray::install(cx, |cx| {
                    // "Show window" — gpui-tray on Linux has no direct
                    // window-activate API from here in this spike; log the
                    // intent instead of wiring OS-specific focus calls.
                    log::info!("show window requested (no-op in this spike: single window, already visible)");
                    let _ = cx;
                });
                if let Err(err) = install {
                    log::warn!("gpui-tray unavailable: {err:#} (continuing without a tray icon)");
                }
            });
        })
        .detach();
    });
}
