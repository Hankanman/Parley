//! Choosing which calendar event a new recording belongs to (issue #107).
//!
//! When a recording starts, the meeting is linked to the calendar event in
//! progress. Taking "whatever is on right now" links calls to recurring
//! blockers ("Lunch", "Focus time") and to all-day or cancelled entries,
//! which then pollute pre-meeting briefs and series filters. This module
//! decides instead:
//!
//! 1. **Exclusions** ([`exclusion`]): all-day events, cancelled events
//!    (`STATUS:CANCELLED`, or Outlook's "Canceled: …" summary), events
//!    marked free (`TRANSP:TRANSPARENT`, `X-MICROSOFT-CDO-BUSYSTATUS:FREE`),
//!    events the user declined, and events with nobody else on them (no
//!    attendees, only the organizer, or only the user).
//! 2. **Ranking** ([`pick_event`]) of what's left, by how close the event's
//!    start is to the recording start, how many other people are on it, and
//!    whether its conferencing link matches the meeting app detected (#85).
//!
//! The user is identified by the email on the self voice profile and by a
//! calendar-owner heuristic ([`infer_owner_emails`]): ICS feeds list their
//! owner as an attendee or organizer of nearly every event that has
//! attendees. Both are optional; without either, the "only the user"
//! check falls back to "only the organizer".
//!
//! [`pick_event`] and its helpers are pure; [`find_event_for_recording`]
//! loads the inputs from the database.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use sqlx::SqlitePool;

use super::models::{Attendee, CalendarEvent, CalendarEventRow};
use crate::audio::meeting_detect::MeetingAppEvent;
use crate::database::repositories::voice_profile::VoiceProfilesRepository;
use crate::speaker_diarization::SELF_SPEAKER_LABEL;

/// A recording may start this long before its event does (joining early).
pub const EARLY_START: Duration = Duration::minutes(5);
/// ... or this long after the event's scheduled end (an overrunning call).
pub const LATE_END: Duration = Duration::minutes(15);
/// Start-time distance at which the closeness score bottoms out.
const CLOSENESS_HORIZON_MINUTES: f64 = 60.0;
/// Attendee count at which the attendee score saturates.
const ATTENDEE_SATURATION: usize = 8;

const WEIGHT_CLOSENESS: f64 = 2.0;
const WEIGHT_ATTENDEES: f64 = 1.0;
const WEIGHT_APP_MATCH: f64 = 1.5;
const WEIGHT_BROWSER_MATCH: f64 = 0.75;

/// A calendar event considered for auto-linking, with the raw VEVENT text
/// that carries the status/transparency flags [`CalendarEvent`] drops.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub event: CalendarEvent,
    pub raw_ics: Option<String>,
}

impl From<CalendarEvent> for Candidate {
    fn from(event: CalendarEvent) -> Self {
        Candidate { event, raw_ics: None }
    }
}

/// Who the user is and what call (if any) is live.
#[derive(Debug, Clone, Default)]
pub struct AutoLinkContext {
    /// The user's own addresses, lowercased.
    pub self_emails: HashSet<String>,
    /// The user's display name, lowercased (from the self voice profile,
    /// unless it's still the default "Me").
    pub self_name: Option<String>,
    /// The meeting app detected when the recording started.
    pub app: Option<MeetingAppEvent>,
}

impl AutoLinkContext {
    fn is_self(&self, attendee_key: &str) -> bool {
        self.self_emails.contains(attendee_key) || self.self_name.as_deref() == Some(attendee_key)
    }
}

/// Why an event can never be auto-linked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusion {
    AllDay,
    Cancelled,
    /// Marked free / transparent: a blocker, not a meeting.
    Free,
    /// The user declined it.
    Declined,
    /// Nobody on it but the organizer and/or the user.
    NoOtherAttendees,
    /// Doesn't overlap the recording start (with the early/late grace).
    OutOfWindow,
}

/// Lowercased identity for an attendee: email if present, else name.
fn key(email: Option<&str>, name: Option<&str>) -> Option<String> {
    email
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| name.map(str::trim).filter(|s| !s.is_empty()))
        .map(str::to_lowercase)
}

fn attendee_key(a: &Attendee) -> Option<String> {
    key(a.email.as_deref(), a.name.as_deref())
}

