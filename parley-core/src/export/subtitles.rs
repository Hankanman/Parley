//! SRT and WebVTT subtitle export from transcript segments.
//!
//! Cue timing rules (segments come from the live pipeline, imports, and
//! retranscription, so timing quality varies):
//! - Segments are ordered by start time (stable, so ties keep their stored
//!   order). Empty segments are skipped.
//! - A segment with no start time inherits the previous cue's end (or 0 for
//!   the first). If *no* segment has a start time — an import from a source
//!   with no timing — there is nothing to sync subtitles to, so export fails
//!   with a clear message instead of inventing a timeline.
//! - A missing or non-positive end is estimated from the text length
//!   (~15 characters/second, 1–7 s).
//! - A cue that runs past the next cue's start is trimmed to it, so cues
//!   don't stack on screen. Cues that *start* at the same instant (mic and
//!   system talking over each other) are left overlapping: both are real
//!   and players show them together.
//! - Every cue lasts at least [`MIN_CUE_SECONDS`] so nothing is zero-length.

use super::ExportedSegment;

const MIN_CUE_SECONDS: f64 = 0.3;
const MIN_ESTIMATE_SECONDS: f64 = 1.0;
const MAX_ESTIMATE_SECONDS: f64 = 7.0;
const CHARS_PER_SECOND: f64 = 15.0;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Cue {
    pub start: f64,
    pub end: f64,
    pub speaker: Option<String>,
    pub text: String,
}

/// Turn transcript segments into ordered, non-degenerate cues.
pub(crate) fn build_cues(segments: &[ExportedSegment]) -> Result<Vec<Cue>, String> {
    if !segments
        .iter()
        .any(|s| s.start_seconds.is_some_and(|t| t.is_finite()))
    {
        return Err(
            "This transcript has no timestamps, so it can't be exported as subtitles".to_string(),
        );
    }

    // First pass: resolve start times (inheriting from the previous segment
    // in stored order) and keep each segment's own end if it has one.
    let mut raw: Vec<(f64, Option<f64>, Option<String>, String)> = Vec::new();
    let mut last_end = 0.0f64;
    for seg in segments {
        let text = normalize_text(&seg.text);
        if text.is_empty() {
            continue;
        }
        let start = seg
            .start_seconds
            .filter(|t| t.is_finite())
            .map(|t| t.max(0.0))
            .unwrap_or(last_end);
        let end = seg.end_seconds.filter(|e| e.is_finite() && *e > start);
        last_end = end.unwrap_or(start);
        let speaker = seg
            .speaker
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        raw.push((start, end, speaker, text));
    }

    raw.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut cues = Vec::with_capacity(raw.len());
    for i in 0..raw.len() {
        let (start, end, speaker, text) = &raw[i];
        let start = *start;
        let mut end = end.unwrap_or_else(|| start + estimate_duration(text));
        // Trim into the next cue that starts strictly later.
        if let Some(next_start) = raw[i + 1..].iter().map(|r| r.0).find(|&s| s > start) {
            if end > next_start {
                end = next_start;
            }
        }
        if end - start < MIN_CUE_SECONDS {
            end = start + MIN_CUE_SECONDS;
        }
        cues.push(Cue {
            start,
            end,
            speaker: speaker.clone(),
            text: text.clone(),
        });
    }
    Ok(cues)
}

fn estimate_duration(text: &str) -> f64 {
    (text.chars().count() as f64 / CHARS_PER_SECOND)
        .clamp(MIN_ESTIMATE_SECONDS, MAX_ESTIMATE_SECONDS)
}

/// Collapse a segment's text to non-empty lines. Blank lines end a cue in
/// both formats, so they must never appear inside cue text.
fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `HH:MM:SS<sep>mmm`, rounding to the millisecond.
pub(crate) fn format_cue_time(seconds: f64, ms_separator: char) -> String {
    let total_ms = (seconds.max(0.0) * 1000.0).round() as u64;
    let ms = total_ms % 1000;
    let total_s = total_ms / 1000;
    format!(
        "{:02}:{:02}:{:02}{}{:03}",
        total_s / 3600,
        (total_s % 3600) / 60,
        total_s % 60,
        ms_separator,
        ms
    )
}

/// SRT has no escaping mechanism; the only things that can break a cue are
/// a blank line (handled by [`normalize_text`]) and a stray `-->`, which
/// some parsers take for a timing line. Angle brackets are left alone: most
/// players treat `<i>`-style tags as formatting, but transcript text rarely
/// contains them and there's no portable way to escape them.
pub(crate) fn escape_srt(text: &str) -> String {
    text.replace("-->", "->")
}

/// WebVTT cue text escaping: `&`, `<` and `>` must be character references
/// (this also neutralises `-->`).
pub(crate) fn escape_vtt(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

pub(crate) fn render_srt(cues: &[Cue]) -> String {
    let mut out = String::new();
    for (i, cue) in cues.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n",
            i + 1,
            format_cue_time(cue.start, ','),
            format_cue_time(cue.end, ',')
        ));
        let text = escape_srt(&cue.text);
        match &cue.speaker {
            Some(speaker) => out.push_str(&format!("{}: {}\n\n", escape_srt(speaker), text)),
            None => out.push_str(&format!("{text}\n\n")),
        }
    }
    out
}

