//! What the summariser and the action-item extractors are told about a
//! meeting besides its words — and the checks that hold their output to it.
//!
//! Without this the model sees only `[mm:ss] text` lines. It then makes up
//! years from its training era ("due October 26, 2023" on a 2026 meeting),
//! attributes tasks to names it half-heard or invented ("Dave Hewitt" for
//! Dave Williams), and can't tell who said what. Three things fix most of
//! that:
//!
//! 1. **Speaker-labelled transcript** — [`build_transcript_text`] writes
//!    `[mm:ss] Speaker: text`, so ownership can follow who spoke.
//! 2. **Meeting date and participants** in the prompt
//!    ([`MeetingContext::prompt_block`]) — the date anchors "Friday" / "next
//!    week", and the participant list (calendar attendees, the user, named
//!    speakers) bounds who can own a task.
//! 3. **Post-validation** — the model is told all this and ignores it often
//!    enough (small local models especially) that its output is checked too:
//!    [`MeetingContext::check_owner`] drops owners who aren't participants and
//!    [`MeetingContext::check_due`] drops due dates in a year before the
//!    meeting or naming a year nobody said. An empty field is better than a
//!    confident wrong one.

use crate::calendar::repository::CalendarRepository;
use crate::database::models::MeetingTranscript;
use crate::followup::{is_person, SelfIdentity};
use crate::summary::action_extraction::is_generic_speaker_label;
use chrono::{DateTime, Datelike, Local, NaiveDate};
use sqlx::SqlitePool;
use tracing::warn;

/// The facts about a meeting that aren't in its transcript text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MeetingContext {
    /// The day the meeting happened, in local time.
    pub date: Option<NaiveDate>,
    /// People known to have been there: calendar attendees and organiser,
    /// the user, and speakers labelled with a real name. Deduplicated
    /// case-insensitively, in that order.
    pub participants: Vec<String>,
    /// Whether `participants` is a real roster (calendar attendees or named
    /// speakers), not just the user. Only a real roster is strict enough to
    /// reject an owner outright; see [`check_owner`](Self::check_owner).
    pub has_roster: bool,
}

impl MeetingContext {
    /// Context for an in-progress recording: today's date, and the user if a
    /// self voice profile exists. No calendar lookup — there's no meeting row
    /// to link from yet.
    pub async fn live(pool: Option<&SqlitePool>) -> Self {
        let mut ctx = MeetingContext {
            date: Some(Local::now().date_naive()),
            participants: Vec::new(),
            has_roster: false,
        };
        if let Some(pool) = pool {
            if let Ok(me) = SelfIdentity::load(pool).await {
                if let Some(name) = me.name {
                    ctx.add_participant(&name);
                }
            }
        }
        ctx
    }

    /// Gather the context for a stored meeting. Best-effort: a failed lookup
    /// leaves that part empty rather than failing the summary.
    pub async fn load(pool: &SqlitePool, meeting_id: &str) -> Self {
        let mut ctx = MeetingContext::default();

        let event = match CalendarRepository::get_event_for_meeting(pool, meeting_id).await {
            Ok(event) => event,
            Err(e) => {
                warn!("Couldn't load calendar event for {meeting_id}: {e}");
                None
            }
        };

        // Date: the calendar slot if linked (the recording may have started
        // early or late), else when the recording started.
        ctx.date = match &event {
            Some(event) => Some(event.start_at.with_timezone(&Local).date_naive()),
            None => sqlx::query_scalar::<_, String>("SELECT created_at FROM meetings WHERE id = ?")
                .bind(meeting_id)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten()
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|d| d.with_timezone(&Local).date_naive()),
        };

        let me = SelfIdentity::load(pool).await.unwrap_or_default();
        if let Some(name) = &me.name {
            ctx.add_participant(name);
        }

        if let Some(event) = &event {
            for attendee in event.attendees.iter().filter(|a| is_person(a)) {
                if let Some(name) = person_name(attendee.name.as_deref(), attendee.email.as_deref())
                {
                    ctx.add_participant(&name);
                }
            }
            if let Some(name) = person_name(
                event.organizer_name.as_deref(),
                event.organizer_email.as_deref(),
            ) {
                ctx.add_participant(&name);
            }
        }

