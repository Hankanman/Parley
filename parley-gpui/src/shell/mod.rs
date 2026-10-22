//! Top-level window content: title bar, sidebar navigation, and the routed
//! page. Each page is its own entity in `views/`; the shell owns them so
//! their state (e.g. the live transcript) survives navigating away.

mod meeting_list;

use std::time::Duration;

use chrono::Local;
use gpui_kit::component::{
    ActiveTheme, IconName, TitleBar, WindowExt as _, h_flex, v_flex,
    button::{Button, ButtonVariants as _},
    dialog::DialogFooter,
    input::{Input, InputEvent, InputState},
    sidebar::{Sidebar, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem},
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use parley_core::database::models::MeetingModel;
use parley_core::database::repositories::meeting::MeetingsRepository;

use crate::app_state::AppServices;
use crate::runtime::Io;
use crate::views::{
    action_items::ActionItemsView, ask::AskView, brief::BriefView, import, meeting::MeetingView, recording::RecordingView,
    settings::SettingsView, speakers::SpeakersView,
};
use meeting_list::{ContentMatch, MeetingRow};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Recording,
    Meeting(String),
    Settings,
    ActionItems,
    Speakers,
    /// "Ask Parley" across all meetings.
    Ask,
    /// Pre-meeting brief for a calendar event (by event id).
    Brief(String),
}

/// Lets any view switch pages without holding the shell: `navigate(Route::Meeting(id), cx)`.
struct Navigator(WeakEntity<AppShell>);

impl Global for Navigator {}

///
/// Deferred to the end of the current effect cycle: callers are usually
/// inside a view's own update (a core-event handler), and switching routes
/// updates the pages being left and shown — possibly that same view, which
/// GPUI would reject as a double lease.
pub fn navigate(route: Route, cx: &mut App) {
    let Some(shell) = cx.try_global::<Navigator>().map(|n| n.0.clone()) else {
        return;
    };
    cx.defer(move |cx| {
        let _ = shell.update(cx, |shell, cx| shell.navigate(route, cx));
    });
}

/// Open meeting `id` and, once its transcript has loaded, scroll to
/// `seconds` (used by cross-meeting "Ask Parley" citations).
pub fn open_meeting_at(id: String, seconds: Option<f64>, cx: &mut App) {
    let Some(shell) = cx.try_global::<Navigator>().map(|n| n.0.clone()) else {
        return;
    };
    // Deferred for the same reason as `navigate`.
    cx.defer(move |cx| {
        let _ = shell.update(cx, |shell, cx| {
            shell.navigate(Route::Meeting(id), cx);
            if let Some(seconds) = seconds {
                shell.meeting.update(cx, |view, cx| view.jump_when_loaded(seconds, cx));
            }
        });
    });
}

/// Lets any view ask the sidebar to reload its meeting list — e.g. after a
/// title rename, a delete, or a summary generation finishing (none of those
/// have a dedicated core event; recording/import/retranscription events are
/// consumed directly from `core_events`, see [`AppShell::new`]).
struct Refresher(WeakEntity<AppShell>);

impl Global for Refresher {}

pub fn refresh_meetings(cx: &mut App) {
    let Some(shell) = cx.try_global::<Refresher>().map(|n| n.0.clone()) else {
        return;
    };
    let _ = shell.update(cx, |shell, cx| shell.load_meetings(cx));
}

/// Core events that mean the meeting list may be stale and should be
/// reloaded. Mirrors what the React sidebar listens to
/// (`components/Sidebar/SidebarProvider.tsx`).
const REFRESH_EVENTS: &[&str] = &[
    "recording-started",
    "recording-stopped",
    "meeting-refined",
    "import-complete",
    "retranscription-complete",
];

/// How many ranked segment hits the sidebar's transcript search fetches.
/// They're reduced to one row per meeting, so this bounds how many distinct
/// meetings can show up, not how many rows render.
const CONTENT_SEARCH_LIMIT: u32 = 200;

