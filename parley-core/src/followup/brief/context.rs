//! Gathering the history a pre-meeting brief is built from.
//!
//! A past meeting is related to an upcoming event when it was recorded
//! before the event starts and:
//!
//! - **same series**: it's linked to an occurrence of the same calendar
//!   event (same source and ICS UID), or
//! - **same people**: one of the event's other attendees (or its organizer)
//!   was in it, matched by
//!   - the past meeting's own calendar event listing them (by email, else
//!     by name), or
//!   - a voice profile whose email or name matches them speaking in it
//!     (`transcripts.voice_profile_id`), or a transcript segment labelled
//!     with their name.
//!
//! The user is left out of "people" (the `is_self` voice profile, by email
//! or name); otherwise every meeting they recorded would match.
//!
//! Up to [`MAX_SERIES_MEETINGS`] of the most recent same-series meetings are
//! kept, then the rest of [`MAX_MEETINGS`] is filled with people matches,
//! most shared people first, then most recent. Each selected meeting brings
//! its stored summary and its open action items.

use crate::calendar::models::{Attendee, CalendarEvent};
use crate::database::repositories::action_item::STATUS_OPEN;
use crate::followup::{attendee_label, load_summary, SelfIdentity, StoredSummary};
use chrono::{DateTime, Utc};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};
use std::collections::{BTreeSet, HashMap, HashSet};

/// Meetings a brief draws on.
pub const MAX_MEETINGS: usize = 6;
/// Of those, at most this many come from the event's own series.
pub const MAX_SERIES_MEETINGS: usize = 4;
/// Open action items per meeting.
pub const MAX_ITEMS_PER_MEETING: usize = 8;

/// The other people expected at an event.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct People {
    /// Lowercased emails.
    pub emails: HashSet<String>,
    /// Lowercased names.
    pub names: HashSet<String>,
    /// How to show each person, in attendee order.
    pub labels: Vec<String>,
}

impl People {
    pub fn is_empty(&self) -> bool {
        self.emails.is_empty() && self.names.is_empty()
    }

    /// The label of the person `attendee` is, if they're one of these.
    fn matching_label(&self, attendee: &Attendee, by_key: &HashMap<String, String>) -> Option<String> {
        let email = attendee.email.as_deref().map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty());
        if let Some(email) = email {
            if self.emails.contains(&email) {
                return by_key.get(&email).cloned();
            }
        }
        let name = attendee.name.as_deref().map(|n| n.trim().to_lowercase()).filter(|n| !n.is_empty());
        name.filter(|n| self.names.contains(n)).and_then(|n| by_key.get(&n).cloned())
    }
}

/// An event's attendees plus its organizer, minus the user.
pub fn event_people(event: &CalendarEvent, me: &SelfIdentity) -> People {
    let mut everyone: Vec<Attendee> = event.attendees.clone();
    if event.organizer_email.is_some() || event.organizer_name.is_some() {
        everyone.push(Attendee {
            name: event.organizer_name.clone(),
            email: event.organizer_email.clone(),
            is_organizer: true,
            ..Default::default()
        });
    }
    let mut people = People::default();
    for attendee in everyone.iter().filter(|a| !me.is(a)) {
        let email = attendee.email.as_deref().map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty());
        let name = attendee.name.as_deref().map(|n| n.trim().to_lowercase()).filter(|n| !n.is_empty());
        let known = email.as_ref().is_some_and(|e| people.emails.contains(e))
            || name.as_ref().is_some_and(|n| people.names.contains(n));
        if let Some(e) = email {
            people.emails.insert(e);
        }
        if let Some(n) = name {
            people.names.insert(n);
        }
        if !known {
            if let Some(label) = attendee_label(attendee) {
                people.labels.push(label);
            }
        }
    }
    people
}

/// Email/name (lowercased) → display label, for reporting who matched.
fn label_index(event: &CalendarEvent, me: &SelfIdentity) -> HashMap<String, String> {
    let mut index = HashMap::new();
    let organizer = Attendee {
        name: event.organizer_name.clone(),
        email: event.organizer_email.clone(),
        ..Default::default()
    };
    for attendee in event.attendees.iter().chain(std::iter::once(&organizer)).filter(|a| !me.is(a)) {
        let Some(label) = attendee_label(attendee) else { continue };
        for key in [attendee.email.as_deref(), attendee.name.as_deref()].into_iter().flatten() {
            let key = key.trim().to_lowercase();
            if !key.is_empty() {
                index.entry(key).or_insert_with(|| label.clone());
            }
        }
    }
    index
}

/// An open action item from a past meeting.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenItem {
    pub text: String,
    pub owner: Option<String>,
    pub due: Option<String>,
}

/// A past meeting the brief draws on.
#[derive(Debug, Clone, PartialEq)]
pub struct RelatedMeeting {
    pub meeting_id: String,
    pub title: String,
    pub created_at: Option<DateTime<Utc>>,
    pub same_series: bool,
    /// Labels of the event's people who were in it.
    pub people: Vec<String>,
    pub summary: StoredSummary,
    pub open_items: Vec<OpenItem>,
}