        match sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT TRIM(speaker) FROM transcripts \
             WHERE meeting_id = ? AND speaker IS NOT NULL AND TRIM(speaker) != ''",
        )
        .bind(meeting_id)
        .fetch_all(pool)
        .await
        {
            Ok(speakers) => {
                for speaker in speakers {
                    if !is_generic_speaker_label(&speaker) && !speaker.eq_ignore_ascii_case("me") {
                        ctx.add_participant(&speaker);
                    }
                }
            }
            Err(e) => warn!("Couldn't load speakers for {meeting_id}: {e}"),
        }

        // A roster that names nobody but the user (no calendar link, the
        // user the only named speaker) can't tell a real owner from an
        // invented one.
        let is_me = |p: &String| {
            me.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(p))
        };
        ctx.has_roster = ctx.participants.iter().any(|p| !is_me(p));

        ctx
    }

    fn add_participant(&mut self, name: &str) {
        let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
        if name.is_empty()
            || self
                .participants
                .iter()
                .any(|p| p.eq_ignore_ascii_case(&name))
        {
            return;
        }
        self.participants.push(name);
    }

    pub fn is_empty(&self) -> bool {
        self.date.is_none() && self.participants.is_empty()
    }

    /// A `<meeting_context>` block for the user prompt, or "" when there's
    /// nothing to say.
    pub fn prompt_block(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut block = String::from("<meeting_context>\n");
        if let Some(date) = self.date {
            block.push_str(&format!(
                "Meeting date: {}. Treat this as \"today\" when reading relative dates such as \"Friday\" or \"next week\".\n",
                date.format("%A %-d %B %Y")
            ));
        }
        if !self.participants.is_empty() {
            block.push_str(&format!(
                "Participants: {}. Only these people (or a name the transcript clearly says) can own an action item.\n",
                self.participants.join(", ")
            ));
        }
        block.push_str("</meeting_context>\n");
        block
    }

    /// Keep `owner` only if it names known participants. Each name in a list
    /// ("Ross, Mark and Tom") is checked on its own and the unknown ones are
    /// dropped. A name matches a participant whose name contains all its
    /// words, so "Seb" matches "Seb Burrell" (and becomes "Seb Burrell" when
    /// that's the only match) while "Dave Hewitt" doesn't match "Dave
    /// Williams".
    ///
    /// A name that matches no participant is dropped when there's a real
    /// roster ([`has_roster`](Self::has_roster)). Without one (no calendar
    /// link, unnamed speakers) the list holds at most the user, so an
    /// unmatched name is instead kept if it was spoken in `transcript` — or
    /// kept as is when there's no transcript to check against.
    pub fn check_owner(&self, owner: Option<String>, transcript: Option<&str>) -> Option<String> {
        let owner = owner?;
        let spoken = transcript.map(name_words);
        let kept: Vec<String> = split_names(&owner)
            .into_iter()
            .filter_map(|name| {
                if let Some(full) = self.match_participant(&name) {
                    return Some(full);
                }
                if self.has_roster {
                    return None;
                }
                let said = match &spoken {
                    Some(words) => {
                        let wanted = name_words(&name);
                        !wanted.is_empty() && wanted.iter().all(|w| words.contains(w))
                    }
                    None => true,
                };
                said.then_some(name)
            })
            .fold(Vec::new(), |mut acc, name| {
                if !acc.contains(&name) {
                    acc.push(name);
                }
                acc
            });
        (!kept.is_empty()).then(|| kept.join(", "))
    }

    fn match_participant(&self, name: &str) -> Option<String> {
        let wanted = name_words(name);
        if wanted.is_empty() {
            return None;
        }
        let matches: Vec<&String> = self
            .participants
            .iter()
            .filter(|p| {
                let have = name_words(p);
                wanted.iter().all(|w| have.contains(w))
            })
            .collect();
        match matches.as_slice() {
            [] => None,
            [only] => Some((*only).clone()),
            // Ambiguous ("Tom" with two Toms present): keep what was said.
            _ => Some(name.to_string()),
        }
    }

    /// Keep `due` unless it's clearly made up: it names a year before the
    /// meeting's, or (when the transcript is given) a year that appears
    /// nowhere in it. Spoken deadlines rarely carry a year, so a year that
    /// nobody said is almost always the model's invention.
    pub fn check_due(&self, due: Option<String>, transcript: Option<&str>) -> Option<String> {
        let due = due?;
        let years = years_in(&due);
        if let Some(date) = self.date {
            if years.iter().any(|&y| y < date.year()) {
                return None;
            }
            // A full ISO date before the meeting is in the past: not a deadline.
            if let Some(iso) = iso_date_in(&due) {
                if iso < date {
                    return None;
                }
            }
        }
        if let Some(transcript) = transcript {
            if years.iter().any(|y| !transcript.contains(&y.to_string())) {
                return None;
            }
        }
        Some(due)
    }

    /// [`check_owner`](Self::check_owner) and [`check_due`](Self::check_due)
    /// on each item, in place.
    pub fn check_items(
        &self,
        items: &mut [crate::database::repositories::action_item::NewActionItem],
        transcript: Option<&str>,
    ) {
        for item in items {
            item.assignee = self.check_owner(item.assignee.take(), transcript);
            item.due_hint = self.check_due(item.due_hint.take(), transcript);
        }
    }
}

