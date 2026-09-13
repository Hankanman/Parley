// audio/transcription/echo_dedup.rs
//
// Cross-source echo suppression.
//
// The pipeline runs VAD + transcription independently on the mic stream and
// the system-audio stream (see worker.rs — chunks carry `chunk.device_type`).
// When the local user is on speakers rather than headphones, a remote call
// participant's voice comes out the speakers (captured as `System`) *and*
// bleeds acoustically into the room, back into the mic (captured as
// `Microphone`). Both streams run VAD + Whisper independently, so the same
// sentence gets transcribed twice — once tagged `mic`, once tagged `system`
// — and without this module both copies would be emitted and saved.
//
// The worker pool is serial (NUM_WORKERS = 1 — see worker.rs), so segments
// are processed one at a time in completion order. That gives us a natural
// serialization point: a small rolling buffer of recently-accepted segments
// that each new segment can be checked against before it's emitted.

use crate::audio::recording_state::DeviceType;
use log::info;
use std::collections::VecDeque;

/// Similarity threshold above which two normalized transcripts are treated
/// as "the same utterance" for echo-detection purposes. Picked to tolerate
/// the small transcription differences between the two copies (mic-side
/// room noise vs. clean system audio commonly cost a word or punctuation
/// here and there) while staying well clear of merely-similar sentences.
const SIMILARITY_THRESHOLD: f64 = 0.85;

/// How far apart (in seconds) two segments' audio intervals may be while
/// still counting as "overlapping" for echo purposes. The mic copy of a
/// system-audio utterance is not perfectly time-aligned with the original —
/// room acoustics and independent VAD segmentation on each stream can shift
/// the mic copy's boundaries by up to roughly this much relative to the
/// system copy.
const OVERLAP_TOLERANCE_SECS: f64 = 1.5;

/// Minimum word count a segment's normalized text must have to be eligible
/// for echo dedup at all. Short backchannel acknowledgements ("yeah",
/// "okay", "right") normalize identically across speakers and commonly land
/// within [`OVERLAP_TOLERANCE_SECS`] of each other purely because both
/// parties are reacting to the same moment in conversation — not because
/// one is an acoustic echo of the other. Below this word count (or below
/// [`MIN_MATCH_DURATION_SECS`]) we never treat the pair as an echo,
/// regardless of similarity.
const MIN_MATCH_WORD_COUNT: usize = 3;

/// Minimum audio duration (seconds) a segment must span to be eligible for
/// echo dedup. Companion floor to [`MIN_MATCH_WORD_COUNT`] — a very short
/// utterance is a backchannel even if it happens to tokenize to 3+ words.
const MIN_MATCH_DURATION_SECS: f64 = 1.0;

/// For texts that clear the length floor above but are still short (3-5
/// words), require a tighter similarity than [`SIMILARITY_THRESHOLD`]: at
/// this length a couple of shared common words is enough to hit 0.85 by
/// coincidence, so genuine echoes (near-identical) are distinguished from
/// merely-similar-length short phrases by demanding near-exact agreement.
const SHORT_TEXT_WORD_COUNT_MAX: usize = 5;
const SHORT_TEXT_SIMILARITY_THRESHOLD: f64 = 0.95;

/// Hard cap on the number of recently-accepted segments retained for
/// cross-source comparison. This is a safety bound; the time-based prune in
/// [`EchoDedup::prune`] is the primary bound and normally keeps the buffer
/// well under this during real conversation pacing.
const MAX_BUFFER_LEN: usize = 12;

/// Segments whose end time is more than this many seconds behind the
/// newest incoming segment's start time are pruned from the buffer — an
/// echo pair is only ever a few seconds apart (see [`OVERLAP_TOLERANCE_SECS`]),
/// so nothing older than this is ever going to match again.
const MAX_BUFFER_AGE_SECS: f64 = 30.0;

/// A previously-accepted (emitted) segment, kept around only long enough to
/// check newer segments against it.
#[derive(Debug, Clone)]
struct AcceptedSegment {
    source: DeviceType,
    text_normalized: String,
    audio_start_time: f64,
    audio_end_time: f64,
}

/// Outcome of running a candidate segment through [`EchoDedup::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EchoDecision {
    /// Emit normally.
    Accept,
    /// Suppress: this is a mic-bleed echo of a system-audio segment that's
    /// already buffered.
    DropAsMicEcho,
}

/// Rolling cross-source dedup state. One instance lives for the lifetime of
/// a single transcription worker loop, i.e. per recording session (see
/// worker.rs, alongside the per-session `context_tail`).
pub struct EchoDedup {
    buffer: VecDeque<AcceptedSegment>,
}