/// The unfolded `NAME:value` properties of a VEVENT, name uppercased and
/// parameters dropped.
fn ics_props(raw: &str) -> impl Iterator<Item = (String, &str)> {
    raw.lines().filter_map(|line| {
        let line = line.trim_end_matches('\r');
        let colon = line.find(':')?;
        let (head, value) = (&line[..colon], &line[colon + 1..]);
        let name = head.split(';').next().unwrap_or(head).trim().to_ascii_uppercase();
        Some((name, value.trim()))
    })
}

fn is_cancelled(c: &Candidate) -> bool {
    let by_status = c.raw_ics.as_deref().is_some_and(|raw| {
        ics_props(raw).any(|(n, v)| n == "STATUS" && v.eq_ignore_ascii_case("CANCELLED"))
    });
    let summary = c.event.summary.as_deref().unwrap_or_default().trim_start().to_lowercase();
    by_status || summary.starts_with("canceled:") || summary.starts_with("cancelled:")
}

fn is_free(c: &Candidate) -> bool {
    c.raw_ics.as_deref().is_some_and(|raw| {
        ics_props(raw).any(|(n, v)| match n.as_str() {
            "TRANSP" => v.eq_ignore_ascii_case("TRANSPARENT"),
            "X-MICROSOFT-CDO-BUSYSTATUS" | "X-MICROSOFT-CDO-INTENDEDSTATUS" => v.eq_ignore_ascii_case("FREE"),
            _ => false,
        })
    })
}

fn self_declined(event: &CalendarEvent, ctx: &AutoLinkContext) -> bool {
    event.attendees.iter().any(|a| {
        attendee_key(a).is_some_and(|k| ctx.is_self(&k))
            && a.status.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("DECLINED"))
    })
}

/// How many people other than the user are on the event, counting the
/// organizer. Zero when nobody was invited besides the organizer (a
/// personal blocker, even when we don't know who the user is).
pub fn other_attendee_count(event: &CalendarEvent, ctx: &AutoLinkContext) -> usize {
    let organizer = key(event.organizer_email.as_deref(), event.organizer_name.as_deref());
    let mut invited: HashSet<String> = event
        .attendees
        .iter()
        .filter(|a| !a.role.as_deref().is_some_and(|r| r.eq_ignore_ascii_case("NON-PARTICIPANT")))
        .filter_map(attendee_key)
        .collect();
    if let Some(org) = &organizer {
        invited.remove(org);
    }
    if invited.is_empty() {
        return 0;
    }
    invited.extend(organizer);
    invited.retain(|k| !ctx.is_self(k));
    invited.len()
}

/// Why `candidate` can't be auto-linked to a recording started at `at`, or
/// `None` when it's eligible.
pub fn exclusion(candidate: &Candidate, at: DateTime<Utc>, ctx: &AutoLinkContext) -> Option<Exclusion> {
    let ev = &candidate.event;
    if ev.start_at > at + EARLY_START || ev.end_at < at - LATE_END {
        return Some(Exclusion::OutOfWindow);
    }
    if ev.is_all_day {
        return Some(Exclusion::AllDay);
    }
    if is_cancelled(candidate) {
        return Some(Exclusion::Cancelled);
    }
    if is_free(candidate) {
        return Some(Exclusion::Free);
    }
    if self_declined(ev, ctx) {
        return Some(Exclusion::Declined);
    }
    if other_attendee_count(ev, ctx) == 0 {
        return Some(Exclusion::NoOtherAttendees);
    }
    None
}

/// Conferencing-link markers for a detected desktop app id.
fn app_markers(app_id: &str) -> &'static [&'static str] {
    match app_id {
        "teams" => &["teams.microsoft.com", "teams.live.com"],
        "zoom" => &["zoom.us", "zoomgov.com"],
        "webex" => &["webex.com"],
        "jitsi" => &["meet.jit.si", "jitsi"],
        "slack" => &["slack.com"],
        "discord" => &["discord.gg", "discord.com"],
        _ => &[],
    }
}

/// Links any browser call could be: Meet is browser-only, and Teams / Zoom /
/// Webex all have web clients.
const WEB_MEETING_MARKERS: &[&str] =
    &["meet.google.com", "teams.microsoft.com", "teams.live.com", "zoom.us", "webex.com", "meet.jit.si", "whereby.com"];

