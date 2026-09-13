//! Pure logic for the recording page: transcript ordering/dedupe, elapsed
//! time formatting, level-meter extraction, and phase -> button
//! enablement. Kept free of GPUI types so it's unit-testable on its own
//! (`cargo test -p parley-gpui`).

use chrono::{DateTime, Local};
use std::cmp::Ordering;
use std::path::Path;

use parley_core::audio::recording_phase::RecordingPhase;
use parley_core::audio::simple_level_monitor::AudioLevelData;

// ============================================================================
// Transcript ordering / dedupe
// ============================================================================

/// One row of the live transcript, built from either a final
/// `transcript-update` (`parley_core::audio::transcription::worker::TranscriptUpdate`)
/// or the in-progress `transcript-partial` overlay
/// (`parley_core::audio::transcription::partial_worker`).
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptRow {
    pub sequence_id: u64,
    /// Seconds from recording start — the true chronological anchor across
    /// sources (mirrors `frontend/src/lib/transcriptOrder.ts`'s rationale:
    /// dual-VAD segments can complete out of order).
    pub audio_start_time: f64,
    pub speaker: String,
    pub text: String,
    /// "mic" | "system".
    pub source: String,
}

fn order_key(row: &TranscriptRow) -> (f64, u64) {
    (row.audio_start_time, row.sequence_id)
}

/// Chronological ordering per `frontend/src/lib/transcriptOrder.ts`'s
/// `compareSegments`: `audio_start_time` first, `sequence_id` as tiebreaker.
pub fn compare_rows(a: &TranscriptRow, b: &TranscriptRow) -> Ordering {
    order_key(a)
        .partial_cmp(&order_key(b))
        .unwrap_or(Ordering::Equal)
}

/// How [`upsert_row`] changed the list — enough for the view to translate
/// into the minimal `MessageScrollerState::splice`/`append` calls instead of
/// always replacing the whole list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowChange {
    /// A new row landed at the end of the list (the common case).
    Appended,
    /// A new row was inserted mid-list (an earlier segment finished late).
    InsertedAt(usize),
    /// An existing row (same `sequence_id`) was updated without moving.
    ReplacedAt(usize),
    /// An existing row moved because its order key changed.
    Repositioned { removed_at: usize, inserted_at: usize },
}

/// Upsert `row` into `rows` (assumed already sorted per [`compare_rows`]) by
/// `sequence_id`, mirroring `transcriptOrder.ts`'s `upsertSorted`: a row
/// with a matching `sequence_id` is replaced in place if its order key is
/// unchanged, otherwise removed and re-inserted at its new sorted position;
/// a row with no match is inserted directly.
pub fn upsert_row(rows: &mut Vec<TranscriptRow>, row: TranscriptRow) -> RowChange {
    if let Some(idx) = rows.iter().position(|r| r.sequence_id == row.sequence_id) {
        if order_key(&rows[idx]) == order_key(&row) {
            rows[idx] = row;
            return RowChange::ReplacedAt(idx);
        }
        rows.remove(idx);
        let insert_at = rows.partition_point(|r| compare_rows(r, &row) != Ordering::Greater);
        rows.insert(insert_at, row);
        return RowChange::Repositioned {
            removed_at: idx,
            inserted_at: insert_at,
        };
    }
    let insert_at = rows.partition_point(|r| compare_rows(r, &row) != Ordering::Greater);
    rows.insert(insert_at, row);
    if insert_at == rows.len() - 1 {
        RowChange::Appended
    } else {
        RowChange::InsertedAt(insert_at)
    }
}

// ============================================================================
// Elapsed time formatting
// ============================================================================