/// Everything a brief is built from.
#[derive(Debug, Clone, PartialEq)]
pub struct BriefContext {
    pub event_title: String,
    pub event_start: DateTime<Utc>,
    /// The other people expected.
    pub people: Vec<String>,
    /// Selected meetings, most recent first.
    pub meetings: Vec<RelatedMeeting>,
    /// How many related meetings there were before selection.
    pub total_related: usize,
}

impl BriefContext {
    /// Whether any selected meeting has a summary or open items to brief on.
    pub fn has_substance(&self) -> bool {
        self.meetings.iter().any(|m| {
            !m.open_items.is_empty()
                || m.summary.structured.is_some()
                || m.summary.markdown.as_deref().is_some_and(|s| !s.trim().is_empty())
        })
    }
}

#[derive(Debug, Default)]
struct Candidate {
    same_series: bool,
    people: BTreeSet<String>,
}

/// Gather the related history for `event`.
pub async fn gather(pool: &SqlitePool, event: &CalendarEvent) -> Result<BriefContext, sqlx::Error> {
    let me = SelfIdentity::load(pool).await?;
    let people = event_people(event, &me);
    let labels = label_index(event, &me);
    let before = event.start_at.to_rfc3339();
    let mut candidates: HashMap<String, Candidate> = HashMap::new();

    // Same series.
    let series: Vec<(String,)> = sqlx::query_as(
        "SELECT m.id FROM meetings m
         JOIN calendar_events ce ON ce.id = m.calendar_event_id
         WHERE ce.source_id = ? AND ce.ics_uid = ? AND ce.id != ?
           AND julianday(m.created_at) < julianday(?)",
    )
    .bind(&event.source_id)
    .bind(&event.ics_uid)
    .bind(&event.id)
    .bind(&before)
    .fetch_all(pool)
    .await?;
    for (id,) in series {
        candidates.entry(id).or_default().same_series = true;
    }

    if !people.is_empty() {
        // People listed on past meetings' own calendar events.
        let linked: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT m.id, ce.attendees_json, ce.organizer_name, ce.organizer_email FROM meetings m
             JOIN calendar_events ce ON ce.id = m.calendar_event_id
             WHERE julianday(m.created_at) < julianday(?)",
        )
        .bind(&before)
        .fetch_all(pool)
        .await?;
        for (id, attendees_json, organizer_name, organizer_email) in linked {
            let mut attendees: Vec<Attendee> = serde_json::from_str(&attendees_json).unwrap_or_default();
            attendees.push(Attendee {
                name: organizer_name,
                email: organizer_email,
                ..Default::default()
            });
            for attendee in attendees.iter().filter(|a| !me.is(a)) {
                if let Some(label) = people.matching_label(attendee, &labels) {
                    candidates.entry(id.clone()).or_default().people.insert(label);
                }
            }
        }

        // People heard speaking: voice profiles matching by email or name.
        let profiles: Vec<(String, String, Option<String>)> =
            sqlx::query_as("SELECT id, name, email FROM voice_profiles WHERE is_self = 0")
                .fetch_all(pool)
                .await?;
        let mut profile_labels: HashMap<String, String> = HashMap::new();
        for (id, name, email) in profiles {
            let as_attendee = Attendee {
                name: Some(name),
                email,
                ..Default::default()
            };
            if me.is(&as_attendee) {
                continue;
            }
            if let Some(label) = people.matching_label(&as_attendee, &labels) {
                profile_labels.insert(id, label);
            }
        }
        if !profile_labels.is_empty() {
            let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
                "SELECT DISTINCT meeting_id, voice_profile_id FROM transcripts WHERE voice_profile_id IN (",
            );
            push_list(&mut qb, profile_labels.keys());
            qb.push(")");
            let rows: Vec<(String, String)> = qb.build_query_as().fetch_all(pool).await?;
            for (meeting_id, profile_id) in rows {
                if let Some(label) = profile_labels.get(&profile_id) {
                    candidates.entry(meeting_id).or_default().people.insert(label.clone());
                }
            }
        }

        // Segments labelled with an attendee's name (manual labels without
        // a profile link).
        if !people.names.is_empty() {
            let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
                "SELECT DISTINCT meeting_id, LOWER(TRIM(speaker)) FROM transcripts WHERE LOWER(TRIM(speaker)) IN (",
            );
            push_list(&mut qb, people.names.iter());
            qb.push(")");
            let rows: Vec<(String, String)> = qb.build_query_as().fetch_all(pool).await?;
            for (meeting_id, name) in rows {
                if let Some(label) = labels.get(&name) {
                    candidates.entry(meeting_id).or_default().people.insert(label.clone());
                }
            }
        }
    }

    // Meeting metadata; only finished meetings from before the event.
    let mut related: Vec<(String, String, Option<DateTime<Utc>>, Candidate)> = Vec::new();
    for (id, candidate) in candidates {
        let row: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT title, strftime('%Y-%m-%dT%H:%M:%SZ', created_at), status FROM meetings
             WHERE id = ? AND julianday(created_at) < julianday(?)",
        )
        .bind(&id)
        .bind(&before)
        .fetch_optional(pool)
        .await?;
        let Some((title, created, status)) = row else { continue };
        if status.as_deref() == Some("recording") {
            continue;
        }
        let created_at = created
            .and_then(|c| DateTime::parse_from_rfc3339(&c).ok())
            .map(|d| d.with_timezone(&Utc));
        related.push((id, title, created_at, candidate));
    }
    let total_related = related.len();

    let selected = select(related);
    let mut meetings = Vec::with_capacity(selected.len());
    for (id, title, created_at, candidate) in selected {
        let summary = load_summary(pool, &id).await?;
        let items: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT text, assignee, due_hint FROM action_items
             WHERE meeting_id = ? AND status = ? ORDER BY created_at ASC LIMIT ?",
        )
        .bind(&id)
        .bind(STATUS_OPEN)
        .bind(MAX_ITEMS_PER_MEETING as i64)
        .fetch_all(pool)
        .await?;
        meetings.push(RelatedMeeting {
            meeting_id: id,
            title,
            created_at,
            same_series: candidate.same_series,
            people: candidate.people.into_iter().collect(),
            summary,
            open_items: items
                .into_iter()
                .map(|(text, owner, due)| OpenItem { text, owner, due })
                .collect(),
        });
    }

    Ok(BriefContext {
        event_title: event
            .summary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Untitled event")
            .to_string(),
        event_start: event.start_at,
        people: people.labels,
        meetings,
        total_related,
    })
}