/// How strongly the event's conferencing link matches the detected app:
/// the weight to add, or 0.
fn app_match(candidate: &Candidate, app: Option<&MeetingAppEvent>) -> f64 {
    let Some(app) = app else { return 0.0 };
    let ev = &candidate.event;
    let haystack = [ev.location.as_deref(), ev.description.as_deref(), candidate.raw_ics.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    if app.is_browser {
        if WEB_MEETING_MARKERS.iter().any(|m| haystack.contains(m)) {
            return WEIGHT_BROWSER_MATCH;
        }
        return 0.0;
    }
    if app_markers(&app.app_id).iter().any(|m| haystack.contains(m)) {
        WEIGHT_APP_MATCH
    } else {
        0.0
    }
}

/// Ranking score of an eligible candidate; higher is better.
pub fn score(candidate: &Candidate, at: DateTime<Utc>, ctx: &AutoLinkContext) -> f64 {
    let ev = &candidate.event;
    let minutes_off = (ev.start_at - at).num_seconds().abs() as f64 / 60.0;
    let closeness = 1.0 - (minutes_off / CLOSENESS_HORIZON_MINUTES).min(1.0);
    let others = other_attendee_count(ev, ctx).min(ATTENDEE_SATURATION) as f64 / ATTENDEE_SATURATION as f64;
    WEIGHT_CLOSENESS * closeness + WEIGHT_ATTENDEES * others + app_match(candidate, ctx.app.as_ref())
}

/// The best event to link a recording started at `at` to, or `None` when
/// no candidate is eligible. Ties go to the earlier start, then the id, so
/// the choice is deterministic.
pub fn pick_event<'a>(candidates: &'a [Candidate], at: DateTime<Utc>, ctx: &AutoLinkContext) -> Option<&'a Candidate> {
    candidates
        .iter()
        .filter(|c| exclusion(c, at, ctx).is_none())
        .map(|c| (score(c, at, ctx), c))
        .max_by(|(sa, a), (sb, b)| {
            sa.total_cmp(sb)
                .then_with(|| b.event.start_at.cmp(&a.event.start_at))
                .then_with(|| b.event.id.cmp(&a.event.id))
        })
        .map(|(_, c)| c)
}

/// Infer each calendar's owner: the address on at least
/// `OWNER_SHARE` of that source's events that have attendees (as attendee
/// or organizer). ICS feeds are one person's calendar, so their address
/// is on almost every meeting; no colleague comes close. Needs a few
/// events per source to say anything.
pub fn infer_owner_emails(events: &[(String, Option<String>, Vec<Attendee>)]) -> HashSet<String> {
    const OWNER_SHARE: f64 = 0.75;
    const MIN_EVENTS: usize = 3;

    let mut per_source: HashMap<&str, (usize, HashMap<String, usize>)> = HashMap::new();
    for (source, organizer, attendees) in events {
        if attendees.is_empty() {
            continue;
        }
        let mut emails: HashSet<String> = attendees
            .iter()
            .filter_map(|a| a.email.as_deref())
            .chain(organizer.as_deref())
            .map(|e| e.trim().to_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        let entry = per_source.entry(source.as_str()).or_default();
        entry.0 += 1;
        for e in emails.drain() {
            *entry.1.entry(e).or_default() += 1;
        }
    }

    let mut owners = HashSet::new();
    for (total, counts) in per_source.into_values() {
        if total < MIN_EVENTS {
            continue;
        }
        if let Some((email, n)) = counts.into_iter().max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0))) {
            if n as f64 >= OWNER_SHARE * total as f64 {
                owners.insert(email);
            }
        }
    }
    owners
}

