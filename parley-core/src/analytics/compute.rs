//! Pure talk-time / speaking statistics over a meeting's transcript
//! segments (issue #86). No database, no LLM, no I/O.
//!
//! This file depends on nothing but `std` and `serde` on purpose: the
//! standalone `parley-mcp` server (which doesn't link `parley-core`)
//! compiles the very same file via `#[path]`, so the app and its MCP
//! `get_meeting_stats` tool can never disagree about what the numbers mean.
//! Keep it that way: no `crate::` imports here.
//!
//! Definitions (all times are seconds from the start of the recording):
//!
//! * **Talk time** — the summed length of a speaker's segments. Overlapping
//!   speech counts for both speakers, so shares are of the summed talk time
//!   (they always add up to 100%), not of the wall-clock meeting length.
//! * **Turn** — a run of consecutive segments (in time order) by one
//!   speaker, broken by another speaker or by a pause longer than
//!   [`TURN_GAP_SECS`].
//! * **Longest monologue** — the longest turn, first-segment start to
//!   last-segment end.
//! * **Words per minute** — words in timed segments over talk minutes, only
//!   once a speaker has talked for [`MIN_WPM_TALK_SECS`].
//! * **Question** — a segment whose text ends with `?`.
//! * **Interruption** — a speaker change where the new speaker's segment
//!   starts more than [`OVERLAP_TOLERANCE_SECS`] before the previous
//!   segment (by someone else) ended. Credited to the new speaker; the
//!   previous speaker gets an "interrupted" count.
//! * **Silence** — gaps of at least [`SILENCE_GAP_SECS`] with nobody
//!   speaking, including before the first and after the last segment.
//!
//! Segments without usable timestamps still count towards words, turns and
//! questions; they just add no talk time, can't form a timed monologue, and
//! are reported in [`MeetingStats::untimed_segments`].

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A pause longer than this ends a speaker's turn even if nobody else spoke.
pub const TURN_GAP_SECS: f64 = 5.0;
/// Gaps between speech at least this long count as silence.
pub const SILENCE_GAP_SECS: f64 = 2.0;
/// Overlaps shorter than this are VAD padding, not interruptions.
pub const OVERLAP_TOLERANCE_SECS: f64 = 0.5;
/// Below this much talk time a words-per-minute figure is noise.
pub const MIN_WPM_TALK_SECS: f64 = 10.0;

/// "You vs others" is only reported when this fraction of the talk time
/// knows which channel it came from; a split over a sliver of it misleads.
pub const ME_SHARE_MIN_COVERAGE: f64 = 0.8;

/// Display name for a segment with no speaker information at all.
pub const UNKNOWN_SPEAKER: &str = "Unknown speaker";

/// Which capture channel a segment came from. The mic is the local user,
/// the system stream is everyone else, so "me vs them" is reliable even
/// when diarization isn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Me,
    Others,
    #[default]
    Unknown,
}

impl Channel {
    /// Map a stored `transcripts.source` (`"mic"` / `"system"`) value.
    pub fn from_source(source: Option<&str>) -> Self {
        match source.map(str::trim) {
            Some("mic") => Channel::Me,
            Some("system") => Channel::Others,
            _ => Channel::Unknown,
        }
    }
}

/// One transcript segment, as the stats need it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Segment {
    /// Resolved display name (see [`speaker_display_name`]); `None` or blank
    /// means unknown.
    pub speaker: Option<String>,
    pub channel: Channel,
    pub text: String,
    pub start: Option<f64>,
    pub end: Option<f64>,
    /// Segment length, used when one of `start` / `end` is missing.
    pub duration: Option<f64>,
}

