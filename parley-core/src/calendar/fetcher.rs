use std::time::Duration as StdDuration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, Utc};

use super::models::ParsedEvent;
use super::parser;

/// How far back / forward to materialise occurrences of recurring events
/// when a source is refreshed. The +90d horizon covers typical meeting
/// scheduling; the -7d window keeps recently-recorded meetings linkable.
const WINDOW_BACK_DAYS: i64 = 7;
const WINDOW_FORWARD_DAYS: i64 = 90;
const MAX_OCCURRENCES_PER_RULE: u16 = 200;

/// One materialised event ready for upsert.
#[derive(Debug, Clone)]
pub struct OccurrenceForUpsert {
    pub master: ParsedEvent,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    pub recurrence_id: Option<DateTime<Utc>>,
}

/// Fetch + parse + expand a single ICS URL. Returns a list of
/// occurrences within the active window, plus override events
/// (those carrying RECURRENCE-ID) so the caller can store both and
/// let the unique constraint resolve override-vs-generated conflicts.
pub async fn fetch_and_expand(url: &str) -> Result<Vec<OccurrenceForUpsert>> {
    let client = reqwest::Client::builder()
        .timeout(StdDuration::from_secs(30))
        .user_agent("Parley/0.4 (+https://github.com/Hankanman/Parley)")
        .build()
        .context("building reqwest client")?;

    let resp = client.get(url).send().await.context("fetching ICS URL")?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "ICS endpoint returned {} ({})",
            resp.status(),
            url
        ));
    }
    let body = resp.text().await.context("reading ICS body")?;
    let masters = parser::parse_ics(&body)?;

    let now = Utc::now();
    let from = now - Duration::days(WINDOW_BACK_DAYS);
    let to = now + Duration::days(WINDOW_FORWARD_DAYS);

    // Split into master events (no RECURRENCE-ID) and overrides.
    let (overrides, masters): (Vec<_>, Vec<_>) = masters
        .into_iter()
        .partition(|e| e.recurrence_id.is_some());

    let mut out = Vec::new();
    for master in masters {
        let occurrences =
            parser::expand_event_window(&master, from, to, MAX_OCCURRENCES_PER_RULE);
        let duration = master.end_at - master.start_at;
        let is_recurring = master.rrule_block.is_some();
        for start in occurrences {
            out.push(OccurrenceForUpsert {
                master: master.clone(),
                start_at: start,
                end_at: start + duration,
                recurrence_id: if is_recurring { Some(start) } else { None },
            });
        }
    }
    for ov in overrides {
        // Override events keep their explicit RECURRENCE-ID so the unique
        // index (source_id, ics_uid, recurrence_id) replaces the generated
        // occurrence rather than creating a duplicate row.
        out.push(OccurrenceForUpsert {
            start_at: ov.start_at,
            end_at: ov.end_at,
            recurrence_id: ov.recurrence_id,
            master: ov,
        });
    }
    Ok(out)
}