/// Build the user's identity: the self voice profile's email and name, plus
/// the inferred owner of each calendar (from events within ~3 months of
/// `at`).
pub async fn load_context(
    pool: &SqlitePool,
    at: DateTime<Utc>,
    app: Option<MeetingAppEvent>,
) -> Result<AutoLinkContext, sqlx::Error> {
    let mut ctx = AutoLinkContext { app, ..Default::default() };

    if let Some(me) = VoiceProfilesRepository::get_self(pool).await? {
        if let Some(email) = me.email.map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()) {
            ctx.self_emails.insert(email);
        }
        let name = me.name.trim().to_lowercase();
        if !name.is_empty() && name != SELF_SPEAKER_LABEL.to_lowercase() {
            ctx.self_name = Some(name);
        }
    }

    let rows: Vec<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT source_id, organizer_email, attendees_json FROM calendar_events
         WHERE attendees_json != '[]' AND start_at >= ? AND start_at <= ?",
    )
    .bind((at - Duration::days(60)).to_rfc3339())
    .bind((at + Duration::days(30)).to_rfc3339())
    .fetch_all(pool)
    .await?;
    let events: Vec<(String, Option<String>, Vec<Attendee>)> = rows
        .into_iter()
        .filter_map(|(source, organizer, json)| {
            serde_json::from_str::<Vec<Attendee>>(&json).ok().map(|a| (source, organizer, a))
        })
        .collect();
    ctx.self_emails.extend(infer_owner_emails(&events));
    Ok(ctx)
}

/// Events overlapping a recording started at `at` (with the early/late
/// grace), with their raw VEVENT text.
pub async fn load_candidates(pool: &SqlitePool, at: DateTime<Utc>) -> Result<Vec<Candidate>, sqlx::Error> {
    let rows = sqlx::query_as::<_, CalendarEventRow>(
        "SELECT id, source_id, ics_uid, recurrence_id, summary, description,
                location, organizer_name, organizer_email, start_at, end_at,
                is_all_day, attendees_json, raw_ics, fetched_at
         FROM calendar_events
         WHERE start_at <= ? AND end_at >= ?
         ORDER BY start_at ASC",
    )
    .bind((at + EARLY_START).to_rfc3339())
    .bind((at - LATE_END).to_rfc3339())
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let raw_ics = row.raw_ics.clone();
            CalendarEvent::try_from(row).ok().map(|event| Candidate { event, raw_ics })
        })
        .collect())
}