impl Default for EchoDedup {
    fn default() -> Self {
        Self::new()
    }
}

impl EchoDedup {
    pub fn new() -> Self {
        Self {
            buffer: VecDeque::with_capacity(MAX_BUFFER_LEN),
        }
    }

    /// Decide whether a candidate segment (`source`, `text`, spanning
    /// `audio_start_time..audio_end_time`) is an echo of an already-accepted
    /// segment from the *other* source.
    ///
    /// # Echo resolution policy (and its tradeoff)
    ///
    /// System audio is the cleaner capture of a remote participant's voice
    /// (no room acoustics or mic self-noise), so it's preferred over the mic
    /// copy when both exist. Concretely:
    ///
    /// - Candidate is `Microphone`, matches an already-buffered `System`
    ///   segment → drop the mic copy. This is the common case: system audio
    ///   generally completes around the same time as (or before) its mic
    ///   echo since both streams are fed through the same serial worker
    ///   queue, so the system copy is already in the buffer by the time the
    ///   mic copy would be checked.
    /// - Candidate is `System`, matches an already-buffered `Microphone`
    ///   segment → the mic copy was *already emitted*. We do not attempt to
    ///   retract it: the frontend has no "unsend" for a transcript line, and
    ///   reaching into recording storage after the fact to delete an
    ///   already-saved segment risks racing the save itself. So v1 still
    ///   emits the system copy and only logs the collision.
    ///
    ///   Net effect of this asymmetry: the common case (system arrives
    ///   first or concurrently) cleanly suppresses the mic echo before it's
    ///   ever emitted, while the less common mic-first race leaves a
    ///   same-content duplicate line behind. Fixing the latter would require
    ///   either delaying emission of every segment to let the "better" copy
    ///   win first (added latency on *all* segments, not just echoes) or a
    ///   retraction event across the Tauri boundary (more surface area for
    ///   a v1 whose goal is suppressing the common case cheaply). Neither
    ///   tradeoff was judged worth it yet.
    ///
    /// Same-source matches (e.g. someone genuinely repeating themselves,
    /// "yeah yeah") never count as an echo — only cross-source matches do.
    ///
    /// If only one source has ever produced an accepted segment (mic-only
    /// or system-only recording, or a session where the other stream never
    /// had a chunk above the silence gate), this is naturally a no-op:
    /// there is never an opposite-source entry in the buffer to match
    /// against, so nothing is ever suppressed.
    pub fn check(
        &mut self,
        source: DeviceType,
        text: &str,
        audio_start_time: f64,
        audio_end_time: f64,
    ) -> EchoDecision {
        self.prune(audio_start_time);

        let normalized = normalize_for_comparison(text);
        if normalized.is_empty() {
            return EchoDecision::Accept;
        }

        let opposite_match = self.buffer.iter().find(|seg| {
            is_cross_source_match(
                source,
                audio_start_time,
                audio_end_time,
                &normalized,
                seg.source,
                seg.audio_start_time,
                seg.audio_end_time,
                &seg.text_normalized,
            )
        });

        match (source, opposite_match) {
            (DeviceType::Microphone, Some(sys_seg)) => {
                info!(
                    "🔇 Echo dedup: dropping mic segment '{}' ({:.1}-{:.1}s) — matches system segment '{}' ({:.1}-{:.1}s)",
                    text,
                    audio_start_time,
                    audio_end_time,
                    sys_seg.text_normalized,
                    sys_seg.audio_start_time,
                    sys_seg.audio_end_time
                );
                EchoDecision::DropAsMicEcho
            }
            (DeviceType::System, Some(mic_seg)) => {
                info!(
                    "⚠️ Echo dedup: system segment '{}' ({:.1}-{:.1}s) matches already-emitted mic segment '{}' ({:.1}-{:.1}s) — emitting anyway (v1 does not retract already-emitted mic echoes; see EchoDedup::check docs)",
                    text,
                    audio_start_time,
                    audio_end_time,
                    mic_seg.text_normalized,
                    mic_seg.audio_start_time,
                    mic_seg.audio_end_time
                );
                EchoDecision::Accept
            }
            _ => EchoDecision::Accept,
        }
    }