pub struct AppShell {
    route: Route,
    recording: Entity<RecordingView>,
    meeting: Entity<MeetingView>,
    settings: Entity<SettingsView>,
    action_items: Entity<ActionItemsView>,
    speakers: Entity<SpeakersView>,
    ask: Entity<AskView>,
    brief: Entity<BriefView>,
    /// Calendar events starting soon, shown as the sidebar's "Upcoming"
    /// group (each opens its brief). Refreshed every few minutes.
    upcoming: Vec<UpcomingEvent>,
    _upcoming_refresh: Task<()>,
    meetings: Vec<MeetingRow>,
    meetings_loading: bool,
    /// (groups, meeting rows) the sidebar was last rendered with. gpui-kit's
    /// `Sidebar` lays its groups out in a `list()` and resets that list's
    /// measurements *during* render, so the frame that first shows a changed
    /// list paints stale heights — the meetings stayed invisible until the
    /// next input event. When the shape changes we ask for one more frame.
    rendered_sidebar_shape: (usize, usize),
    search: Entity<InputState>,
    /// Transcript-content search hits for the current query (debounced —
    /// see [`Self::search_transcript_content`]), shown below the
    /// title-filtered groups, best match first. Backed by the FTS5 index
    /// (`parley_core::database::search`).
    content_matches: Vec<ContentMatch>,
    content_search_loading: bool,
    _content_search_debounce: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AppShell {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.set_global(Navigator(cx.entity().downgrade()));
        cx.set_global(Refresher(cx.entity().downgrade()));

        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search meetings…"));
        let mut subscriptions = vec![cx.subscribe(&search, |this, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.search_transcript_content(cx);
                cx.notify();
            }
        })];

        let core_events = AppServices::global(cx).core_events.clone();
        subscriptions.push(cx.subscribe(&core_events, |this, _, event, cx| {
            if REFRESH_EVENTS.contains(&event.name.as_str()) {
                this.load_meetings(cx);
            }
        }));

        let mut shell = Self {
            route: Route::Recording,
            recording: cx.new(|cx| RecordingView::new(window, cx)),
            meeting: cx.new(|cx| MeetingView::new(window, cx)),
            settings: cx.new(|cx| SettingsView::new(window, cx)),
            action_items: cx.new(|cx| ActionItemsView::new(window, cx)),
            speakers: cx.new(|cx| SpeakersView::new(window, cx)),
            ask: cx.new(|cx| AskView::new(window, cx)),
            brief: cx.new(|cx| BriefView::new(window, cx)),
            upcoming: Vec::new(),
            _upcoming_refresh: Task::ready(()),
            meetings: Vec::new(),
            meetings_loading: false,
            rendered_sidebar_shape: (0, 0),
            search,
            content_matches: Vec::new(),
            content_search_loading: false,
            _content_search_debounce: None,
            _subscriptions: subscriptions,
        };
        shell.load_meetings(cx);
        shell._upcoming_refresh = shell.start_upcoming_refresh(cx);
        shell
    }

    pub fn navigate(&mut self, route: Route, cx: &mut Context<Self>) {
        if let Route::Meeting(id) = &route {
            let id = id.clone();
            self.meeting.update(cx, |view, cx| view.load(id, cx));
        }
        if route == Route::Ask {
            self.ask.update(cx, |view, cx| view.on_show(cx));
        }
        if let Route::Brief(event_id) = &route {
            let event_id = event_id.clone();
            self.brief.update(cx, |view, cx| view.load(event_id, cx));
        }
        // Leaving the Speakers page while self-voice enrollment is recording
        // would otherwise leave the microphone open indefinitely — cancel it,
        // same as clicking "Cancel" on the enrollment panel. Checked against
        // the *current* route (before it's overwritten below) so navigating
        // Speakers -> Speakers (a no-op route) doesn't cancel anything.
        if matches!(self.route, Route::Speakers) && route != Route::Speakers {
            self.speakers.update(cx, |view, cx| view.on_leave(cx));
        }
        // The Recording page's level meter holds the mic open, so it only
        // runs while that page is on screen.
        if self.route == Route::Recording && route != Route::Recording {
            self.recording.update(cx, |view, cx| view.on_leave(cx));
        } else if self.route != Route::Recording && route == Route::Recording {
            self.recording.update(cx, |view, cx| view.on_show(cx));
        }
        self.route = route;
        cx.notify();
    }

    /// Navigation entry point for in-shell UI (sidebar clicks): asks the
    /// currently-shown page whether it's OK to leave before switching routes
    /// — today only the Meeting page (an unsaved summary edit) cares, via
    /// [`crate::views::meeting::MeetingView::has_unsaved_summary_edits`].
    /// The free `navigate(route, cx)` function stays ungated for callers
    /// without a `Window` (core-event handlers, post-delete redirects, …).
    fn attempt_navigate(&mut self, route: Route, window: &mut Window, cx: &mut Context<Self>) {
        if route == self.route {
            return;
        }
        let leaving_meeting_with_unsaved_edits =
            matches!(self.route, Route::Meeting(_)) && self.meeting.read(cx).has_unsaved_summary_edits(cx);
        if leaving_meeting_with_unsaved_edits {
            self.confirm_leave_meeting(route, window, cx);
        } else {
            self.navigate(route, cx);
        }
    }

    /// "You have unsaved summary edits" dialog with Save / Discard / Cancel,
    /// shown by [`Self::attempt_navigate`].
    fn confirm_leave_meeting(&mut self, route: Route, window: &mut Window, cx: &mut Context<Self>) {
        let shell = cx.entity();
        let meeting = self.meeting.clone();

        window.open_alert_dialog(cx, move |alert, _, _| {
            let shell_discard = shell.clone();
            let meeting_discard = meeting.clone();
            let route_discard = route.clone();
            let shell_save = shell.clone();
            let meeting_save = meeting.clone();
            let route_save = route.clone();

            alert
                .title("Unsaved summary edits")
                .description("You have unsaved changes to this meeting's summary. Save them before leaving?")
                .footer(
                    DialogFooter::new()
                        .justify_center()
                        .child(
                            Button::new("leave-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("leave-discard")
                                .danger()
                                .label("Discard")
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    meeting_discard.update(cx, |m, cx| m.discard_summary_edits(cx));
                                    shell_discard.update(cx, |shell, cx| shell.navigate(route_discard.clone(), cx));
                                }),
                        )
                        .child(
                            Button::new("leave-save")
                                .primary()
                                .label("Save")
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    meeting_save.update(cx, |m, cx| m.save_summary_and_leave(window, cx));
                                    shell_save.update(cx, |shell, cx| shell.navigate(route_save.clone(), cx));
                                }),
                        ),
                )
        });
    }

    /// (Re)load the meeting list from the database, replacing what's shown.
    /// Cheap enough to call on every relevant core event / mutation — the
    /// sidebar only ever holds id/title/created_at, not transcripts.
    fn load_meetings(&mut self, cx: &mut Context<Self>) {
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        self.meetings_loading = true;
        cx.notify();

        let io = Io::global(cx);
        cx.spawn(async move |this, cx| {
            let result = io
                .spawn(async move { MeetingsRepository::get_meetings(&pool).await })
                .await;

            let rows: Vec<MeetingRow> = match result {
                Ok(Ok(models)) => {
                    log::info!("Loaded {} meeting(s) for the sidebar", models.len());
                    models.into_iter().map(meeting_row).collect()
                }
                Ok(Err(e)) => {
                    log::error!("Failed to load meetings: {e}");
                    Vec::new()
                }
                Err(e) => {
                    log::error!("Meeting list load task panicked: {e}");
                    Vec::new()
                }
            };

            let _ = this.update(cx, |this, cx| {
                this.meetings = rows;
                this.meetings_loading = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Debounced transcript-content search: 300ms after the last keystroke,
    /// runs a ranked FTS5 search over transcript text for the current query
    /// (`parley_core::database::search::search_transcripts`), and stashes the
    /// results as [`ContentMatch`]es for `render` to show below the title-filtered
    /// groups. Superseded debounces are dropped by replacing
    /// `_content_search_debounce`, which cancels the previous task.
    fn search_transcript_content(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).value().to_string();
        if query.trim().is_empty() {
            self.content_matches.clear();
            self.content_search_loading = false;
            self._content_search_debounce = None;
            cx.notify();
            return;
        }
        let Some(pool) = AppServices::global(cx).pool() else {
            return;
        };
        self.content_search_loading = true;
        let io = Io::global(cx);
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(300)).await;

            // Bail if the query changed while we were waiting out the debounce.
            let still_current = this
                .update(cx, |this, cx| this.search.read(cx).value().to_string() == query)
                .unwrap_or(false);
            if !still_current {
                return;
            }

            let query_for_search = query.clone();
            let result = io
                .spawn(async move {
                    parley_core::database::search::search_transcripts(
                        &pool,
                        &query_for_search,
                        CONTENT_SEARCH_LIMIT,
                        parley_core::database::search::Highlight::NONE,
                    )
                    .await
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.content_search_loading = false;
                if this.search.read(cx).value().to_string() != query {
                    return;
                }
                match result {
                    Ok(Ok(rows)) => {
                        // Hits arrive best-first (bm25), so keeping the
                        // first per meeting keeps each meeting's best hit.
                        let rows = rows.into_iter().map(|r| ContentMatch {
                            meeting_id: r.meeting_id,
                            title: r.meeting_title,
                            snippet: r.snippet,
                        });
                        let already_shown: Vec<String> =
                            meeting_list::filter_by_title(&this.meetings, &query).into_iter().map(|m| m.id.clone()).collect();
                        this.content_matches = meeting_list::build_content_matches(rows, &already_shown);
                    }
                    Ok(Err(e)) => {
                        log::error!("Transcript content search failed: {e}");
                        this.content_matches.clear();
                    }
                    Err(e) => {
                        log::error!("Transcript content search task panicked: {e}");
                        this.content_matches.clear();
                    }
                }
                cx.notify();
            });
        });
        self._content_search_debounce = Some(task);
    }

    fn nav_item(
        &self,
        label: &'static str,
        icon: impl Into<gpui_kit::component::Icon>,
        route: Route,
        cx: &mut Context<Self>,
    ) -> SidebarMenuItem {
        let this = cx.entity();
        SidebarMenuItem::new(label)
            .icon(icon)
            .active(self.route == route)
            .on_click(move |_, window, cx| {
                let route = route.clone();
                this.update(cx, |shell, cx| shell.attempt_navigate(route, window, cx));
            })
    }

    fn meeting_item(&self, meeting: &MeetingRow, cx: &mut Context<Self>) -> SidebarMenuItem {
        let this = cx.entity();
        let id = meeting.id.clone();
        let active = matches!(&self.route, Route::Meeting(active_id) if *active_id == meeting.id);
        let label = if meeting.title.trim().is_empty() {
            "Untitled meeting".to_string()
        } else {
            meeting.title.clone()
        };
        SidebarMenuItem::new(label)
            .icon(IconName::FileText)
            .active(active)
            .on_click(move |_, window, cx| {
                let id = id.clone();
                this.update(cx, |shell, cx| shell.attempt_navigate(Route::Meeting(id), window, cx));
            })
    }
}

