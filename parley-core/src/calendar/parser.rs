use std::io::BufReader;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use ical::parser::ical::component::IcalEvent;
use ical::property::Property;
use rrule::RRuleSet;

use super::models::{Attendee, ParsedEvent};

/// Parse an ICS document string into a list of master events. Recurring
/// events are returned as a single master entry whose `rrule_block` carries
/// the DTSTART + RRULE block — expansion happens in `expand_event_window`
/// at fetch time. Override events (those carrying RECURRENCE-ID) are also
/// returned as their own ParsedEvent so the repository can use them to
/// supersede generated occurrences.
pub fn parse_ics(ics: &str) -> Result<Vec<ParsedEvent>> {
    let buf = BufReader::new(ics.as_bytes());
    let parser = ical::IcalParser::new(buf);

    let mut events = Vec::new();
    for cal in parser {
        let cal = cal.map_err(|e| anyhow!("ICS parse error: {}", e))?;
        for ev in cal.events {
            match parse_event(&ev) {
                Ok(parsed) => events.push(parsed),
                Err(e) => {
                    log::warn!(
                        "skipping malformed VEVENT (uid={:?}): {}",
                        prop_value(&ev, "UID"),
                        e
                    );
                }
            }
        }
    }
    Ok(events)
}

fn parse_event(ev: &IcalEvent) -> Result<ParsedEvent> {
    let uid = prop_value(ev, "UID")
        .context("VEVENT missing UID")?
        .to_string();

    let (start_at, start_is_all_day) = parse_datetime_property(ev, "DTSTART")?;
    let end_at = match parse_optional_datetime_property(ev, "DTEND")? {
        Some((dt, _)) => dt,
        None => match parse_optional_duration(ev) {
            Some(dur) => start_at + dur,
            None => {
                if start_is_all_day {
                    start_at + Duration::days(1)
                } else {
                    start_at + Duration::minutes(30)
                }
            }
        },
    };

    let recurrence_id = parse_optional_datetime_property(ev, "RECURRENCE-ID")?
        .map(|(dt, _)| dt);

    let summary = prop_value(ev, "SUMMARY").map(|s| s.to_string());
    let description = prop_value(ev, "DESCRIPTION").map(|s| s.to_string());
    let location = prop_value(ev, "LOCATION").map(|s| s.to_string());

    let (organizer_name, organizer_email) = parse_organizer(ev);
    let attendees = parse_attendees(ev);

    let rrule_block = build_rrule_block(ev);
    let raw_ics = serialize_event(ev);

    Ok(ParsedEvent {
        ics_uid: uid,
        recurrence_id,
        summary,
        description,
        location,
        organizer_name,
        organizer_email,
        start_at,
        end_at,
        is_all_day: start_is_all_day,
        attendees,
        rrule_block,
        raw_ics,
    })
}

fn prop<'a>(ev: &'a IcalEvent, name: &str) -> Option<&'a Property> {
    ev.properties.iter().find(|p| p.name == name)
}

fn prop_value<'a>(ev: &'a IcalEvent, name: &str) -> Option<&'a str> {
    prop(ev, name).and_then(|p| p.value.as_deref())
}

fn parse_datetime_property(
    ev: &IcalEvent,
    name: &str,
) -> Result<(DateTime<Utc>, bool)> {
    parse_optional_datetime_property(ev, name)?
        .ok_or_else(|| anyhow!("missing {} property", name))
}

fn parse_optional_datetime_property(
    ev: &IcalEvent,
    name: &str,
) -> Result<Option<(DateTime<Utc>, bool)>> {
    let Some(p) = prop(ev, name) else {
        return Ok(None);
    };
    let value = p
        .value
        .as_deref()
        .ok_or_else(|| anyhow!("{} has no value", name))?;
    let params = p.params.as_ref();

    let value_type = params
        .and_then(|ps| ps.iter().find(|(k, _)| k.eq_ignore_ascii_case("VALUE")))
        .and_then(|(_, v)| v.first().cloned())
        .unwrap_or_default();

    if value_type.eq_ignore_ascii_case("DATE") || value.len() == 8 {
        let d = NaiveDate::parse_from_str(value, "%Y%m%d")
            .with_context(|| format!("invalid DATE in {}: {}", name, value))?;
        let dt = d.and_hms_opt(0, 0, 0).unwrap();
        return Ok(Some((Utc.from_utc_datetime(&dt), true)));
    }

    let tzid = params
        .and_then(|ps| ps.iter().find(|(k, _)| k.eq_ignore_ascii_case("TZID")))
        .and_then(|(_, v)| v.first().cloned());

    let dt = parse_naive_or_utc(value)
        .with_context(|| format!("invalid DATE-TIME in {}: {}", name, value))?;

    Ok(Some((apply_tz(dt, value, tzid.as_deref())?, false)))
}

