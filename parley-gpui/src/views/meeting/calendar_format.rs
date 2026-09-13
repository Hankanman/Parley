//! Pure formatting/sorting helpers for the meeting page's calendar-event
//! panel and picker — mirrors
//! `frontend/src/components/MeetingDetails/CalendarEventPanel.tsx`'s
//! `formatTimeRange` and `CalendarEventPicker.tsx`'s `formatOffset` /
//! proximity sort, in local time.

use chrono::{DateTime, Local, Utc};

/// "Wed, Sep 11 · 2:00 PM – 3:00 PM" (same local day), or
/// "Wed, Sep 11 2:00 PM – Thu, Sep 12 3:00 PM" (spanning days) — mirrors
/// `CalendarEventPanel.tsx`'s `formatTimeRange`.
pub fn format_time_range(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    let start = start.with_timezone(&Local);
    let end = end.with_timezone(&Local);
    let same_day = start.date_naive() == end.date_naive();
    let date_fmt = start.format("%a, %b %-d");
    let start_time = start.format("%-I:%M %p");
    if same_day {
        let end_time = end.format("%-I:%M %p");
        format!("{date_fmt} · {start_time} – {end_time}")
    } else {
        format!("{date_fmt} {start_time} – {}", end.format("%a, %b %-d %-I:%M %p"))
    }
}

/// "at recording time" / "5m before" / "2h after" / "3d after" — mirrors
/// `CalendarEventPicker.tsx`'s `formatOffset`.
pub fn format_offset(event_start: DateTime<Utc>, anchor: DateTime<Utc>) -> String {
    let diff = event_start.signed_duration_since(anchor);
    let abs_ms = diff.num_milliseconds().unsigned_abs();
    if abs_ms < 60_000 {
        return "at recording time".to_string();
    }
    let sign = if diff.num_milliseconds() < 0 { "before" } else { "after" };
    if abs_ms < 3_600_000 {
        return format!("{}m {sign}", (abs_ms as f64 / 60_000.0).round() as i64);
    }
    if abs_ms < 86_400_000 {
        return format!("{}h {sign}", (abs_ms as f64 / 3_600_000.0).round() as i64);
    }
    format!("{}d {sign}", (abs_ms as f64 / 86_400_000.0).round() as i64)
}

/// Sort picker results by proximity to `anchor` — closest match first,
/// mirroring the picker's sort in `CalendarEventPicker.tsx`.
pub fn sort_by_proximity<T>(events: &mut [T], anchor: DateTime<Utc>, start_of: impl Fn(&T) -> DateTime<Utc>) {
    events.sort_by_key(|e| (start_of(e) - anchor).num_milliseconds().abs());
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn dt(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    #[test]
    fn same_day_range_uses_short_form() {
        // A 1-hour range starting at local noon is on the same local day
        // under any real-world UTC offset (-12..+14).
        let noon_utc = dt(2026, 9, 11, 12, 0);
        let s = noon_utc;
        let e = noon_utc + chrono::Duration::hours(1);
        assert_eq!(s.with_timezone(&Local).date_naive(), e.with_timezone(&Local).date_naive());
        let out = format_time_range(s, e);
        assert!(out.contains('·'), "expected a same-day separator in {out:?}");
    }

    #[test]
    fn spanning_days_uses_long_form() {
        // A 30-hour range guarantees different local calendar days under any
        // real-world UTC offset (max spread is 26h, from UTC-12 to UTC+14).
        let s = dt(2026, 9, 11, 12, 0);
        let e = s + chrono::Duration::hours(30);
        assert_ne!(s.with_timezone(&Local).date_naive(), e.with_timezone(&Local).date_naive());
        let out = format_time_range(s, e);
        assert!(!out.contains('·'), "expected the long form (no ·) in {out:?}");
    }

    #[test]
    fn offset_within_a_minute_is_at_recording_time() {
        let anchor = dt(2026, 9, 11, 14, 0);
        let event = dt(2026, 9, 11, 14, 0);
        assert_eq!(format_offset(event, anchor), "at recording time");
    }

    #[test]
    fn offset_minutes_before() {
        let anchor = dt(2026, 9, 11, 14, 0);
        let event = dt(2026, 9, 11, 13, 55);
        assert_eq!(format_offset(event, anchor), "5m before");
    }

    #[test]
    fn offset_hours_after() {
        let anchor = dt(2026, 9, 11, 14, 0);
        let event = dt(2026, 9, 11, 16, 0);
        assert_eq!(format_offset(event, anchor), "2h after");
    }

    #[test]
    fn offset_days_after() {
        let anchor = dt(2026, 9, 11, 14, 0);
        let event = dt(2026, 9, 14, 14, 0);
        assert_eq!(format_offset(event, anchor), "3d after");
    }

    #[test]
    fn proximity_sort_orders_closest_first() {
        let anchor = dt(2026, 9, 11, 14, 0);
        let mut events = vec![dt(2026, 9, 13, 14, 0), dt(2026, 9, 11, 14, 5), dt(2026, 9, 10, 14, 0)];
        sort_by_proximity(&mut events, anchor, |e| *e);
        assert_eq!(events[0], dt(2026, 9, 11, 14, 5));
    }
}