/// Resolve a segment's display name: an enrolled voice profile's name
/// first, then the stored speaker label, then the capture channel.
///
/// Rows written before diarization stored the raw channel (`"mic"` /
/// `"system"`) as the speaker label, so those map to "Me" / "Others" too.
pub fn speaker_display_name(profile_name: Option<&str>, speaker: Option<&str>, source: Option<&str>) -> String {
    let clean = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    if let Some(name) = clean(profile_name) {
        return name;
    }
    match clean(speaker).as_deref() {
        Some("mic") => return "Me".to_string(),
        Some("system") => return "Others".to_string(),
        Some(label) => return label.to_string(),
        None => {}
    }
    match Channel::from_source(source) {
        Channel::Me => "Me".to_string(),
        Channel::Others => "Others".to_string(),
        Channel::Unknown => UNKNOWN_SPEAKER.to_string(),
    }
}

/// One stored transcript row, as both the app's and the MCP server's
/// loaders read it (see [`segments_from_rows`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoredSegment {
    pub text: String,
    /// `transcripts.speaker`.
    pub speaker: Option<String>,
    /// `transcripts.source` (`"mic"` / `"system"`).
    pub source: Option<String>,
    /// The linked voice profile's name, if any.
    pub profile_name: Option<String>,
    /// The linked voice profile is the user's own enrolled voice.
    pub profile_is_self: bool,
    pub start: Option<f64>,
    pub end: Option<f64>,
    pub duration: Option<f64>,
}

/// Resolve stored rows into [`Segment`]s: display names, and the capture
/// channel. Live recordings store the channel (`source`). Imported or
/// retranscribed meetings don't; there a segment is "me" when it is linked
/// to the self voice profile, carries the legacy `"mic"` label, or shares a
/// display name with such a segment (speakers are grouped by name, so a
/// second profile with the user's name is the user too). Only when the
/// meeting has such a segment, i.e. the diarizer did know who "me" was, is
/// every other segment someone else.
pub fn segments_from_rows(rows: Vec<StoredSegment>) -> Vec<Segment> {
    let stored_channel = |r: &StoredSegment| match Channel::from_source(r.source.as_deref()) {
        Channel::Unknown => match r.speaker.as_deref().map(str::trim) {
            _ if r.profile_is_self => Channel::Me,
            Some("mic") => Channel::Me,
            Some("system") => Channel::Others,
            _ => Channel::Unknown,
        },
        known => known,
    };
    let resolved: Vec<(String, Channel, bool)> = rows
        .iter()
        .map(|r| {
            let name = speaker_display_name(r.profile_name.as_deref(), r.speaker.as_deref(), r.source.as_deref());
            let has_source = Channel::from_source(r.source.as_deref()) != Channel::Unknown;
            (name, stored_channel(r), has_source)
        })
        .collect();
    let self_names: std::collections::HashSet<&str> = resolved
        .iter()
        .filter(|(_, channel, has_source)| !has_source && *channel == Channel::Me)
        .map(|(name, _, _)| name.as_str())
        .collect();
    let channels: Vec<Channel> = resolved
        .iter()
        .map(|(name, channel, has_source)| match channel {
            Channel::Unknown if !has_source && self_names.contains(name.as_str()) => Channel::Me,
            Channel::Unknown if !self_names.is_empty() => Channel::Others,
            c => *c,
        })
        .collect();
    rows.into_iter()
        .zip(resolved)
        .zip(channels)
        .map(|((r, (name, _, _)), channel)| Segment {
            speaker: Some(name),
            channel,
            text: r.text,
            start: r.start,
            end: r.end,
            duration: r.duration,
        })
        .collect()
}