impl AppShell {
    fn content_match_item(&self, hit: &ContentMatch, cx: &mut Context<Self>) -> SidebarMenuItem {
        let this = cx.entity();
        let id = hit.meeting_id.clone();
        let title = if hit.title.trim().is_empty() { "Untitled meeting".to_string() } else { hit.title.clone() };
        let label = format!("{title} — {}", hit.snippet);
        let active = matches!(&self.route, Route::Meeting(active_id) if *active_id == hit.meeting_id);
        SidebarMenuItem::new(label)
            .icon(IconName::Search)
            .active(active)
            .on_click(move |_, window, cx| {
                let id = id.clone();
                this.update(cx, |shell, cx| shell.attempt_navigate(Route::Meeting(id), window, cx));
            })
    }
}

/// One "Upcoming" sidebar entry.
#[derive(Clone, Debug, PartialEq)]
struct UpcomingEvent {
    id: String,
    title: String,
    start: chrono::DateTime<chrono::Utc>,
}

/// How far ahead the "Upcoming" group looks, and how many it lists.
const UPCOMING_HORIZON_HOURS: i64 = 12;
const UPCOMING_MAX: usize = 3;
/// An event stays listed this long after it starts (you may open its brief
/// while joining late).
const UPCOMING_GRACE_MINUTES: i64 = 10;
const UPCOMING_REFRESH: Duration = Duration::from_secs(5 * 60);