fn parse_naive_or_utc(value: &str) -> Result<NaiveDateTime> {
    if let Some(stripped) = value.strip_suffix('Z') {
        return NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S")
            .map_err(|e| e.into());
    }
    NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").map_err(|e| e.into())
}

fn apply_tz(naive: NaiveDateTime, raw: &str, tzid: Option<&str>) -> Result<DateTime<Utc>> {
    if raw.ends_with('Z') {
        return Ok(Utc.from_utc_datetime(&naive));
    }
    if let Some(tzid) = tzid {
        let tz: Tz = tzid
            .parse()
            .map_err(|e| anyhow!("unknown TZID {}: {}", tzid, e))?;
        return tz
            .from_local_datetime(&naive)
            .single()
            .ok_or_else(|| anyhow!("ambiguous local time {} in {}", naive, tzid))
            .map(|dt| dt.with_timezone(&Utc));
    }
    // Floating local time — interpret as UTC. Acceptable fallback for
    // public ICS feeds that omit TZID; users can fix on the source side.
    Ok(Utc.from_utc_datetime(&naive))
}

fn parse_optional_duration(ev: &IcalEvent) -> Option<Duration> {
    let raw = prop_value(ev, "DURATION")?;
    parse_iso_duration(raw)
}

fn parse_iso_duration(raw: &str) -> Option<Duration> {
    // Minimal subset: PT#H#M#S, P#D, P#W. Sufficient for typical ICS feeds.
    let (sign, rest) = if let Some(s) = raw.strip_prefix('-') {
        (-1, s)
    } else if let Some(s) = raw.strip_prefix('+') {
        (1, s)
    } else {
        (1, raw)
    };
    let rest = rest.strip_prefix('P')?;
    let mut total = Duration::zero();
    let mut buf = String::new();
    let mut in_time = false;
    for ch in rest.chars() {
        if ch == 'T' {
            in_time = true;
            continue;
        }
        if ch.is_ascii_digit() {
            buf.push(ch);
            continue;
        }
        let n: i64 = buf.parse().ok()?;
        buf.clear();
        let unit = match (ch, in_time) {
            ('W', false) => Duration::weeks(n),
            ('D', false) => Duration::days(n),
            ('H', true) => Duration::hours(n),
            ('M', true) => Duration::minutes(n),
            ('S', true) => Duration::seconds(n),
            _ => return None,
        };
        total = total + unit;
    }
    Some(total * sign as i32)
}

/// Strip the ".ogcs" marker Outlook Google Calendar Sync appends to
/// addresses it manages (e.g. "bob@example.com.ogcs" — OGCS's "don't send
/// invites through Google" tag, not part of the real address). Applied to
/// every email-ish value we ingest so downstream consumers (attendee
/// suggestions, voice-profile emails) never see the synthetic suffix.
fn strip_ogcs_suffix(value: &str) -> &str {
    let trimmed = value.trim();
    let len = trimmed.len();
    if len >= 5
        && trimmed.is_char_boundary(len - 5)
        && trimmed[len - 5..].eq_ignore_ascii_case(".ogcs")
    {
        &trimmed[..len - 5]
    } else {
        trimmed
    }
}

fn parse_organizer(ev: &IcalEvent) -> (Option<String>, Option<String>) {
    let Some(p) = prop(ev, "ORGANIZER") else {
        return (None, None);
    };
    let email = p
        .value
        .as_deref()
        .and_then(|v| v.strip_prefix("mailto:").or(Some(v)))
        .map(|s| strip_ogcs_suffix(s).to_string());
    let name = p
        .params
        .as_ref()
        .and_then(|ps| ps.iter().find(|(k, _)| k.eq_ignore_ascii_case("CN")))
        .and_then(|(_, v)| v.first().cloned())
        .map(|n| strip_ogcs_suffix(&n).to_string());
    (name, email)
}