fn push_list<'a>(qb: &mut QueryBuilder<'_, Sqlite>, values: impl Iterator<Item = &'a String>) {
    let mut separated = qb.separated(", ");
    for value in values {
        separated.push_bind(value.clone());
    }
}

type Related = (String, String, Option<DateTime<Utc>>, Candidate);

/// Pick the meetings to brief on (see the module docs), most recent first.
fn select(mut related: Vec<Related>) -> Vec<Related> {
    // Most recent first everywhere; undated last.
    related.sort_by(|a, b| b.2.cmp(&a.2));
    let (series, others): (Vec<Related>, Vec<Related>) = related.into_iter().partition(|r| r.3.same_series);
    let mut chosen: Vec<Related> = series.into_iter().take(MAX_SERIES_MEETINGS).collect();
    let mut others = others;
    // Stable: equal overlap keeps recency order.
    others.sort_by(|a, b| b.3.people.len().cmp(&a.3.people.len()));
    let room = MAX_MEETINGS.saturating_sub(chosen.len());
    chosen.extend(others.into_iter().take(room));
    chosen.sort_by(|a, b| b.2.cmp(&a.2));
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn related(id: &str, day: u32, series: bool, people: &[&str]) -> Related {
        (
            id.to_string(),
            id.to_string(),
            Some(Utc.with_ymd_and_hms(2026, 9, day, 9, 0, 0).unwrap()),
            Candidate {
                same_series: series,
                people: people.iter().map(|p| p.to_string()).collect(),
            },
        )
    }

    #[test]
    fn selection_prefers_recent_series_then_overlap() {
        let mut all = Vec::new();
        for day in 1..=6 {
            all.push(related(&format!("s{day}"), day, true, &[]));
        }
        all.push(related("one-person-new", 20, false, &["Ann"]));
        all.push(related("two-people-old", 2, false, &["Ann", "Bob"]));
        all.push(related("one-person-old", 1, false, &["Bob"]));
        let ids: Vec<String> = select(all).into_iter().map(|r| r.0).collect();
        assert_eq!(ids, vec!["one-person-new", "s6", "s5", "s4", "s3", "two-people-old"]);
    }

    #[test]
    fn people_exclude_the_user_and_dedupe() {
        let event = CalendarEvent {
            id: "e".into(),
            source_id: "s".into(),
            ics_uid: "u".into(),
            recurrence_id: None,
            summary: Some("Sync".into()),
            description: None,
            location: None,
            organizer_name: Some("Seb".into()),
            organizer_email: Some("SEB@example.com".into()),
            start_at: Utc::now(),
            end_at: Utc::now(),
            is_all_day: false,
            attendees: vec![
                Attendee { name: Some("Ann Lee".into()), email: Some("ann@example.com".into()), ..Default::default() },
                Attendee { name: None, email: Some("Ann@Example.com".into()), ..Default::default() },
                Attendee { name: Some("Seb".into()), email: Some("seb@example.com".into()), ..Default::default() },
                Attendee { name: None, email: Some("bob@example.com".into()), ..Default::default() },
            ],
        };
        let me = SelfIdentity { name: Some("Seb".into()), email: Some("seb@example.com".into()) };
        let people = event_people(&event, &me);
        assert_eq!(people.labels, vec!["Ann Lee".to_string(), "bob@example.com".to_string()]);
        assert!(people.emails.contains("ann@example.com"));
        assert!(!people.emails.contains("seb@example.com"));
        assert!(people.names.contains("ann lee"));
    }
}