/// Pick the "Upcoming" entries from `events` (any order) at `now`.
fn upcoming_from(
    events: Vec<parley_core::calendar::models::CalendarEvent>,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<UpcomingEvent> {
    let earliest = now - chrono::Duration::minutes(UPCOMING_GRACE_MINUTES);
    let mut upcoming: Vec<UpcomingEvent> = events
        .into_iter()
        .filter(|e| !e.is_all_day && e.start_at >= earliest)
        .map(|e| UpcomingEvent {
            id: e.id,
            title: e
                .summary
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Untitled event".to_string()),
            start: e.start_at,
        })
        .collect();
    upcoming.sort_by_key(|e| e.start);
    upcoming.truncate(UPCOMING_MAX);
    upcoming
}

impl AppShell {
    /// Reload the "Upcoming" group now and every [`UPCOMING_REFRESH`].
    fn start_upcoming_refresh(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let io = Io::global(cx);
        cx.spawn(async move |this, cx| loop {
            let pool = cx.update(|cx| AppServices::global(cx).pool());
            if let Some(pool) = pool {
                let now = chrono::Utc::now();
                let result = io
                    .spawn(async move {
                        parley_core::calendar::repository::CalendarRepository::list_events_in_range(
                            &pool,
                            now,
                            now + chrono::Duration::hours(UPCOMING_HORIZON_HOURS),
                        )
                        .await
                    })
                    .await;
                let upcoming = match result {
                    Ok(Ok(events)) => upcoming_from(events, now),
                    Ok(Err(e)) => {
                        log::warn!("failed to load upcoming events: {e}");
                        Vec::new()
                    }
                    Err(e) => {
                        log::warn!("upcoming events task failed: {e}");
                        Vec::new()
                    }
                };
                let alive = this.update(cx, |this, cx| {
                    if this.upcoming != upcoming {
                        this.upcoming = upcoming;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
            cx.background_executor().timer(UPCOMING_REFRESH).await;
        })
    }

    fn upcoming_item(&self, event: &UpcomingEvent, cx: &mut Context<Self>) -> SidebarMenuItem {
        let this = cx.entity();
        let id = event.id.clone();
        let active = matches!(&self.route, Route::Brief(active_id) if *active_id == event.id);
        let local = event.start.with_timezone(&Local);
        let when = if local.date_naive() == Local::now().date_naive() {
            local.format("%H:%M").to_string()
        } else {
            local.format("%a %H:%M").to_string()
        };
        SidebarMenuItem::new(format!("{when} {}", event.title))
            .icon(gpui_kit::assets::IconName::CalendarClock)
            .active(active)
            .on_click(move |_, window, cx| {
                let id = id.clone();
                this.update(cx, |shell, cx| shell.attempt_navigate(Route::Brief(id), window, cx));
            })
    }
}

fn meeting_row(model: MeetingModel) -> MeetingRow {
    MeetingRow {
        id: model.id,
        title: model.title,
        created_at: model.created_at.0,
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page: AnyView = match &self.route {
            Route::Recording => self.recording.clone().into(),
            Route::Meeting(_) => self.meeting.clone().into(),
            Route::Settings => self.settings.clone().into(),
            Route::ActionItems => self.action_items.clone().into(),
            Route::Speakers => self.speakers.clone().into(),
            Route::Ask => self.ask.clone().into(),
            Route::Brief(_) => self.brief.clone().into(),
        };

        let query = self.search.read(cx).value().to_string();
        let filtered: Vec<MeetingRow> = meeting_list::filter_by_title(&self.meetings, &query)
            .into_iter()
            .cloned()
            .collect();
        let grouped = meeting_list::group_meetings(Local::now(), &filtered);

        let mut sidebar = Sidebar::new("app-sidebar")
            .header(
                SidebarHeader::new()
                    .child(v_flex().w_full().gap_2().child("Parley").child(
                        Input::new(&self.search).prefix(IconName::Search.view(cx)),
                    )),
            )
            .child(
                SidebarGroup::new("Meetings").child(SidebarMenu::new().children([
                    self.nav_item("Record", IconName::Play, Route::Recording, cx),
                    self.nav_item("Ask Parley", gpui_kit::assets::IconName::Sparkles, Route::Ask, cx),
                    SidebarMenuItem::new("Import audio")
                        .icon(gpui_kit::assets::IconName::Upload)
                        .on_click(|_, window, cx| import::open(window, cx)),
                    self.nav_item("Settings", IconName::Settings, Route::Settings, cx),
                ])),
            )
            .child(
                SidebarGroup::new("Tools").child(SidebarMenu::new().children([
                    self.nav_item("Action items", IconName::CircleCheck, Route::ActionItems, cx),
                    self.nav_item("Speakers", IconName::User, Route::Speakers, cx),
                ])),
            );

        if !self.upcoming.is_empty() {
            let items: Vec<SidebarMenuItem> = self.upcoming.iter().map(|e| self.upcoming_item(e, cx)).collect();
            sidebar = sidebar.child(SidebarGroup::new("Upcoming").child(SidebarMenu::new().children(items)));
        }

        for (label, meetings) in &grouped {
            let items: Vec<SidebarMenuItem> =
                meetings.iter().map(|m| self.meeting_item(m, cx)).collect();
            sidebar = sidebar.child(SidebarGroup::new(*label).child(SidebarMenu::new().children(items)));
        }

        let shown_no_matches = !self.meetings_loading && grouped.is_empty() && !query.trim().is_empty();
        let shown_content_matches =
            !query.trim().is_empty() && (self.content_search_loading || !self.content_matches.is_empty());
        let shape = (
            2 + (!self.upcoming.is_empty()) as usize
                + grouped.len()
                + shown_no_matches as usize
                + shown_content_matches as usize,
            grouped.iter().map(|(_, rows)| rows.len()).sum::<usize>()
                + self.content_matches.len()
                + self.upcoming.len(),
        );
        if shape != self.rendered_sidebar_shape {
            self.rendered_sidebar_shape = shape;
            window.request_animation_frame();
        }

        if shown_no_matches {
            sidebar = sidebar.child(
                SidebarGroup::new("").child(SidebarMenu::new().children([SidebarMenuItem::new(
                    "No matching meetings",
                )
                .disable(true)])),
            );
        }

        if shown_content_matches {
            let items: Vec<SidebarMenuItem> = self.content_matches.iter().map(|m| self.content_match_item(m, cx)).collect();
            let mut group = SidebarGroup::new("In transcripts");
            group = if items.is_empty() {
                group.child(SidebarMenu::new().children([SidebarMenuItem::new("Searching…").disable(true)]))
            } else {
                group.child(SidebarMenu::new().children(items))
            };
            sidebar = sidebar.child(group);
        }

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            // Drag-and-drop an audio file onto the window to import it.
            .on_drop(move |paths: &ExternalPaths, window, cx| {
                if let Some(path) = import::pick_dropped_audio_file(paths.paths()) {
                    import::open_with_file(window, cx, path.clone());
                }
            })
            // Our title bar is only for when the app has to draw its own
            // window frame. When the compositor draws one (server-side
            // decorations), this strip holds nothing the system bar doesn't
            // already show, so skip it rather than stack two title bars.
            //
            // In the client-side case, `on_close_window` matters: without it
            // gpui-kit's X calls `window.remove_window()` directly, bypassing
            // `on_window_should_close`. Routing it through the same policy
            // keeps both close paths identical.
            .when(
                matches!(window.window_decorations(), Decorations::Client { .. }),
                |this| {
                    this.child(
                        TitleBar::new()
                            .on_close_window(|_, window, cx| {
                                if crate::window::close_to_tray_or_quit(cx) {
                                    window.remove_window();
                                }
                            })
                            .child("Parley"),
                    )
                },
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(sidebar)
                    .child(div().flex_1().min_w_0().h_full().child(page)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use parley_core::calendar::models::CalendarEvent;

    fn event(id: &str, minutes: i64, all_day: bool, title: Option<&str>) -> CalendarEvent {
        let now = Utc.with_ymd_and_hms(2026, 10, 16, 9, 0, 0).unwrap();
        CalendarEvent {
            id: id.into(),
            source_id: "s".into(),
            ics_uid: id.into(),
            recurrence_id: None,
            summary: title.map(str::to_string),
            description: None,
            location: None,
            organizer_name: None,
            organizer_email: None,
            start_at: now + chrono::Duration::minutes(minutes),
            end_at: now + chrono::Duration::minutes(minutes + 30),
            is_all_day: all_day,
            attendees: vec![],
        }
    }

    #[std::prelude::v1::test]
    fn upcoming_lists_the_next_few_timed_events() {
        let now = Utc.with_ymd_and_hms(2026, 10, 16, 9, 0, 0).unwrap();
        let picked = upcoming_from(
            vec![
                event("later", 240, false, Some("Later")),
                event("all-day", 30, true, Some("Holiday")),
                event("started-long-ago", -45, false, Some("Old")),
                event("just-started", -5, false, Some("  ")),
                event("soon", 20, false, Some("Sync")),
                event("after", 300, false, Some("After")),
            ],
            now,
        );
        let ids: Vec<&str> = picked.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["just-started", "soon", "later"]);
        assert_eq!(picked[0].title, "Untitled event");
    }
}