/// The longest uninterrupted stretch by one speaker.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Monologue {
    pub start_secs: f64,
    pub duration_secs: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeakerStats {
    pub name: String,
    /// Most of this speaker's segments came from the microphone.
    pub is_me: bool,
    pub segments: usize,
    pub talk_secs: f64,
    /// Fraction (0–1) of the meeting's summed talk time.
    pub share: f64,
    pub turns: usize,
    pub words: usize,
    pub words_per_minute: Option<f64>,
    pub longest_monologue: Option<Monologue>,
    pub questions: usize,
    /// Times this speaker started talking over someone else.
    pub interruptions: usize,
    /// Times someone else started talking over this speaker.
    pub interrupted: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeetingStats {
    /// Most talk time first.
    pub speakers: Vec<SpeakerStats>,
    /// Recording length: the stored meeting duration or the last segment's
    /// end, whichever is later. `None` when nothing is timed.
    pub duration_secs: Option<f64>,
    pub total_talk_secs: f64,
    pub silence_secs: f64,
    /// Silence over duration (0–1). `None` without a duration.
    pub silence_ratio: Option<f64>,
    /// Share (0–1) of talk time from the microphone channel. `None` unless
    /// at least [`ME_SHARE_MIN_COVERAGE`] of the talk time knows its channel.
    pub me_share: Option<f64>,
    pub segment_count: usize,
    /// Segments without usable timestamps (no talk time counted).
    pub untimed_segments: usize,
    pub total_words: usize,
    pub total_questions: usize,
    pub total_interruptions: usize,
    /// The meeting's longest monologue and who gave it.
    pub longest_monologue: Option<(String, Monologue)>,
}

impl MeetingStats {
    pub fn is_empty(&self) -> bool {
        self.segment_count == 0
    }
}

/// A segment's time span, if its timestamps make sense.
fn interval(seg: &Segment) -> Option<(f64, f64)> {
    let ok = |v: Option<f64>| v.filter(|x| x.is_finite() && *x >= 0.0);
    let (start, end, duration) = (ok(seg.start), ok(seg.end), ok(seg.duration));
    let span = match (start, end, duration) {
        (Some(s), Some(e), _) => (s, e),
        (Some(s), None, Some(d)) => (s, s + d),
        (None, Some(e), Some(d)) => ((e - d).max(0.0), e),
        _ => return None,
    };
    (span.1 >= span.0).then_some(span)
}

fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

fn is_question(text: &str) -> bool {
    let trimmed = text.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ']' | '\u{201d}' | '\u{2019}' | '\u{bb}')
    });
    trimmed.ends_with('?') || trimmed.ends_with('\u{ff1f}')
}

#[derive(Default)]
struct Acc {
    name: String,
    segments: usize,
    me_segments: usize,
    talk_secs: f64,
    turns: usize,
    words: usize,
    timed_words: usize,
    longest: Option<Monologue>,
    questions: usize,
    interruptions: usize,
    interrupted: usize,
}

/// An open turn: who, and its timed extent so far (if any).
struct Turn {
    speaker: usize,
    span: Option<(f64, f64)>,
}

fn close_turn(turn: Option<Turn>, accs: &mut [Acc]) {
    let Some(Turn { speaker, span: Some((s, e)) }) = turn else {
        return;
    };
    let candidate = Monologue { start_secs: s, duration_secs: e - s };
    let acc = &mut accs[speaker];
    if acc.longest.is_none_or(|m| candidate.duration_secs > m.duration_secs) {
        acc.longest = Some(candidate);
    }
}