fn parse_attendees(ev: &IcalEvent) -> Vec<Attendee> {
    ev.properties
        .iter()
        .filter(|p| p.name == "ATTENDEE")
        .map(|p| {
            let email = p
                .value
                .as_deref()
                .and_then(|v| v.strip_prefix("mailto:").or(Some(v)))
                .map(|s| strip_ogcs_suffix(s).to_string());
            let mut name = None;
            let mut role = None;
            let mut status = None;
            if let Some(ps) = &p.params {
                for (k, vs) in ps {
                    let v = vs.first().cloned();
                    match k.to_ascii_uppercase().as_str() {
                        // CN is stripped too: when the calendar has no display
                        // name, OGCS-synced feeds put the tagged address there.
                        "CN" => name = v.map(|n| strip_ogcs_suffix(&n).to_string()),
                        "ROLE" => role = v,
                        "PARTSTAT" => status = v,
                        _ => {}
                    }
                }
            }
            Attendee {
                name,
                email,
                role,
                status,
                is_organizer: false,
            }
        })
        .collect()
}

/// Reconstruct a minimal `DTSTART:...\nRRULE:...` block for the rrule crate.
/// Returns None if the event has no RRULE.
fn build_rrule_block(ev: &IcalEvent) -> Option<String> {
    let rrule = prop(ev, "RRULE")?;
    let rrule_value = rrule.value.as_deref()?;

    let dtstart = prop(ev, "DTSTART")?;
    let dtstart_str = serialize_property(dtstart);

    let mut block = String::new();
    block.push_str(&dtstart_str);
    block.push('\n');
    block.push_str(&format!("RRULE:{}", rrule_value));

    for name in ["EXDATE", "RDATE", "EXRULE"] {
        for p in ev.properties.iter().filter(|p| p.name == name) {
            block.push('\n');
            block.push_str(&serialize_property(p));
        }
    }

    Some(block)
}

fn serialize_property(p: &Property) -> String {
    let mut s = p.name.clone();
    if let Some(params) = &p.params {
        for (k, v) in params {
            s.push(';');
            s.push_str(k);
            s.push('=');
            s.push_str(&v.join(","));
        }
    }
    s.push(':');
    if let Some(v) = &p.value {
        s.push_str(v);
    }
    s
}

fn serialize_event(ev: &IcalEvent) -> String {
    let mut s = String::from("BEGIN:VEVENT\n");
    for p in &ev.properties {
        s.push_str(&serialize_property(p));
        s.push('\n');
    }
    s.push_str("END:VEVENT");
    s
}

/// Expand a master event's RRULE over the given window (UTC). For non-
/// recurring events this returns a single occurrence at the master start
/// (or empty if the master is outside the window).
pub fn expand_event_window(
    master: &ParsedEvent,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    max_occurrences: u16,
) -> Vec<DateTime<Utc>> {
    if let Some(block) = &master.rrule_block {
        match block.parse::<RRuleSet>() {
            Ok(set) => {
                let result = set
                    .after(rrule_dt(from))
                    .before(rrule_dt(to))
                    .all(max_occurrences);
                return result
                    .dates
                    .into_iter()
                    .map(|dt| dt.with_timezone(&Utc))
                    .collect();
            }
            Err(e) => {
                log::warn!(
                    "RRULE parse failed for uid={} ({}); falling back to single occurrence",
                    master.ics_uid,
                    e
                );
            }
        }
    }
    if master.start_at >= from && master.start_at <= to {
        vec![master.start_at]
    } else {
        Vec::new()
    }
}