/// `mm:ss`, or `h:mm:ss` past one hour — used both for the header's overall
/// elapsed time and each transcript row's recording-relative timestamp.
pub fn format_elapsed(total_secs: u64) -> String {
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

// ============================================================================
// Default meeting title
// ============================================================================

/// Client-side default meeting title, matching the frontend's
/// `useRecordingStart.ts` generated format ("Meeting DD_MM_YY_HH_MM_SS").
pub fn default_meeting_title(now: DateTime<Local>) -> String {
    format!("Meeting {}", now.format("%d_%m_%y_%H_%M_%S"))
}

/// The `RecordingArgs { save_path }` argument `stop()` needs, built the same
/// way the Tauri shell's Stop button (`RecordingTopBar.tsx`) and the core's
/// own `spawn_fatal_error_stop` fallback do: `<app-data-dir>/recording-<ts>.wav`.
/// `stop()` only uses this path's parent directory existing — the actual
/// audio file is written elsewhere — but the format is kept identical for
/// consistency across shells.
pub fn stop_save_path(app_data_dir: &Path, now: DateTime<Local>) -> String {
    let timestamp = now.format("%Y-%m-%dT%H-%M-%S").to_string();
    app_data_dir
        .join(format!("recording-{timestamp}.wav"))
        .to_string_lossy()
        .to_string()
}

// ============================================================================
// Level meters
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Levels {
    pub mic: f32,
    pub system: f32,
}

/// Pull mic/system RMS levels out of an `audio-levels` event's
/// `AudioLevelUpdate.levels` (role keyed by `device_name`: "mic" | "system").
pub fn levels_from_update(levels: &[AudioLevelData]) -> Levels {
    let mut out = Levels::default();
    for l in levels {
        match l.device_name.as_str() {
            "mic" => out.mic = l.rms_level,
            "system" => out.system = l.rms_level,
            _ => {}
        }
    }
    out
}

// ============================================================================
// Phase -> button enablement
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ButtonState {
    pub can_start: bool,
    pub can_pause: bool,
    pub can_resume: bool,
    pub can_stop: bool,
}

/// Which recording controls are enabled for a given canonical
/// `RecordingPhase` (`parley_core::audio::recording_phase`). Transient
/// phases (`Starting`/`Stopping`/`Finalising`) disable everything — matches
/// the frontend's `RecordingStateContext` treating those as busy states.
pub fn button_state(phase: RecordingPhase) -> ButtonState {
    use RecordingPhase::*;
    match phase {
        Idle | Error => ButtonState {
            can_start: true,
            ..Default::default()
        },
        Starting | Stopping | Finalising => ButtonState::default(),
        Recording => ButtonState {
            can_pause: true,
            can_stop: true,
            ..Default::default()
        },
        Paused => ButtonState {
            can_resume: true,
            can_stop: true,
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(seq: u64, start: f64, text: &str) -> TranscriptRow {
        TranscriptRow {
            sequence_id: seq,
            audio_start_time: start,
            speaker: "You".to_string(),
            text: text.to_string(),
            source: "mic".to_string(),
        }
    }

    #[test]
    fn upsert_inserts_in_chronological_order_even_out_of_arrival_order() {
        let mut rows = Vec::new();
        upsert_row(&mut rows, row(2, 5.0, "second"));
        upsert_row(&mut rows, row(1, 1.0, "first"));
        upsert_row(&mut rows, row(3, 10.0, "third"));

        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["first", "second", "third"]);
    }

    #[test]
    fn upsert_replaces_a_row_with_the_same_sequence_id_in_place() {
        let mut rows = Vec::new();
        upsert_row(&mut rows, row(1, 1.0, "first"));
        upsert_row(&mut rows, row(2, 2.0, "second"));
        let change = upsert_row(&mut rows, row(1, 1.0, "first (revised)"));

        assert_eq!(change, RowChange::ReplacedAt(0));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "first (revised)");
        assert_eq!(rows[1].text, "second");
    }

    #[test]
    fn upsert_repositions_a_row_whose_order_key_changed() {
        let mut rows = Vec::new();
        upsert_row(&mut rows, row(1, 1.0, "first"));
        upsert_row(&mut rows, row(2, 2.0, "second"));
        // Same sequence_id as "first", but its audio_start_time moved past
        // "second" — must be relocated, not just replaced in place.
        let change = upsert_row(&mut rows, row(1, 3.0, "first (moved later)"));

        assert_eq!(
            change,
            RowChange::Repositioned {
                removed_at: 0,
                inserted_at: 1
            }
        );
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["second", "first (moved later)"]);
    }

    #[test]
    fn duplicate_sequence_id_and_order_key_is_a_true_no_op_replace() {
        let mut rows = Vec::new();
        upsert_row(&mut rows, row(1, 1.0, "first"));
        let change = upsert_row(&mut rows, row(1, 1.0, "first"));
        assert_eq!(change, RowChange::ReplacedAt(0));
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn upsert_reports_appended_for_the_common_in_order_case() {
        let mut rows = Vec::new();
        assert_eq!(upsert_row(&mut rows, row(1, 1.0, "first")), RowChange::Appended);
        assert_eq!(upsert_row(&mut rows, row(2, 2.0, "second")), RowChange::Appended);
    }

    #[test]
    fn upsert_reports_inserted_at_for_a_late_out_of_order_arrival() {
        let mut rows = Vec::new();
        upsert_row(&mut rows, row(1, 1.0, "first"));
        upsert_row(&mut rows, row(3, 3.0, "third"));
        let change = upsert_row(&mut rows, row(2, 2.0, "second"));
        assert_eq!(change, RowChange::InsertedAt(1));
    }

    #[test]
    fn elapsed_formats_mm_ss_under_an_hour() {
        assert_eq!(format_elapsed(0), "00:00");
        assert_eq!(format_elapsed(5), "00:05");
        assert_eq!(format_elapsed(65), "01:05");
        assert_eq!(format_elapsed(3599), "59:59");
    }

    #[test]
    fn elapsed_formats_h_mm_ss_past_an_hour() {
        assert_eq!(format_elapsed(3600), "1:00:00");
        assert_eq!(format_elapsed(3661), "1:01:01");
    }

    /// Build a fixed local instant directly (rather than parsing a UTC
    /// RFC3339 string and converting), so the test's expected string
    /// doesn't depend on the test runner's local timezone offset.
    fn fixed_local_instant() -> DateTime<Local> {
        use chrono::TimeZone as _;
        Local.with_ymd_and_hms(2026, 9, 11, 8, 25, 23).unwrap()
    }

    #[test]
    fn default_title_matches_frontend_format() {
        assert_eq!(
            default_meeting_title(fixed_local_instant()),
            "Meeting 11_09_26_08_25_23"
        );
    }

    #[test]
    fn stop_save_path_matches_the_shared_recording_dash_timestamp_wav_shape() {
        let path = stop_save_path(Path::new("/tmp/app-data"), fixed_local_instant());
        assert_eq!(path, "/tmp/app-data/recording-2026-09-11T08-25-23.wav");
    }

    #[test]
    fn levels_are_read_by_role_key() {
        let update = vec![
            AudioLevelData {
                device_name: "mic".to_string(),
                device_type: "input".to_string(),
                rms_level: 0.42,
                peak_level: 0.9,
                is_active: true,
            },
            AudioLevelData {
                device_name: "system".to_string(),
                device_type: "output".to_string(),
                rms_level: 0.13,
                peak_level: 0.5,
                is_active: false,
            },
        ];
        let levels = levels_from_update(&update);
        assert_eq!(levels.mic, 0.42);
        assert_eq!(levels.system, 0.13);
    }

    #[test]
    fn levels_default_to_zero_when_a_source_is_missing() {
        let levels = levels_from_update(&[]);
        assert_eq!(levels, Levels::default());
    }

    #[test]
    fn button_state_matches_the_recording_lifecycle() {
        assert_eq!(
            button_state(RecordingPhase::Idle),
            ButtonState {
                can_start: true,
                ..Default::default()
            }
        );
        assert_eq!(
            button_state(RecordingPhase::Error),
            ButtonState {
                can_start: true,
                ..Default::default()
            }
        );
        assert_eq!(button_state(RecordingPhase::Starting), ButtonState::default());
        assert_eq!(
            button_state(RecordingPhase::Recording),
            ButtonState {
                can_pause: true,
                can_stop: true,
                ..Default::default()
            }
        );
        assert_eq!(
            button_state(RecordingPhase::Paused),
            ButtonState {
                can_resume: true,
                can_stop: true,
                ..Default::default()
            }
        );
        assert_eq!(button_state(RecordingPhase::Stopping), ButtonState::default());
        assert_eq!(button_state(RecordingPhase::Finalising), ButtonState::default());
    }
}