    /// Record an accepted (emitted) segment so later cross-source checks
    /// can match against it. Only call this for segments that actually get
    /// emitted — echoes dropped by [`EchoDedup::check`] are intentionally
    /// not recorded, since nothing downstream should ever match against a
    /// suppressed duplicate.
    pub fn record(
        &mut self,
        source: DeviceType,
        text: &str,
        audio_start_time: f64,
        audio_end_time: f64,
    ) {
        let text_normalized = normalize_for_comparison(text);
        if text_normalized.is_empty() {
            return;
        }
        self.buffer.push_back(AcceptedSegment {
            source,
            text_normalized,
            audio_start_time,
            audio_end_time,
        });
        while self.buffer.len() > MAX_BUFFER_LEN {
            self.buffer.pop_front();
        }
    }

    /// Drop buffered segments too old to ever match a new one arriving at
    /// `newest_start_time`.
    fn prune(&mut self, newest_start_time: f64) {
        while let Some(front) = self.buffer.front() {
            if newest_start_time - front.audio_end_time > MAX_BUFFER_AGE_SECS {
                self.buffer.pop_front();
            } else {
                break;
            }
        }
    }
}

/// Normalize transcript text for comparison: lowercase, strip punctuation
/// (keeping alphanumerics and whitespace), and collapse runs of whitespace.
/// Pure and independently testable.
fn normalize_for_comparison(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Levenshtein edit distance between two strings, operating on chars so
/// multi-byte UTF-8 text is handled correctly. O(n*m) two-row DP — fine for
/// the short transcript segments this module compares.
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();

    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }

    let mut prev_row: Vec<usize> = (0..=b.len()).collect();
    let mut curr_row = vec![0usize; b.len() + 1];

    for (i, &ca) in a.iter().enumerate() {
        curr_row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr_row[j + 1] = (prev_row[j + 1] + 1) // deletion
                .min(curr_row[j] + 1) // insertion
                .min(prev_row[j] + cost); // substitution
        }
        std::mem::swap(&mut prev_row, &mut curr_row);
    }

    prev_row[b.len()]
}

/// Levenshtein-ratio similarity in `[0.0, 1.0]`: `1 - distance / max_len`.
/// Two empty strings are considered identical (ratio 1.0). Expects inputs
/// already normalized via [`normalize_for_comparison`] — this function does
/// no normalization of its own so it stays a pure, independently testable
/// string-distance primitive.
fn similarity_ratio(a: &str, b: &str) -> f64 {
    let max_len = a.chars().count().max(b.chars().count());
    if max_len == 0 {
        return 1.0;
    }
    let distance = levenshtein_distance(a, b);
    1.0 - (distance as f64 / max_len as f64)
}

/// Whether two audio intervals overlap once each is allowed `tolerance`
/// seconds of slack — i.e. a gap of up to `tolerance` between them still
/// counts as "overlapping". Pure and independently testable.
fn intervals_overlap(a_start: f64, a_end: f64, b_start: f64, b_end: f64, tolerance: f64) -> bool {
    a_start <= b_end + tolerance && b_start <= a_end + tolerance
}

/// Pure decision predicate: do segment A and segment B (each identified by
/// source, time interval, and pre-normalized text) constitute a cross-source
/// echo pair? Same-source pairs never match, regardless of how similar the
/// text or how close the timing — that's ordinary same-source repetition,
/// not echo.
#[allow(clippy::too_many_arguments)]
fn is_cross_source_match(
    source_a: DeviceType,
    start_a: f64,
    end_a: f64,
    text_a: &str,
    source_b: DeviceType,
    start_b: f64,
    end_b: f64,
    text_b: &str,
) -> bool {
    if source_a == source_b {
        return false;
    }
    if !intervals_overlap(start_a, end_a, start_b, end_b, OVERLAP_TOLERANCE_SECS) {
        return false;
    }

    // Length floor: short backchannel acknowledgements ("yeah", "okay",
    // "right") are never treated as echoes, no matter how similar or
    // well-timed — see MIN_MATCH_WORD_COUNT / MIN_MATCH_DURATION_SECS docs.
    let word_count_a = word_count(text_a);
    let word_count_b = word_count(text_b);
    let duration_a = end_a - start_a;
    let duration_b = end_b - start_b;
    if word_count_a < MIN_MATCH_WORD_COUNT
        || word_count_b < MIN_MATCH_WORD_COUNT
        || duration_a < MIN_MATCH_DURATION_SECS
        || duration_b < MIN_MATCH_DURATION_SECS
    {
        return false;
    }

    let threshold =
        if word_count_a <= SHORT_TEXT_WORD_COUNT_MAX || word_count_b <= SHORT_TEXT_WORD_COUNT_MAX {
            SHORT_TEXT_SIMILARITY_THRESHOLD
        } else {
            SIMILARITY_THRESHOLD
        };

    similarity_ratio(text_a, text_b) >= threshold
}