fn rrule_dt(dt: DateTime<Utc>) -> DateTime<rrule::Tz> {
    dt.with_timezone(&rrule::Tz::UTC)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_ICS: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Test//Test//EN\r\n\
BEGIN:VEVENT\r\n\
UID:abc-123@test\r\n\
DTSTAMP:20260506T120000Z\r\n\
DTSTART:20260506T143000Z\r\n\
DTEND:20260506T153000Z\r\n\
SUMMARY:Team Standup\r\n\
DESCRIPTION:Daily sync\r\n\
LOCATION:Zoom\r\n\
ORGANIZER;CN=Alice Example:mailto:alice@example.com\r\n\
ATTENDEE;CN=Bob;ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED:mailto:bob@example.com\r\n\
ATTENDEE;CN=Carol;ROLE=OPT-PARTICIPANT;PARTSTAT=TENTATIVE:mailto:carol@example.com\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn parses_simple_event() {
        let events = parse_ics(SAMPLE_ICS).unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.ics_uid, "abc-123@test");
        assert_eq!(e.summary.as_deref(), Some("Team Standup"));
        assert_eq!(e.location.as_deref(), Some("Zoom"));
        assert_eq!(e.organizer_email.as_deref(), Some("alice@example.com"));
        assert_eq!(e.organizer_name.as_deref(), Some("Alice Example"));
        assert_eq!(e.attendees.len(), 2);
        assert!(!e.is_all_day);
        assert_eq!(e.rrule_block, None);
    }

    /// OGCS (Outlook Google Calendar Sync) tags the addresses it manages
    /// with a trailing ".ogcs"; the parser must hand back real addresses.
    #[test]
    fn strips_ogcs_suffix_from_organizer_and_attendees() {
        let ics = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Test//Test//EN\r\n\
BEGIN:VEVENT\r\n\
UID:ogcs-1@test\r\n\
DTSTAMP:20260506T120000Z\r\n\
DTSTART:20260506T143000Z\r\n\
DTEND:20260506T153000Z\r\n\
SUMMARY:Synced Meeting\r\n\
ORGANIZER;CN=Alice Example:mailto:alice@example.com.OGCS\r\n\
ATTENDEE;CN=bob@example.com.ogcs;ROLE=REQ-PARTICIPANT:mailto:bob@example.com.ogcs\r\n\
ATTENDEE;CN=Carol:mailto:carol@example.com\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = parse_ics(ics).unwrap();
        let e = &events[0];
        assert_eq!(e.organizer_email.as_deref(), Some("alice@example.com"));
        // Suffix stripped from the email AND from a CN that's just the
        // tagged address; an untagged attendee passes through untouched.
        assert_eq!(e.attendees[0].email.as_deref(), Some("bob@example.com"));
        assert_eq!(e.attendees[0].name.as_deref(), Some("bob@example.com"));
        assert_eq!(e.attendees[1].email.as_deref(), Some("carol@example.com"));
        assert_eq!(e.attendees[1].name.as_deref(), Some("Carol"));
    }

    #[test]
    fn strip_ogcs_suffix_is_case_insensitive_and_conservative() {
        assert_eq!(strip_ogcs_suffix("bob@example.com.ogcs"), "bob@example.com");
        assert_eq!(strip_ogcs_suffix("bob@example.com.OGCS"), "bob@example.com");
        assert_eq!(strip_ogcs_suffix(" bob@example.com.ogcs "), "bob@example.com");
        // Untagged values are only trimmed.
        assert_eq!(strip_ogcs_suffix("bob@example.com"), "bob@example.com");
        assert_eq!(strip_ogcs_suffix("Alice Example"), "Alice Example");
        // Degenerate inputs don't panic or over-strip.
        assert_eq!(strip_ogcs_suffix(".ogcs"), "");
        assert_eq!(strip_ogcs_suffix("ogcs"), "ogcs");
        assert_eq!(strip_ogcs_suffix(""), "");
    }

    #[test]
    fn expands_recurring_event() {
        let ics = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Test//Test//EN\r\n\
BEGIN:VEVENT\r\n\
UID:rec-1@test\r\n\
DTSTAMP:20260506T120000Z\r\n\
DTSTART:20260506T140000Z\r\n\
DTEND:20260506T150000Z\r\n\
SUMMARY:Weekly\r\n\
RRULE:FREQ=WEEKLY;COUNT=3\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = parse_ics(ics).unwrap();
        let master = &events[0];
        assert!(master.rrule_block.is_some());
        let occurrences = expand_event_window(
            master,
            "2026-05-01T00:00:00Z".parse().unwrap(),
            "2026-07-01T00:00:00Z".parse().unwrap(),
            100,
        );
        assert_eq!(occurrences.len(), 3);
    }

    #[test]
    fn parses_iso_duration() {
        assert_eq!(parse_iso_duration("PT1H30M"), Some(Duration::minutes(90)));
        assert_eq!(parse_iso_duration("P1D"), Some(Duration::days(1)));
        assert_eq!(parse_iso_duration("P1W"), Some(Duration::weeks(1)));
    }
}