/// The calendar event a recording starting at `at` should be linked to, if
/// any. `app` is the meeting app detected for the call, when known.
pub async fn find_event_for_recording(
    pool: &SqlitePool,
    at: DateTime<Utc>,
    app: Option<MeetingAppEvent>,
) -> Result<Option<CalendarEvent>, sqlx::Error> {
    let candidates = load_candidates(pool, at).await?;
    if candidates.is_empty() {
        return Ok(None);
    }
    let ctx = load_context(pool, at, app).await?;
    for c in &candidates {
        if let Some(why) = exclusion(c, at, &ctx) {
            log::debug!(
                "calendar auto-link: skipping \"{}\" ({:?})",
                c.event.summary.as_deref().unwrap_or_default(),
                why
            );
        }
    }
    let picked = pick_event(&candidates, at, &ctx).map(|c| c.event.clone());
    match &picked {
        Some(ev) => log::info!("calendar auto-link: picked \"{}\"", ev.summary.as_deref().unwrap_or_default()),
        None if !candidates.is_empty() => log::info!(
            "calendar auto-link: none of {} overlapping event(s) look like a meeting",
            candidates.len()
        ),
        None => {}
    }
    Ok(picked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 15, 12, 0, 0).unwrap()
    }

    fn person(email: &str) -> Attendee {
        Attendee {
            name: Some(email.split('@').next().unwrap().to_string()),
            email: Some(email.to_string()),
            role: Some("REQ-PARTICIPANT".into()),
            status: Some("ACCEPTED".into()),
            is_organizer: false,
        }
    }

    /// An event starting `offset_min` from [`at`], lasting `len_min`.
    fn event(id: &str, offset_min: i64, len_min: i64, organizer: Option<&str>, attendees: &[&str]) -> CalendarEvent {
        let start = at() + Duration::minutes(offset_min);
        CalendarEvent {
            id: id.into(),
            source_id: "src".into(),
            ics_uid: format!("{id}@uid"),
            recurrence_id: None,
            summary: Some(id.into()),
            description: None,
            location: None,
            organizer_name: None,
            organizer_email: organizer.map(str::to_string),
            start_at: start,
            end_at: start + Duration::minutes(len_min),
            is_all_day: false,
            attendees: attendees.iter().map(|e| person(e)).collect(),
        }
    }

    fn cand(event: CalendarEvent, raw: Option<&str>) -> Candidate {
        Candidate { event, raw_ics: raw.map(str::to_string) }
    }

    fn me() -> AutoLinkContext {
        AutoLinkContext { self_emails: ["me@corp.com".to_string()].into(), ..Default::default() }
    }

    const ME: &str = "me@corp.com";

    fn lunch() -> Candidate {
        // The real shape: a recurring blocker with no organizer or attendees.
        let mut e = event("Lunch", 0, 60, None, &[]);
        e.summary = Some("Lunch".into());
        cand(e, Some("BEGIN:VEVENT\nSUMMARY:Lunch\nSTATUS:CONFIRMED\nTRANSP:OPAQUE\nEND:VEVENT"))
    }

    #[test]
    fn lunch_blocker_is_never_picked() {
        let c = [lunch()];
        assert_eq!(exclusion(&c[0], at(), &me()), Some(Exclusion::NoOtherAttendees));
        assert_eq!(exclusion(&c[0], at(), &AutoLinkContext::default()), Some(Exclusion::NoOtherAttendees));
        assert!(pick_event(&c, at(), &me()).is_none());
    }

    #[test]
    fn solo_events_are_excluded_even_without_knowing_the_user() {
        let unknown = AutoLinkContext::default();
        // Organizer invited only themself.
        let own = cand(event("focus", 0, 60, Some(ME), &[ME]), None);
        assert_eq!(exclusion(&own, at(), &unknown), Some(Exclusion::NoOtherAttendees));
        // Someone else's event with only me on it is a 1:1, not a blocker.
        let one_to_one = cand(event("1:1", 0, 30, Some("boss@corp.com"), &[ME]), None);
        assert_eq!(exclusion(&one_to_one, at(), &me()), None);
        assert_eq!(other_attendee_count(&one_to_one.event, &me()), 1);
        // I organized it and invited only me + a room (non-participant).
        let mut room = event("room", 0, 30, Some(ME), &[ME, "room-1@corp.com"]);
        room.attendees[1].role = Some("NON-PARTICIPANT".into());
        assert_eq!(exclusion(&cand(room, None), at(), &me()), Some(Exclusion::NoOtherAttendees));
        // Only me listed as attendee, organizer unknown: needs the identity.
        let only_me = cand(event("hold", 0, 30, None, &[ME]), None);
        assert_eq!(exclusion(&only_me, at(), &me()), Some(Exclusion::NoOtherAttendees));
    }

    #[test]
    fn all_day_cancelled_free_and_declined_are_excluded() {
        let mut all_day = event("offsite", -720, 1440, Some("a@corp.com"), &[ME, "b@corp.com"]);
        all_day.is_all_day = true;
        assert_eq!(exclusion(&cand(all_day, None), at(), &me()), Some(Exclusion::AllDay));

        let cancelled_status = cand(
            event("sync", 0, 30, Some("a@corp.com"), &[ME, "b@corp.com"]),
            Some("BEGIN:VEVENT\r\nSTATUS:CANCELLED\r\nEND:VEVENT"),
        );
        assert_eq!(exclusion(&cancelled_status, at(), &me()), Some(Exclusion::Cancelled));
        let mut outlook = event("x", 0, 30, Some("a@corp.com"), &[ME, "b@corp.com"]);
        outlook.summary = Some("Canceled: Weekly sync".into());
        assert_eq!(exclusion(&cand(outlook, None), at(), &me()), Some(Exclusion::Cancelled));

        for raw in ["TRANSP:TRANSPARENT", "X-MICROSOFT-CDO-BUSYSTATUS:FREE"] {
            let free = cand(event("ooo", 0, 30, Some("a@corp.com"), &[ME, "b@corp.com"]), Some(raw));
            assert_eq!(exclusion(&free, at(), &me()), Some(Exclusion::Free), "{raw}");
        }

        let mut declined = event("declined", 0, 30, Some("a@corp.com"), &[ME, "b@corp.com"]);
        declined.attendees[0].status = Some("DECLINED".into());
        assert_eq!(exclusion(&cand(declined, None), at(), &me()), Some(Exclusion::Declined));
    }

    #[test]
    fn window_allows_early_join_and_overrun() {
        let soon = cand(event("soon", 4, 30, Some("a@x.com"), &["b@x.com"]), None);
        let later = cand(event("later", 10, 30, Some("a@x.com"), &["b@x.com"]), None);
        let overran = cand(event("overran", -70, 60, Some("a@x.com"), &["b@x.com"]), None);
        let over = cand(event("over", -90, 60, Some("a@x.com"), &["b@x.com"]), None);
        assert_eq!(exclusion(&soon, at(), &me()), None);
        assert_eq!(exclusion(&later, at(), &me()), Some(Exclusion::OutOfWindow));
        assert_eq!(exclusion(&overran, at(), &me()), None);
        assert_eq!(exclusion(&over, at(), &me()), Some(Exclusion::OutOfWindow));
    }

    #[test]
    fn real_meeting_wins_over_blockers() {
        let mut all_day = event("holiday", -720, 1440, None, &[]);
        all_day.is_all_day = true;
        let cancelled = cand(
            event("cancelled", 0, 60, Some("a@corp.com"), &[ME, "b@corp.com", "c@corp.com"]),
            Some("STATUS:CANCELLED"),
        );
        let real = cand(event("Project review", 0, 30, Some("a@corp.com"), &[ME, "b@corp.com"]), None);
        let c = [lunch(), cand(all_day, None), cancelled, real];
        assert_eq!(pick_event(&c, at(), &me()).unwrap().event.id, "Project review");
    }

    #[test]
    fn overlapping_meetings_prefer_closest_start() {
        // An all-hands that started 50 min ago vs a 1:1 starting now.
        let many: Vec<String> = (0..20).map(|i| format!("p{i}@corp.com")).collect();
        let many_refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let all_hands = cand(event("all-hands", -50, 90, Some("ceo@corp.com"), &many_refs), None);
        let one_to_one = cand(event("1:1", 0, 30, Some("boss@corp.com"), &[ME]), None);
        let c = [all_hands, one_to_one];
        assert_eq!(pick_event(&c, at(), &me()).unwrap().event.id, "1:1");
    }

    #[test]
    fn overlapping_meetings_same_start_prefer_more_attendees() {
        let small = cand(event("small", 0, 30, Some("a@corp.com"), &[ME, "b@corp.com"]), None);
        let big = cand(
            event("big", 0, 30, Some("a@corp.com"), &[ME, "b@corp.com", "c@corp.com", "d@corp.com"]),
            None,
        );
        let c = [small, big];
        assert_eq!(pick_event(&c, at(), &me()).unwrap().event.id, "big");
    }

    #[test]
    fn overlapping_meetings_prefer_detected_app() {
        let mut zoom = event("zoom call", 0, 30, Some("a@corp.com"), &[ME, "b@corp.com", "c@corp.com"]);
        zoom.location = Some("https://corp.zoom.us/j/123".into());
        let teams = cand(
            event("teams call", 0, 30, Some("a@corp.com"), &[ME, "b@corp.com"]),
            Some("X-MICROSOFT-SKYPETEAMSMEETINGURL:https://teams.microsoft.com/l/meetup-join/abc"),
        );
        let c = [cand(zoom, None), teams];
        // Without app context the bigger meeting wins…
        assert_eq!(pick_event(&c, at(), &me()).unwrap().event.id, "zoom call");
        // …but a live Teams call points at the Teams meeting.
        let ctx = AutoLinkContext {
            app: Some(MeetingAppEvent { app_id: "teams".into(), app_name: "Microsoft Teams".into(), is_browser: false }),
            ..me()
        };
        assert_eq!(pick_event(&c, at(), &ctx).unwrap().event.id, "teams call");
    }

    #[test]
    fn infers_calendar_owner() {
        let ev = |source: &str, org: &str, att: &[&str]| {
            (source.to_string(), Some(org.to_string()), att.iter().map(|e| person(e)).collect::<Vec<_>>())
        };
        let events = vec![
            ev("work", "a@corp.com", &["Me@Corp.com", "b@corp.com"]),
            ev("work", "me@corp.com", &["c@corp.com"]),
            ev("work", "b@corp.com", &["me@corp.com", "a@corp.com"]),
            ev("work", "d@corp.com", &["me@corp.com", "b@corp.com"]),
            // A second calendar with too few events to say.
            ev("home", "x@home.com", &["y@home.com"]),
        ];
        let owners = infer_owner_emails(&events);
        assert_eq!(owners, ["me@corp.com".to_string()].into());
        // b@ is on 3/4 — not enough when another address is on all 4, and
        // only the top address is ever taken.
        assert!(!owners.contains("b@corp.com"));
    }
}