/// Word count of already-normalized text (whitespace-separated tokens).
fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- normalize_for_comparison -----------------------------------

    #[test]
    fn normalize_lowercases_and_strips_punctuation() {
        assert_eq!(
            normalize_for_comparison("Hello, World! How are you?"),
            "hello world how are you"
        );
    }

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize_for_comparison("  Hi   there  "), "hi there");
    }

    #[test]
    fn normalize_empty_stays_empty() {
        assert_eq!(normalize_for_comparison("   ...  !! "), "");
    }

    // --- similarity_ratio --------------------------------------------

    #[test]
    fn similarity_identical_is_one() {
        assert_eq!(similarity_ratio("hello world", "hello world"), 1.0);
    }

    #[test]
    fn similarity_both_empty_is_one() {
        assert_eq!(similarity_ratio("", ""), 1.0);
    }

    #[test]
    fn similarity_near_identical_clears_threshold() {
        // Single trailing-word difference on an otherwise-identical sentence
        // mirrors the kind of drift between a mic and system transcription
        // of the same remote utterance.
        let ratio = similarity_ratio("we should ship this friday", "we should ship this thursday");
        assert!(
            ratio >= SIMILARITY_THRESHOLD,
            "expected near-identical sentences to clear the threshold, got {}",
            ratio
        );
    }

    #[test]
    fn similarity_unrelated_text_is_low() {
        let ratio = similarity_ratio(
            "i think we should launch on friday",
            "can you pass the salt",
        );
        assert!(
            ratio < SIMILARITY_THRESHOLD,
            "expected unrelated sentences to stay below threshold, got {}",
            ratio
        );
    }

    // --- intervals_overlap ---------------------------------------------

    #[test]
    fn intervals_overlap_when_actually_overlapping() {
        assert!(intervals_overlap(10.0, 13.0, 12.0, 15.0, 1.5));
    }

    #[test]
    fn intervals_overlap_within_tolerance_gap() {
        // 13.0 -> 14.0 is a 1.0s gap, inside the 1.5s tolerance.
        assert!(intervals_overlap(10.0, 13.0, 14.0, 16.0, 1.5));
    }

    #[test]
    fn intervals_do_not_overlap_beyond_tolerance() {
        // 13.0 -> 20.0 is a 7.0s gap, well outside tolerance.
        assert!(!intervals_overlap(10.0, 13.0, 20.0, 23.0, 1.5));
    }

    // --- is_cross_source_match ------------------------------------------

    #[test]
    fn cross_source_overlapping_similar_text_matches() {
        assert!(is_cross_source_match(
            DeviceType::Microphone,
            10.0,
            13.0,
            "we should ship this friday",
            DeviceType::System,
            10.2,
            13.1,
            "we should ship this friday",
        ));
    }

    #[test]
    fn same_source_never_matches_even_if_identical_and_overlapping() {
        assert!(!is_cross_source_match(
            DeviceType::Microphone,
            10.0,
            13.0,
            "yeah yeah",
            DeviceType::Microphone,
            10.1,
            13.1,
            "yeah yeah",
        ));
    }

    #[test]
    fn cross_source_but_dissimilar_text_does_not_match() {
        assert!(!is_cross_source_match(
            DeviceType::Microphone,
            10.0,
            13.0,
            "we should ship this friday",
            DeviceType::System,
            10.2,
            13.1,
            "can you pass the salt",
        ));
    }

    #[test]
    fn cross_source_similar_text_but_no_time_overlap_does_not_match() {
        assert!(!is_cross_source_match(
            DeviceType::Microphone,
            10.0,
            13.0,
            "we should ship this friday",
            DeviceType::System,
            60.0,
            63.0,
            "we should ship this friday",
        ));
    }

    // --- EchoDedup end-to-end -------------------------------------------

    #[test]
    fn mic_echo_of_buffered_system_segment_is_dropped() {
        let mut dedup = EchoDedup::new();
        assert_eq!(
            dedup.check(DeviceType::System, "we should ship this friday", 10.0, 13.0),
            EchoDecision::Accept
        );
        dedup.record(DeviceType::System, "we should ship this friday", 10.0, 13.0);

        // Mic bleed of the same remote utterance, slightly shifted.
        assert_eq!(
            dedup.check(
                DeviceType::Microphone,
                "we should ship this friday",
                10.3,
                13.4
            ),
            EchoDecision::DropAsMicEcho
        );
    }

    #[test]
    fn system_echo_of_buffered_mic_segment_still_accepted() {
        let mut dedup = EchoDedup::new();
        assert_eq!(
            dedup.check(
                DeviceType::Microphone,
                "we should ship this friday",
                10.0,
                13.0
            ),
            EchoDecision::Accept
        );
        dedup.record(
            DeviceType::Microphone,
            "we should ship this friday",
            10.0,
            13.0,
        );

        // Mic copy already emitted; system copy arrives after. Per policy,
        // still accept (v1 doesn't retract), just logged as a collision.
        assert_eq!(
            dedup.check(DeviceType::System, "we should ship this friday", 10.3, 13.4),
            EchoDecision::Accept
        );
    }

    #[test]
    fn same_source_repetition_is_never_suppressed() {
        let mut dedup = EchoDedup::new();
        assert_eq!(
            dedup.check(DeviceType::Microphone, "yeah yeah", 10.0, 11.0),
            EchoDecision::Accept
        );
        dedup.record(DeviceType::Microphone, "yeah yeah", 10.0, 11.0);

        // Same speaker genuinely repeating themselves on the SAME source.
        assert_eq!(
            dedup.check(DeviceType::Microphone, "yeah yeah", 11.2, 12.2),
            EchoDecision::Accept
        );
    }

    #[test]
    fn single_source_recording_is_a_no_op() {
        // Mic-only recording (or system-only): the opposite source never
        // appears in the buffer, so nothing is ever suppressed even though
        // segments repeat and overlap in time.
        let mut dedup = EchoDedup::new();
        for (i, text) in ["let's begin", "let's begin", "okay moving on"]
            .iter()
            .enumerate()
        {
            let start = i as f64 * 3.0;
            let end = start + 2.5;
            assert_eq!(
                dedup.check(DeviceType::Microphone, text, start, end),
                EchoDecision::Accept
            );
            dedup.record(DeviceType::Microphone, text, start, end);
        }
    }

    #[test]
    fn old_buffered_segments_are_pruned_and_no_longer_match() {
        let mut dedup = EchoDedup::new();
        dedup.check(DeviceType::System, "we should ship this friday", 0.0, 3.0);
        dedup.record(DeviceType::System, "we should ship this friday", 0.0, 3.0);

        // Same text, same-ish source-crossing shape, but 40s later — well
        // past MAX_BUFFER_AGE_SECS (30s) and OVERLAP_TOLERANCE_SECS (1.5s).
        // This is coincidental repetition, not the original echo.
        assert_eq!(
            dedup.check(
                DeviceType::Microphone,
                "we should ship this friday",
                40.0,
                43.0
            ),
            EchoDecision::Accept
        );
    }

    #[test]
    fn short_backchannel_pair_is_kept_on_both_sides() {
        // Both parties say a one-word acknowledgement within the overlap
        // tolerance — this must never be treated as an echo, regardless of
        // how similar (here, identical) the normalized text is.
        let mut dedup = EchoDedup::new();
        assert_eq!(
            dedup.check(DeviceType::System, "yeah", 10.0, 10.3),
            EchoDecision::Accept
        );
        dedup.record(DeviceType::System, "yeah", 10.0, 10.3);

        assert_eq!(
            dedup.check(DeviceType::Microphone, "yeah", 10.1, 10.4),
            EchoDecision::Accept
        );
    }

    #[test]
    fn genuine_long_echo_is_still_dropped() {
        // A real 8-word echo comfortably clears both the length floor and
        // the (relaxed, since >5 words) SIMILARITY_THRESHOLD.
        let mut dedup = EchoDedup::new();
        let text = "we should definitely try to ship this friday";
        assert_eq!(
            dedup.check(DeviceType::System, text, 10.0, 13.0),
            EchoDecision::Accept
        );
        dedup.record(DeviceType::System, text, 10.0, 13.0);

        assert_eq!(
            dedup.check(DeviceType::Microphone, text, 10.3, 13.4),
            EchoDecision::DropAsMicEcho
        );
    }

    #[test]
    fn buffer_len_is_capped() {
        let mut dedup = EchoDedup::new();
        for i in 0..(MAX_BUFFER_LEN + 5) {
            let start = i as f64 * 0.1; // tight timing so age-pruning doesn't kick in
            let end = start + 0.05;
            dedup.record(DeviceType::System, &format!("segment {}", i), start, end);
        }
        assert!(dedup.buffer.len() <= MAX_BUFFER_LEN);
    }
}