/// Compute a meeting's stats. `recorded_duration` is the stored meeting
/// length, if known (it covers trailing silence after the last segment).
pub fn compute(segments: &[Segment], recorded_duration: Option<f64>) -> MeetingStats {
    struct Item<'a> {
        speaker: usize,
        seg: &'a Segment,
        span: Option<(f64, f64)>,
    }

    let mut accs: Vec<Acc> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut items: Vec<Item> = segments
        .iter()
        .map(|seg| {
            let name = seg
                .speaker
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(UNKNOWN_SPEAKER)
                .to_string();
            let speaker = *index.entry(name.clone()).or_insert_with(|| {
                accs.push(Acc { name, ..Acc::default() });
                accs.len() - 1
            });
            Item { speaker, seg, span: interval(seg) }
        })
        .collect();

    // Time order when every segment is timed; otherwise trust the caller's
    // order (the loader sorts by sequence id, then start time).
    if items.iter().all(|i| i.span.is_some()) {
        items.sort_by(|a, b| a.span.unwrap().0.total_cmp(&b.span.unwrap().0));
    }

    let mut untimed = 0;
    let (mut me_talk, mut channel_known_talk) = (0.0, 0.0);
    let mut turn: Option<Turn> = None;
    let mut prev: Option<(usize, Option<(f64, f64)>)> = None;

    for item in &items {
        let acc = &mut accs[item.speaker];
        let words = word_count(&item.seg.text);
        acc.segments += 1;
        acc.words += words;
        if item.seg.channel == Channel::Me {
            acc.me_segments += 1;
        }
        if is_question(&item.seg.text) {
            acc.questions += 1;
        }
        match item.span {
            Some((s, e)) => {
                acc.talk_secs += e - s;
                acc.timed_words += words;
                if item.seg.channel != Channel::Unknown {
                    channel_known_talk += e - s;
                    if item.seg.channel == Channel::Me {
                        me_talk += e - s;
                    }
                }
            }
            None => untimed += 1,
        }

        // Interruption: a speaker change that starts over the previous
        // segment.
        if let Some((prev_speaker, Some((_, prev_end)))) = prev {
            if let Some((s, _)) = item.span {
                if prev_speaker != item.speaker && s < prev_end - OVERLAP_TOLERANCE_SECS {
                    accs[item.speaker].interruptions += 1;
                    accs[prev_speaker].interrupted += 1;
                }
            }
        }
        prev = Some((item.speaker, item.span));

        // Turns: same speaker continues unless the pause was too long.
        let continues = turn.as_ref().is_some_and(|t| {
            t.speaker == item.speaker
                && match (t.span, item.span) {
                    (Some((_, end)), Some((s, _))) => s - end <= TURN_GAP_SECS,
                    _ => true,
                }
        });
        if continues {
            let t = turn.as_mut().unwrap();
            t.span = match (t.span, item.span) {
                (Some((s0, e0)), Some((_, e))) => Some((s0, e0.max(e))),
                (None, span) => span,
                (span, None) => span,
            };
        } else {
            close_turn(turn.take(), &mut accs);
            accs[item.speaker].turns += 1;
            turn = Some(Turn { speaker: item.speaker, span: item.span });
        }
    }
    close_turn(turn, &mut accs);

    // Silence: gaps in the union of speech intervals.
    let mut spans: Vec<(f64, f64)> = items.iter().filter_map(|i| i.span).collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let last_end = spans.iter().map(|s| s.1).fold(None, |m: Option<f64>, e| Some(m.map_or(e, |m| m.max(e))));
    let recorded = recorded_duration.filter(|d| d.is_finite() && *d > 0.0);
    let duration = match (last_end, recorded) {
        (Some(e), Some(r)) => Some(e.max(r)),
        (e, r) => e.or(r),
    };
    let mut silence = 0.0;
    if let Some(duration) = duration {
        let mut cursor = 0.0_f64;
        for (s, e) in &spans {
            if s - cursor >= SILENCE_GAP_SECS {
                silence += s - cursor;
            }
            cursor = cursor.max(*e);
        }
        if duration - cursor >= SILENCE_GAP_SECS {
            silence += duration - cursor;
        }
    }

    let total_talk: f64 = accs.iter().map(|a| a.talk_secs).sum();
    let mut speakers: Vec<SpeakerStats> = accs
        .into_iter()
        .map(|a| SpeakerStats {
            is_me: a.me_segments * 2 > a.segments,
            segments: a.segments,
            share: if total_talk > 0.0 { a.talk_secs / total_talk } else { 0.0 },
            turns: a.turns,
            words: a.words,
            words_per_minute: (a.talk_secs >= MIN_WPM_TALK_SECS)
                .then(|| a.timed_words as f64 / (a.talk_secs / 60.0)),
            longest_monologue: a.longest,
            questions: a.questions,
            interruptions: a.interruptions,
            interrupted: a.interrupted,
            talk_secs: a.talk_secs,
            name: a.name,
        })
        .collect();
    speakers.sort_by(|a, b| {
        b.talk_secs
            .total_cmp(&a.talk_secs)
            .then(b.segments.cmp(&a.segments))
            .then_with(|| a.name.cmp(&b.name))
    });

    let longest_monologue = speakers
        .iter()
        .filter_map(|s| s.longest_monologue.map(|m| (s.name.clone(), m)))
        .fold(None, |best: Option<(String, Monologue)>, cand| match best {
            Some(b) if b.1.duration_secs >= cand.1.duration_secs => Some(b),
            _ => Some(cand),
        });

    MeetingStats {
        duration_secs: duration,
        total_talk_secs: total_talk,
        silence_secs: silence,
        silence_ratio: duration.filter(|d| *d > 0.0).map(|d| (silence / d).clamp(0.0, 1.0)),
        me_share: (channel_known_talk > 0.0 && channel_known_talk >= ME_SHARE_MIN_COVERAGE * total_talk)
            .then(|| me_talk / channel_known_talk),
        segment_count: segments.len(),
        untimed_segments: untimed,
        total_words: speakers.iter().map(|s| s.words).sum(),
        total_questions: speakers.iter().map(|s| s.questions).sum(),
        total_interruptions: speakers.iter().map(|s| s.interruptions).sum(),
        longest_monologue,
        speakers,
    }
}