pub(crate) fn render_vtt(cues: &[Cue]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for cue in cues {
        out.push_str(&format!(
            "{} --> {}\n",
            format_cue_time(cue.start, '.'),
            format_cue_time(cue.end, '.')
        ));
        let text = escape_vtt(&cue.text);
        match &cue.speaker {
            // The voice-tag annotation runs to the closing `>`, so the name
            // gets the same escaping as cue text.
            Some(speaker) => out.push_str(&format!("<v {}>{}</v>\n\n", escape_vtt(speaker), text)),
            None => out.push_str(&format!("{text}\n\n")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(
        start: Option<f64>,
        end: Option<f64>,
        speaker: Option<&str>,
        text: &str,
    ) -> ExportedSegment {
        ExportedSegment {
            start_seconds: start,
            end_seconds: end,
            start_label: None,
            speaker: speaker.map(str::to_string),
            text: text.to_string(),
        }
    }

    #[test]
    fn cue_time_formats() {
        assert_eq!(format_cue_time(0.0, ','), "00:00:00,000");
        assert_eq!(format_cue_time(3661.2345, ','), "01:01:01,235");
        assert_eq!(format_cue_time(59.9996, '.'), "00:01:00.000");
        assert_eq!(format_cue_time(-4.0, '.'), "00:00:00.000");
        assert_eq!(format_cue_time(360_000.5, '.'), "100:00:00.500");
    }

    #[test]
    fn srt_numbering_timing_and_speaker_prefix() {
        let cues = build_cues(&[
            seg(Some(1.0), Some(2.5), Some("Me"), "Hello there"),
            seg(Some(3.0), Some(4.0), None, "General Kenobi"),
        ])
        .unwrap();
        assert_eq!(
            render_srt(&cues),
            "1\n00:00:01,000 --> 00:00:02,500\nMe: Hello there\n\n\
             2\n00:00:03,000 --> 00:00:04,000\nGeneral Kenobi\n\n"
        );
    }

    #[test]
    fn vtt_header_voice_tags_and_escaping() {
        let cues = build_cues(&[seg(
            Some(0.0),
            Some(1.0),
            Some("A<B>&C"),
            "x < y && a --> b",
        )])
        .unwrap();
        assert_eq!(
            render_vtt(&cues),
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\n\
             <v A&lt;B&gt;&amp;C>x &lt; y &amp;&amp; a --&gt; b</v>\n\n"
        );
    }

    #[test]
    fn srt_neutralises_arrows_and_blank_lines() {
        let cues =
            build_cues(&[seg(Some(0.0), Some(1.0), None, "a --> b\n\n\nnext   line")]).unwrap();
        assert_eq!(
            render_srt(&cues),
            "1\n00:00:00,000 --> 00:00:01,000\na -> b\nnext line\n\n"
        );
    }

    #[test]
    fn overlapping_cues_are_trimmed_to_the_next_start() {
        let cues = build_cues(&[
            seg(Some(0.0), Some(5.0), None, "first"),
            seg(Some(2.0), Some(3.0), None, "second"),
        ])
        .unwrap();
        assert_eq!(cues[0].end, 2.0);
        assert_eq!(cues[1].start, 2.0);
    }

    #[test]
    fn simultaneous_starts_stay_overlapping_and_get_sorted() {
        let cues = build_cues(&[
            seg(Some(10.0), Some(12.0), Some("B"), "later"),
            seg(Some(4.0), Some(6.0), Some("Me"), "mic"),
            seg(Some(4.0), Some(7.0), Some("Them"), "system"),
        ])
        .unwrap();
        let order: Vec<_> = cues.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(order, ["mic", "system", "later"]);
        assert_eq!(cues[0].end, 6.0);
        assert_eq!(cues[1].end, 7.0);
    }

    #[test]
    fn missing_times_are_inherited_and_estimated() {
        let cues = build_cues(&[
            seg(Some(1.0), Some(2.0), None, "timed"),
            seg(None, None, None, "untimed follows"),
            seg(Some(20.0), None, None, "no end"),
        ])
        .unwrap();
        assert_eq!(cues[1].start, 2.0);
        assert!(cues[1].end > cues[1].start);
        assert!(cues[2].end - cues[2].start >= MIN_ESTIMATE_SECONDS);
        assert!(cues[2].end - cues[2].start <= MAX_ESTIMATE_SECONDS);
    }

    #[test]
    fn degenerate_end_gets_a_minimum_duration_and_empty_text_is_skipped() {
        let cues = build_cues(&[
            seg(Some(5.0), Some(5.0), None, "zero length"),
            seg(Some(6.0), Some(7.0), None, "   "),
        ])
        .unwrap();
        assert_eq!(cues.len(), 1);
        assert!(cues[0].end > cues[0].start);
    }

    #[test]
    fn untimed_transcripts_are_rejected() {
        assert!(build_cues(&[seg(None, None, None, "imported")]).is_err());
    }
}