/// The plain-text transcript the summariser reads: one
/// `[mm:ss] Speaker: text` line per segment (wall-clock timestamp when the
/// segment has no recording-relative time; no speaker prefix when the
/// segment has no speaker).
pub fn build_transcript_text(transcripts: &[MeetingTranscript]) -> String {
    transcripts
        .iter()
        .filter(|t| !t.text.trim().is_empty())
        .map(|t| {
            let stamp = match t.audio_start_time {
                Some(s) if s.is_finite() && s >= 0.0 => {
                    let total = s.floor() as u64;
                    format!("[{:02}:{:02}]", total / 60, total % 60)
                }
                _ => t.timestamp.clone(),
            };
            match t
                .speaker
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(speaker) => format!("{stamp} {speaker}: {}", t.text.trim()),
                None => format!("{stamp} {}", t.text.trim()),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A display name for a calendar person: their name, else one derived from
/// a `first.last@` email. A bare email that doesn't look like a name
/// ("info@", "x7@") yields nothing — it can't be matched to anything said.
fn person_name(name: Option<&str>, email: Option<&str>) -> Option<String> {
    if let Some(name) = name
        .map(str::trim)
        .filter(|n| !n.is_empty() && !n.contains('@'))
    {
        return Some(name.to_string());
    }
    let local = email?.trim().split('@').next()?;
    let parts: Vec<&str> = local
        .split(['.', '_', '-'])
        .filter(|p| p.len() > 1 && p.chars().all(|c| c.is_alphabetic()))
        .collect();
    if parts.len() < 2 {
        return None;
    }
    Some(
        parts
            .iter()
            .map(|p| {
                let mut chars = p.chars();
                match chars.next() {
                    Some(first) => first
                        .to_uppercase()
                        .chain(chars.flat_map(char::to_lowercase))
                        .collect(),
                    None => String::new(),
                }
            })
            .collect::<Vec<String>>()
            .join(" "),
    )
}

/// Split an owner field into individual names on commas, "&", "/" and the
/// word "and".
fn split_names(owner: &str) -> Vec<String> {
    owner
        .split([',', '&', '/', ';'])
        .flat_map(|part| part.split(" and "))
        .map(|s| s.trim().trim_start_matches("and ").trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Lowercased alphabetic words of a name.
fn name_words(name: &str) -> Vec<String> {
    name.split(|c: char| !(c.is_alphabetic() || c == '\''))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Four-digit years (1900–2199) appearing as whole numbers in `s`.
fn years_in(s: &str) -> Vec<i32> {
    s.split(|c: char| !c.is_ascii_digit())
        .filter(|run| run.len() == 4)
        .filter_map(|run| run.parse::<i32>().ok())
        .filter(|y| (1900..2200).contains(y))
        .collect()
}

/// The first `YYYY-MM-DD` date in `s`.
fn iso_date_in(s: &str) -> Option<NaiveDate> {
    let bytes = s.as_bytes();
    (0..bytes.len().saturating_sub(9)).find_map(|i| {
        let candidate = s.get(i..i + 10)?;
        NaiveDate::parse_from_str(candidate, "%Y-%m-%d").ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(date: Option<(i32, u32, u32)>, people: &[&str]) -> MeetingContext {
        MeetingContext {
            date: date.and_then(|(y, m, d)| NaiveDate::from_ymd_opt(y, m, d)),
            participants: people.iter().map(|s| s.to_string()).collect(),
            has_roster: people.len() > 1,
        }
    }

    fn owner(c: &MeetingContext, name: &str, transcript: Option<&str>) -> Option<String> {
        c.check_owner(Some(name.to_string()), transcript)
    }

    #[test]
    fn owner_must_be_on_the_roster() {
        let c = ctx(None, &["Seb Burrell", "Dave Williams", "Tom Bishop"]);
        // Spoken or not, a name off a real roster is dropped.
        let said = "[00:10] Dave Williams: Hewitt mentioned it, and Lee too";
        assert_eq!(owner(&c, "Dave Hewitt", Some(said)), None);
        assert_eq!(owner(&c, "Lee", Some(said)), None);
        assert_eq!(owner(&c, "Seb", None).as_deref(), Some("Seb Burrell"));
        assert_eq!(
            owner(&c, "dave williams", None).as_deref(),
            Some("Dave Williams")
        );
    }

    #[test]
    fn owner_lists_keep_only_known_names() {
        let c = ctx(None, &["Seb Burrell", "Tom Bishop"]);
        assert_eq!(
            owner(&c, "Ross, Mark, and Tom", None).as_deref(),
            Some("Tom Bishop")
        );
        assert_eq!(owner(&c, "Ross & Mark", None), None);
    }

    #[test]
    fn ambiguous_first_name_is_kept_as_said() {
        let c = ctx(None, &["Tom Bishop", "Tom Jones"]);
        assert_eq!(owner(&c, "Tom", None).as_deref(), Some("Tom"));
    }

    #[test]
    fn without_a_roster_a_spoken_name_is_kept() {
        // Only the user is known (no calendar link, unnamed speakers).
        let c = ctx(Some((2026, 10, 28)), &["Seb Burrell"]);
        let transcript = "[00:10] Speaker 1: Rich, can you take the roadmap?";
        assert_eq!(owner(&c, "Rich", Some(transcript)).as_deref(), Some("Rich"));
        assert_eq!(owner(&c, "Dave Hewitt", Some(transcript)), None);
        assert_eq!(
            owner(&c, "Seb", Some(transcript)).as_deref(),
            Some("Seb Burrell")
        );
        // Nothing to check against: keep it.
        assert_eq!(owner(&c, "Ross", None).as_deref(), Some("Ross"));
        let empty = ctx(None, &[]);
        assert_eq!(owner(&empty, "Ross", None).as_deref(), Some("Ross"));
    }

    #[test]
    fn due_in_an_earlier_year_is_dropped() {
        let c = ctx(Some((2026, 10, 26)), &[]);
        assert_eq!(c.check_due(Some("October 26, 2023".into()), None), None);
        assert_eq!(c.check_due(Some("2024-10-27".into()), None), None);
        assert_eq!(c.check_due(Some("2026-10-01".into()), None), None);
        assert_eq!(
            c.check_due(Some("by Friday".into()), None).as_deref(),
            Some("by Friday")
        );
        assert_eq!(
            c.check_due(Some("2026-11-06".into()), None).as_deref(),
            Some("2026-11-06")
        );
    }

    #[test]
    fn due_with_an_unspoken_year_is_dropped() {
        let c = ctx(Some((2026, 10, 30)), &[]);
        let transcript = "[00:10] Tom: let's get the plan done in Q1";
        assert_eq!(c.check_due(Some("Q1 2027".into()), Some(transcript)), None);
        assert_eq!(
            c.check_due(Some("Q1".into()), Some(transcript)).as_deref(),
            Some("Q1")
        );
        let said = "[00:10] Tom: we want this live by March 2027";
        assert_eq!(
            c.check_due(Some("March 2027".into()), Some(said))
                .as_deref(),
            Some("March 2027")
        );
    }

    #[test]
    fn prompt_block_names_date_and_people() {
        let c = ctx(Some((2026, 10, 26)), &["Seb Burrell", "Ross Smith"]);
        let block = c.prompt_block();
        assert!(block.contains("Monday 26 October 2026"), "{block}");
        assert!(
            block.contains("Participants: Seb Burrell, Ross Smith"),
            "{block}"
        );
        assert_eq!(MeetingContext::default().prompt_block(), "");
    }

    #[test]
    fn calendar_person_names_from_email() {
        assert_eq!(
            person_name(None, Some("dom.bramley@example.com")).as_deref(),
            Some("Dom Bramley")
        );
        assert_eq!(person_name(None, Some("info@example.com")), None);
        assert_eq!(
            person_name(Some("Tom Bishop"), Some("tb@ibm.com")).as_deref(),
            Some("Tom Bishop")
        );
        assert_eq!(person_name(Some("tb@ibm.com"), None), None);
    }

    #[test]
    fn transcript_lines_carry_speakers() {
        let seg = |start: f64, speaker: Option<&str>, text: &str| MeetingTranscript {
            id: String::new(),
            text: text.to_string(),
            timestamp: "10:00:00".to_string(),
            audio_start_time: Some(start),
            audio_end_time: None,
            duration: None,
            speaker: speaker.map(str::to_string),
            voice_profile_id: None,
            source: None,
            confidence: None,
        };
        let text = build_transcript_text(&[
            seg(0.0, Some("Seb Burrell"), "Hello"),
            seg(65.0, None, "World"),
            seg(70.0, Some("Speaker 2"), "  "),
        ]);
        assert_eq!(text, "[00:00] Seb Burrell: Hello\n[01:05] World");
    }
}