/// `m:ss`, or `h:mm:ss` past an hour.
pub fn format_clock(secs: f64) -> String {
    let total = if secs.is_finite() && secs > 0.0 { secs.round() as u64 } else { 0 };
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// A length for humans: `45s`, `3m 05s`, `1h 02m`.
pub fn format_span(secs: f64) -> String {
    let total = if secs.is_finite() && secs > 0.0 { secs.round() as u64 } else { 0 };
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(speaker: &str, start: f64, end: f64, text: &str) -> Segment {
        Segment {
            speaker: Some(speaker.to_string()),
            channel: Channel::Unknown,
            text: text.to_string(),
            start: Some(start),
            end: Some(end),
            duration: None,
        }
    }

    fn speaker<'a>(stats: &'a MeetingStats, name: &str) -> &'a SpeakerStats {
        stats.speakers.iter().find(|s| s.name == name).expect(name)
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn empty_input_yields_empty_stats() {
        let stats = compute(&[], None);
        assert!(stats.is_empty());
        assert!(stats.speakers.is_empty());
        assert_eq!(stats.duration_secs, None);
        assert_eq!(stats.silence_ratio, None);
        assert_eq!(stats.me_share, None);
        assert_eq!(stats.longest_monologue, None);
    }

    #[test]
    fn talk_time_shares_and_ordering() {
        let segs = vec![
            seg("Alice", 0.0, 10.0, "hello there everyone"),
            seg("Bob", 10.0, 40.0, "long answer"),
            seg("Alice", 40.0, 50.0, "ok"),
        ];
        let stats = compute(&segs, None);
        assert_eq!(stats.speakers[0].name, "Bob");
        assert!(approx(speaker(&stats, "Bob").talk_secs, 30.0));
        assert!(approx(speaker(&stats, "Alice").talk_secs, 20.0));
        assert!(approx(speaker(&stats, "Bob").share, 0.6));
        assert!(approx(speaker(&stats, "Alice").share, 0.4));
        assert!(approx(stats.total_talk_secs, 50.0));
        assert_eq!(speaker(&stats, "Alice").turns, 2);
        assert_eq!(speaker(&stats, "Bob").turns, 1);
        assert_eq!(stats.duration_secs, Some(50.0));
    }

    #[test]
    fn words_per_minute_needs_enough_talk() {
        let segs = vec![
            // 30 words in 15 s => 120 wpm.
            seg("Alice", 0.0, 15.0, &"word ".repeat(30)),
            // Only 5 s of talk: no wpm.
            seg("Bob", 15.0, 20.0, "a b c d e"),
        ];
        let stats = compute(&segs, None);
        assert!(approx(speaker(&stats, "Alice").words_per_minute.unwrap(), 120.0));
        assert_eq!(speaker(&stats, "Bob").words_per_minute, None);
        assert_eq!(stats.total_words, 35);
    }

    #[test]
    fn consecutive_segments_merge_into_one_monologue() {
        let segs = vec![
            seg("Alice", 0.0, 5.0, "a"),
            seg("Bob", 5.0, 10.0, "b"),
            seg("Bob", 11.0, 20.0, "b"),
            seg("Bob", 22.0, 40.0, "b"),
            seg("Alice", 40.0, 42.0, "a"),
        ];
        let stats = compute(&segs, None);
        let bob = speaker(&stats, "Bob");
        assert_eq!(bob.turns, 1);
        assert_eq!(bob.longest_monologue, Some(Monologue { start_secs: 5.0, duration_secs: 35.0 }));
        assert_eq!(
            stats.longest_monologue,
            Some(("Bob".to_string(), Monologue { start_secs: 5.0, duration_secs: 35.0 }))
        );
    }

    #[test]
    fn long_pause_splits_a_turn() {
        let segs = vec![seg("Alice", 0.0, 10.0, "a"), seg("Alice", 30.0, 35.0, "a")];
        let stats = compute(&segs, None);
        let alice = speaker(&stats, "Alice");
        assert_eq!(alice.turns, 2);
        assert_eq!(alice.longest_monologue.unwrap().duration_secs, 10.0);
    }

    #[test]
    fn questions_are_segments_ending_in_a_question_mark() {
        let segs = vec![
            seg("Alice", 0.0, 1.0, "Can you hear me?"),
            seg("Alice", 1.0, 2.0, "What about \"this?\" "),
            seg("Bob", 2.0, 3.0, "Is it? No, it isn't."),
            seg("Bob", 3.0, 4.0, "Really？"),
        ];
        let stats = compute(&segs, None);
        assert_eq!(speaker(&stats, "Alice").questions, 2);
        assert_eq!(speaker(&stats, "Bob").questions, 1);
        assert_eq!(stats.total_questions, 3);
    }

    #[test]
    fn overlaps_count_as_interruptions_beyond_tolerance() {
        let segs = vec![
            seg("Alice", 0.0, 20.0, "a"),
            // Starts 15 s before Alice finished: interruption.
            seg("Bob", 5.0, 8.0, "b"),
            // Back to Alice, starting before Bob ended? No: 9 > 8.
            seg("Alice", 9.0, 12.0, "a"),
            // Within tolerance of Alice's end: not an interruption.
            seg("Bob", 11.8, 15.0, "b"),
        ];
        let stats = compute(&segs, None);
        assert_eq!(speaker(&stats, "Bob").interruptions, 1);
        assert_eq!(speaker(&stats, "Alice").interrupted, 1);
        assert_eq!(speaker(&stats, "Alice").interruptions, 0);
        assert_eq!(stats.total_interruptions, 1);
    }

    #[test]
    fn same_speaker_overlap_is_not_an_interruption() {
        let segs = vec![seg("Alice", 0.0, 10.0, "a"), seg("Alice", 5.0, 12.0, "a")];
        let stats = compute(&segs, None);
        assert_eq!(stats.total_interruptions, 0);
    }

    #[test]
    fn input_order_is_sorted_by_start_when_fully_timed() {
        let segs = vec![seg("Bob", 10.0, 20.0, "b"), seg("Alice", 0.0, 10.0, "a")];
        let stats = compute(&segs, None);
        assert_eq!(stats.total_interruptions, 0);
        assert_eq!(speaker(&stats, "Alice").turns, 1);
    }

    #[test]
    fn silence_counts_gaps_over_threshold_including_edges() {
        let segs = vec![
            seg("Alice", 5.0, 10.0, "a"),  // 5 s leading silence
            seg("Bob", 11.0, 20.0, "b"),   // 1 s gap: below threshold
            seg("Alice", 30.0, 40.0, "a"), // 10 s gap
        ];
        // Recording ran to 50 s: 10 s trailing silence.
        let stats = compute(&segs, Some(50.0));
        assert_eq!(stats.duration_secs, Some(50.0));
        assert!(approx(stats.silence_secs, 25.0));
        assert!(approx(stats.silence_ratio.unwrap(), 0.5));
    }

    #[test]
    fn overlapping_speech_is_not_silence() {
        let segs = vec![seg("Alice", 0.0, 30.0, "a"), seg("Bob", 10.0, 15.0, "b"), seg("Bob", 29.0, 40.0, "b")];
        let stats = compute(&segs, None);
        assert!(approx(stats.silence_secs, 0.0));
        // Overlap counted for both speakers.
        assert!(approx(stats.total_talk_secs, 46.0));
    }

    #[test]
    fn missing_timestamps_are_handled() {
        let segs = vec![
            Segment { speaker: Some("Alice".into()), text: "no times here?".into(), ..Segment::default() },
            // Start + duration.
            Segment { speaker: Some("Bob".into()), text: "x".into(), start: Some(2.0), duration: Some(3.0), ..Segment::default() },
            // End + duration.
            Segment { speaker: Some("Bob".into()), text: "y".into(), end: Some(9.0), duration: Some(4.0), ..Segment::default() },
            // Nonsense: end before start.
            Segment { speaker: Some("Alice".into()), text: "z".into(), start: Some(10.0), end: Some(4.0), ..Segment::default() },
            // NaN.
            Segment { speaker: Some("Alice".into()), text: "w".into(), start: Some(f64::NAN), end: Some(1.0), ..Segment::default() },
        ];
        let stats = compute(&segs, None);
        assert_eq!(stats.segment_count, 5);
        assert_eq!(stats.untimed_segments, 3);
        assert!(approx(speaker(&stats, "Bob").talk_secs, 7.0));
        assert!(approx(speaker(&stats, "Alice").talk_secs, 0.0));
        assert_eq!(speaker(&stats, "Alice").questions, 1);
        assert_eq!(speaker(&stats, "Alice").longest_monologue, None);
        assert!(approx(speaker(&stats, "Bob").share, 1.0));
        assert_eq!(stats.duration_secs, Some(9.0));
    }

    #[test]
    fn fully_untimed_meeting_still_counts_words_and_turns() {
        let segs = vec![
            Segment { speaker: Some("Alice".into()), text: "one two".into(), ..Segment::default() },
            Segment { speaker: Some("Bob".into()), text: "three?".into(), ..Segment::default() },
            Segment { speaker: Some("Alice".into()), text: "four".into(), ..Segment::default() },
        ];
        let stats = compute(&segs, None);
        assert_eq!(stats.duration_secs, None);
        assert_eq!(stats.silence_ratio, None);
        assert_eq!(speaker(&stats, "Alice").turns, 2);
        assert_eq!(speaker(&stats, "Alice").words, 3);
        assert_eq!(speaker(&stats, "Alice").share, 0.0);
        assert_eq!(stats.total_questions, 1);
    }

    #[test]
    fn unknown_speakers_are_grouped() {
        let segs = vec![
            Segment { speaker: None, start: Some(0.0), end: Some(2.0), ..Segment::default() },
            Segment { speaker: Some("   ".into()), start: Some(2.0), end: Some(4.0), ..Segment::default() },
        ];
        let stats = compute(&segs, None);
        assert_eq!(stats.speakers.len(), 1);
        assert_eq!(stats.speakers[0].name, UNKNOWN_SPEAKER);
        assert_eq!(stats.speakers[0].turns, 1);
    }

    #[test]
    fn me_share_follows_the_mic_channel() {
        let mut me = seg("Me", 0.0, 10.0, "a");
        me.channel = Channel::Me;
        let mut alice = seg("Alice", 10.0, 30.0, "b");
        alice.channel = Channel::Others;
        let mut bob = seg("Bob", 30.0, 40.0, "c");
        bob.channel = Channel::Others;
        let stats = compute(&[me, alice, bob], None);
        assert!(approx(stats.me_share.unwrap(), 0.25));
        assert!(speaker(&stats, "Me").is_me);
        assert!(!speaker(&stats, "Alice").is_me);
    }

    #[test]
    fn me_share_needs_most_talk_to_know_its_channel() {
        let mut me = seg("Me", 0.0, 10.0, "a");
        me.channel = Channel::Me;
        let unknown = seg("Speaker 1", 10.0, 60.0, "b");
        let stats = compute(&[me, unknown], None);
        assert_eq!(stats.me_share, None);
    }

    #[test]
    fn rows_without_source_infer_channels_from_the_self_profile() {
        let row = |speaker: &str, profile: Option<&str>, is_self: bool, source: Option<&str>| StoredSegment {
            speaker: Some(speaker.to_string()),
            profile_name: profile.map(str::to_string),
            profile_is_self: is_self,
            source: source.map(str::to_string),
            ..StoredSegment::default()
        };
        // Imported meeting: the self profile is recognised, so the rest is
        // everyone else.
        // A second, non-self profile with the same name is still "me".
        let segs = segments_from_rows(vec![
            row("Speaker 1", Some("Seb"), true, None),
            row("Speaker 2", None, false, None),
            row("Seb", Some("Seb"), false, None),
        ]);
        assert_eq!(segs[0].speaker.as_deref(), Some("Seb"));
        assert_eq!(
            segs.iter().map(|s| s.channel).collect::<Vec<_>>(),
            vec![Channel::Me, Channel::Others, Channel::Me]
        );

        // No self anywhere: unknown stays unknown.
        let segs = segments_from_rows(vec![row("Speaker 1", None, false, None), row("Speaker 2", None, false, None)]);
        assert!(segs.iter().all(|s| s.channel == Channel::Unknown));

        // Live recording: the stored source wins.
        let segs = segments_from_rows(vec![
            row("Me", None, false, Some("mic")),
            row("Alice", Some("Alice"), false, Some("system")),
            row("system", None, false, None),
        ]);
        assert_eq!(
            segs.iter().map(|s| s.channel).collect::<Vec<_>>(),
            vec![Channel::Me, Channel::Others, Channel::Others]
        );
        assert_eq!(segs[2].speaker.as_deref(), Some("Others"));
    }

    #[test]
    fn display_name_resolution() {
        assert_eq!(speaker_display_name(Some("Alice"), Some("Speaker 1"), Some("system")), "Alice");
        assert_eq!(speaker_display_name(Some("  "), Some("Speaker 1"), Some("system")), "Speaker 1");
        assert_eq!(speaker_display_name(None, Some("mic"), None), "Me");
        assert_eq!(speaker_display_name(None, Some("system"), None), "Others");
        assert_eq!(speaker_display_name(None, None, Some("mic")), "Me");
        assert_eq!(speaker_display_name(None, Some(""), Some("system")), "Others");
        assert_eq!(speaker_display_name(None, None, None), UNKNOWN_SPEAKER);
        assert_eq!(Channel::from_source(Some("mic")), Channel::Me);
        assert_eq!(Channel::from_source(Some("bogus")), Channel::Unknown);
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(format_clock(0.0), "0:00");
        assert_eq!(format_clock(65.4), "1:05");
        assert_eq!(format_clock(3725.0), "1:02:05");
        assert_eq!(format_clock(f64::NAN), "0:00");
        assert_eq!(format_span(45.0), "45s");
        assert_eq!(format_span(185.0), "3m 05s");
        assert_eq!(format_span(3720.0), "1h 02m");
    }
}
