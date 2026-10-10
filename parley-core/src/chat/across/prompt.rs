//! Prompt construction and citation handling for "Ask Parley across
//! meetings" — pure functions, no I/O.
//!
//! ## Context shape
//!
//! Retrieved excerpts are grouped by meeting. Each meeting gets a short
//! reference label (`M1`, `M2`, … numbered in the order the meetings appear,
//! which is chronological) plus its title and date, and its excerpts are the
//! usual `[mm:ss] Speaker: text` lines with `…` between non-adjacent ones:
//!
//! ```text
//! <meeting ref="M1" title="Platform sync" date="Thu 14 Aug 2026">
//! [03:10] Alice: Where are we on the migration?
//! [03:25] Kingfisher: Cut-over is the 30th.
//! …
//! </meeting>
//! ```
//!
//! ## Citations
//!
//! The model is told to cite `[M1 03:25]` — the meeting label and the
//! timestamp inside one pair of brackets. That's unambiguous across meetings
//! and short enough for small local models to reproduce. Parsing is lenient
//! about what models actually write: `[M1, 03:25]`, `[M1 @ 03:25]`,
//! `[M1 03:25-04:00]` (cited by its start), a bare `[M1]` (the meeting, no
//! time), and a bare `[03:25]` right after the answer mentioned a meeting
//! (resolved to the most recently mentioned meeting, or to the only source
//! when there is just one). Labels that don't name a source stay plain text.
//!
//! Answers are untrusted (model output steered by transcript text anyone in a
//! meeting can influence), so rendering goes through the same
//! escape-then-link approach as single-meeting chat: every answer character
//! that could form markdown link/image/HTML syntax is escaped, and the only
//! links in the result are the citations [`link_citations`] creates. Their
//! URLs carry a *source index* and seconds (`parley-cite:0@754`), never
//! anything taken from the answer, and the UI resolves the index against the
//! sources it got with that answer.

use crate::chat::context::{escape_untrusted_markdown, segment_line, ChatTurn, ContextSegment};
use crate::summary::processor::rough_token_count;
use chrono::{DateTime, Local, Utc};
use once_cell::sync::Lazy;
use regex::Regex;

/// Prior messages carried into the prompt for follow-ups.
pub const MAX_HISTORY_MESSAGES: usize = 6;
const MAX_HISTORY_MESSAGE_CHARS: usize = 800;

pub const SYSTEM_PROMPT: &str = "You are Parley, an assistant that answers questions about the user's recorded meetings. \
You are given excerpts from several meetings. Each meeting has a reference like M1 with its title and date, \
and each excerpt line is `[mm:ss] Speaker: text`.\n\
Rules:\n\
- Answer concisely and directly. Use short paragraphs or bullet points.\n\
- Use only the excerpts. Cite every claim with the meeting reference and timestamp together in square brackets, e.g. [M1 12:34]. \
Never cite a timestamp without its meeting reference.\n\
- Mention which meeting (title or date) a point comes from when it helps, especially when meetings disagree or things changed over time.\n\
- If the excerpts do not answer the question, say so plainly instead of guessing. They are only parts of the meetings.\n\
- Refer to people by the speaker names used in the excerpts.";

pub const ANSWER_REMINDER: &str = "Answer concisely using only the excerpts. \
Cite each point as [M1 12:34] (meeting reference and timestamp). If the excerpts don't answer it, say so.";

/// One meeting's retrieved excerpts, ready for the prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct MeetingExcerpts {
    pub meeting_id: String,
    pub title: String,
    pub created_at: Option<DateTime<Utc>>,
    /// Runs of consecutive segments, in chronological order.
    pub passages: Vec<Vec<ContextSegment>>,
}

/// A meeting the answer may cite, by its position in the prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceRef {
    /// `M1`, `M2`, … — the label the prompt used.
    pub label: String,
    pub meeting_id: String,
    pub title: String,
    pub created_at: Option<DateTime<Utc>>,
    /// Start of the first excerpt shown to the model, if timed — where a
    /// click on the source (rather than a timestamp) lands.
    pub first_seconds: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct BuiltPrompt {
    pub system: String,
    pub user: String,
    pub sources: Vec<SourceRef>,
}

/// `M{n}` for the source at `index`.
pub fn source_label(index: usize) -> String {
    format!("M{}", index + 1)
}

/// Date as shown to the model and in the UI's source list ("Thu 14 Aug 2026").
pub fn format_meeting_date(created_at: &DateTime<Utc>) -> String {
    created_at.with_timezone(&Local).format("%a %-d %b %Y").to_string()
}

/// A title safe to put inside the prompt's `title="…"` attribute.
fn prompt_title(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| match c {
            '"' => '\'',
            '<' | '>' | '\n' | '\r' | '\t' => ' ',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "Untitled meeting".to_string()
    } else {
        cleaned.chars().take(120).collect()
    }
}

/// Build the prompts for one question over `meetings` (already selected and
/// fitted to the budget by retrieval; in the order they should be labelled).
pub fn build_prompt(
    meetings: &[MeetingExcerpts],
    history: &[ChatTurn],
    question: &str,
    filter_note: Option<&str>,
) -> BuiltPrompt {
    let question = question.trim();
    let mut user = String::new();
    let mut sources = Vec::with_capacity(meetings.len());

    if let Some(note) = filter_note.filter(|n| !n.trim().is_empty()) {
        user.push_str(&format!("(Search limited to: {}.)\n\n", note.trim()));
    }

    for (ix, meeting) in meetings.iter().enumerate() {
        let label = source_label(ix);
        let date = meeting
            .created_at
            .as_ref()
            .map(format_meeting_date)
            .unwrap_or_else(|| "unknown date".to_string());
        user.push_str(&format!(
            "<meeting ref=\"{label}\" title=\"{}\" date=\"{date}\">\n",
            prompt_title(&meeting.title)
        ));
        for (pix, passage) in meeting.passages.iter().enumerate() {
            if pix > 0 {
                user.push_str("…\n");
            }
            for segment in passage.iter().filter(|s| !s.text.trim().is_empty()) {
                user.push_str(&segment_line(segment));
                user.push('\n');
            }
        }
        user.push_str("</meeting>\n\n");
        sources.push(SourceRef {
            label,
            meeting_id: meeting.meeting_id.clone(),
            title: meeting.title.clone(),
            created_at: meeting.created_at,
            first_seconds: meeting
                .passages
                .iter()
                .flat_map(|p| p.iter())
                .find_map(|s| s.start),
        });
    }

    let history_lines: Vec<String> = history
        .iter()
        .rev()
        .take(MAX_HISTORY_MESSAGES)
        .map(|turn| {
            // Earlier answers' citations used *their* prompt's labels, which
            // would mislead here — drop them and keep the prose.
            let content = strip_citations(turn.content.trim());
            let content: String = if content.chars().count() > MAX_HISTORY_MESSAGE_CHARS {
                let mut c: String = content.chars().take(MAX_HISTORY_MESSAGE_CHARS).collect();
                c.push_str(" …");
                c
            } else {
                content
            };
            format!("{}: {}", if turn.is_user { "User" } else { "Assistant" }, content)
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if !history_lines.is_empty() {
        user.push_str(&format!(
            "<conversation>\n{}\n</conversation>\n\n",
            history_lines.join("\n")
        ));
    }
    user.push_str(&format!("Question: {question}\n\n{ANSWER_REMINDER}"));

    BuiltPrompt {
        system: SYSTEM_PROMPT.to_string(),
        user,
        sources,
    }
}

/// Tokens the fixed parts of a prompt (system, reminder, question, history)
/// take, so retrieval knows how much is left for excerpts.
pub fn fixed_prompt_tokens(question: &str, history: &[ChatTurn]) -> usize {
    let history_tokens: usize = history
        .iter()
        .rev()
        .take(MAX_HISTORY_MESSAGES)
        .map(|t| {
            let capped: String = t.content.chars().take(MAX_HISTORY_MESSAGE_CHARS).collect();
            rough_token_count(&capped) + 4
        })
        .sum();
    rough_token_count(SYSTEM_PROMPT)
        + rough_token_count(ANSWER_REMINDER)
        + rough_token_count(question)
        + history_tokens
        + 100
}

/// Per-meeting scaffolding (the `<meeting …>` tags) in tokens.
pub const MEETING_SCAFFOLD_TOKENS: usize = 40;

// ---------------------------------------------------------------------------
// Citations
// ---------------------------------------------------------------------------

/// A piece of an answer: plain text, or a citation of one source.
#[derive(Debug, Clone, PartialEq)]
pub enum CrossSpan {
    Text(String),
    Citation {
        /// The citation as written, brackets included.
        label: String,
        /// Index into the answer's sources.
        source: usize,
        /// Cited moment, if the citation had a timestamp.
        seconds: Option<f64>,
    },
}

const TS_PLAIN: &str = r"(?:\d{1,2}:)?\d{1,3}:[0-5]\d";

static CITATION_RE: Lazy<Regex> = Lazy::new(|| {
    let ts = |p: &str| {
        format!(r"(?:(?P<{p}h>\d{{1,2}}):)?(?P<{p}m>\d{{1,3}}):(?P<{p}s>[0-5]\d)")
    };
    let range = format!(r"(?:\s*[-–]\s*{TS_PLAIN})?");
    let pattern = [
        // 1. [M1 12:34] / [M1, 12:34] / [M1 @ 12:34] / [M1: 12:34-13:00] / [M1]
        format!(
            r"\[\s*M(?P<ref>\d{{1,2}})(?:(?:\s*[,;:@·-]\s*|\s+){}{range})?\s*\]",
            ts("a")
        ),
        // 2. bare [12:34] — belongs to the meeting in context
        format!(r"\[{}{range}\]", ts("b")),
        // 3. a plain mention of a label ("in M2, …"): sets the context only
        r"\bM(?P<mention>\d{1,2})\b".to_string(),
    ]
    .join("|");
    Regex::new(&pattern).expect("valid cross-meeting citation regex")
});

fn caps_seconds(caps: &regex::Captures, prefix: &str) -> Option<f64> {
    let minutes: f64 = caps.name(&format!("{prefix}m"))?.as_str().parse().ok()?;
    let secs: f64 = caps.name(&format!("{prefix}s"))?.as_str().parse().ok()?;
    let hours: f64 = caps
        .name(&format!("{prefix}h"))
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0.0);
    Some(hours * 3600.0 + minutes * 60.0 + secs)
}

/// Split an answer into text and citations of its `source_count` sources.
pub fn parse_citations(answer: &str, source_count: usize) -> Vec<CrossSpan> {
    let resolve_label = |n: &str| -> Option<usize> {
        let n: usize = n.parse().ok()?;
        (n >= 1 && n <= source_count).then(|| n - 1)
    };
    let mut spans = Vec::new();
    let mut text = String::new();
    let mut last = 0;
    // The meeting a bare `[mm:ss]` belongs to.
    let mut current: Option<usize> = (source_count == 1).then_some(0);

    for caps in CITATION_RE.captures_iter(answer) {
        let whole = caps.get(0).expect("match");
        text.push_str(&answer[last..whole.start()]);
        last = whole.end();

        let citation = if let Some(r) = caps.name("ref") {
            resolve_label(r.as_str()).map(|source| {
                current = Some(source);
                (source, caps_seconds(&caps, "a"))
            })
        } else if caps.name("bm").is_some() {
            current.map(|source| (source, caps_seconds(&caps, "b")))
        } else {
            // A plain mention ("in M2, …") only sets the context.
            if let Some(source) = caps.name("mention").and_then(|m| resolve_label(m.as_str())) {
                current = Some(source);
            }
            None
        };

        match citation {
            Some((source, seconds)) => {
                if !text.is_empty() {
                    spans.push(CrossSpan::Text(std::mem::take(&mut text)));
                }
                spans.push(CrossSpan::Citation {
                    label: whole.as_str().to_string(),
                    source,
                    seconds,
                });
            }
            None => text.push_str(whole.as_str()),
        }
    }
    text.push_str(&answer[last..]);
    if !text.is_empty() {
        spans.push(CrossSpan::Text(text));
    }
    spans
}

/// URL scheme of the links [`link_citations`] produces.
pub const CITATION_LINK_SCHEME: &str = "parley-cite:";

/// Render an answer as safe markdown: answer text escaped (see
/// [`escape_untrusted_markdown`]), each citation a `parley-cite:` link whose
/// URL is built only from the resolved source index and seconds.
pub fn link_citations(answer: &str, source_count: usize) -> String {
    parse_citations(answer, source_count)
        .into_iter()
        .map(|span| match span {
            CrossSpan::Text(text) => escape_untrusted_markdown(&text),
            CrossSpan::Citation {
                label,
                source,
                seconds,
            } => {
                let target = match seconds {
                    Some(s) => format!("{CITATION_LINK_SCHEME}{source}@{}", s as u64),
                    None => format!("{CITATION_LINK_SCHEME}{source}"),
                };
                format!("[{}]({target})", escape_untrusted_markdown(&label))
            }
        })
        .collect()
}

/// The (source index, seconds) a [`link_citations`] link points at, if `url`
/// is one.
pub fn citation_link_target(url: &str) -> Option<(usize, Option<f64>)> {
    let rest = url.strip_prefix(CITATION_LINK_SCHEME)?;
    match rest.split_once('@') {
        Some((source, secs)) => Some((source.parse().ok()?, Some(secs.parse::<u64>().ok()? as f64))),
        None => Some((rest.parse().ok()?, None)),
    }
}

static STRIP_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!(
        r"\s?\[\s*M\d{{1,2}}(?:(?:\s*[,;:@·-]\s*|\s+){TS_PLAIN}(?:\s*[-–]\s*{TS_PLAIN})?)?\s*\]|\s?\[{TS_PLAIN}(?:\s*[-–]\s*{TS_PLAIN})?\]"
    ))
    .expect("valid strip regex")
});

/// `text` with every citation removed (for carrying old answers into a new
/// prompt, whose labels mean different meetings).
pub fn strip_citations(text: &str) -> String {
    STRIP_RE.replace_all(text, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn seg(start: f64, speaker: &str, text: &str) -> ContextSegment {
        ContextSegment {
            start: Some(start),
            speaker: speaker.into(),
            text: text.into(),
        }
    }

    fn excerpts(id: &str, title: &str, passages: Vec<Vec<ContextSegment>>) -> MeetingExcerpts {
        MeetingExcerpts {
            meeting_id: id.into(),
            title: title.into(),
            created_at: Some(Utc.with_ymd_and_hms(2026, 8, 14, 12, 0, 0).unwrap()),
            passages,
        }
    }

    #[test]
    fn prompt_groups_excerpts_by_meeting_with_labels_titles_and_dates() {
        let meetings = vec![
            excerpts(
                "a",
                "Platform \"sync\" <b>",
                vec![
                    vec![seg(190.0, "Alice", "Where are we on the migration?"), seg(205.0, "Kingfisher", "Cut-over is the 30th.")],
                    vec![seg(2460.0, "Kingfisher", "Migration rollback plan is ready.")],
                ],
            ),
            excerpts("b", "", vec![vec![seg(5.0, "Bob", "Migration done.")]]),
        ];
        let history = vec![
            ChatTurn { is_user: true, content: "When is the cut-over?".into() },
            ChatTurn { is_user: false, content: "The 30th [M1 03:25].".into() },
        ];
        let p = build_prompt(&meetings, &history, " And the rollback? ", Some("Kingfisher"));
        let u = &p.user;
        assert!(u.starts_with("(Search limited to: Kingfisher.)"));
        assert!(u.contains("<meeting ref=\"M1\" title=\"Platform 'sync'  b\" date=\""), "{u}");
        assert!(u.contains("2026\">\n[03:10] Alice: Where are we on the migration?\n[03:25] Kingfisher: Cut-over is the 30th.\n…\n[41:00] Kingfisher: Migration rollback plan is ready.\n</meeting>"), "{u}");
        assert!(u.contains("<meeting ref=\"M2\" title=\"Untitled meeting\""));
        // History keeps prose but drops stale citations.
        assert!(u.contains("Assistant: The 30th.\n"), "{u}");
        assert!(u.ends_with(&format!("Question: And the rollback?\n\n{ANSWER_REMINDER}")));
        assert_eq!(p.sources.len(), 2);
        assert_eq!(p.sources[0].label, "M1");
        assert_eq!(p.sources[0].meeting_id, "a");
        assert_eq!(p.sources[0].first_seconds, Some(190.0));
        assert_eq!(p.sources[1].label, "M2");
        let pos = |s: &str| u.find(s).unwrap();
        assert!(pos("ref=\"M1\"") < pos("ref=\"M2\""));
        assert!(pos("</meeting>") < pos("<conversation>"));
    }

    #[test]
    fn parses_meeting_citations_in_the_forms_models_write() {
        let spans = parse_citations(
            "Cut-over [M1 03:25], rollback [M1, 41:00-41:30], done [M2 @ 1:00:05] and [M2]. Also [M3 00:10].",
            2,
        );
        let cites: Vec<(usize, Option<f64>)> = spans
            .iter()
            .filter_map(|s| match s {
                CrossSpan::Citation { source, seconds, .. } => Some((*source, *seconds)),
                _ => None,
            })
            .collect();
        assert_eq!(
            cites,
            vec![(0, Some(205.0)), (0, Some(2460.0)), (1, Some(3605.0)), (1, None)]
        );
        // M3 isn't a source: left as text.
        assert!(matches!(spans.last(), Some(CrossSpan::Text(t)) if t.contains("[M3 00:10]")));
    }

    #[test]
    fn bare_timestamps_resolve_to_the_meeting_in_context() {
        // After "M2" is mentioned, a bare timestamp belongs to M2.
        let spans = parse_citations("[12:00] first. In M2, they agreed [00:30].", 2);
        assert_eq!(spans[0], CrossSpan::Text("[12:00] first. In M2, they agreed ".into()));
        assert_eq!(
            spans[1],
            CrossSpan::Citation { label: "[00:30]".into(), source: 1, seconds: Some(30.0) }
        );
        // With a single source a bare timestamp is unambiguous.
        let spans = parse_citations("Agreed [00:30].", 1);
        assert!(matches!(spans[1], CrossSpan::Citation { source: 0, seconds: Some(s), .. } if s == 30.0));
        // No sources: nothing is a citation.
        assert_eq!(parse_citations("[M1 00:30]", 0), vec![CrossSpan::Text("[M1 00:30]".into())]);
        assert!(parse_citations("", 3).is_empty());
    }

    #[test]
    fn links_are_built_from_source_index_only_and_text_is_escaped() {
        let linked = link_citations("Yes [M2 03:25]. ![x](https://evil.example/?q=1) <img src=x> [M1]", 2);
        assert_eq!(
            linked,
            "Yes [\\[M2 03:25\\]](parley-cite:1@205). !\\[x\\](https://evil.example/?q=1) \\<img src=x> [\\[M1\\]](parley-cite:0)"
        );
        assert_eq!(citation_link_target("parley-cite:1@205"), Some((1, Some(205.0))));
        assert_eq!(citation_link_target("parley-cite:0"), Some((0, None)));
        assert_eq!(citation_link_target("parley-cite:x@1"), None);
        assert_eq!(citation_link_target("parley-cite:1@-3"), None);
        assert_eq!(citation_link_target("https://example.com"), None);
        assert_eq!(citation_link_target("parley-time:5"), None);
    }

    #[test]
    fn hostile_answers_produce_no_other_links() {
        for h in [
            "![x](https://evil.example/a)",
            "[M1](https://evil.example/b)",
            "[[M1 00:01]](https://evil.example/c)",
            "\\[M1 00:01](https://evil.example/d)",
            "<https://evil.example/auto>",
            "[r]: https://evil.example/ref",
        ] {
            let linked = link_citations(&format!("ok {h}"), 1);
            // Every link target in the output is ours.
            for (ix, _) in linked.match_indices("](") {
                let target = &linked[ix + 2..];
                let escaped = linked[..ix].ends_with('\\');
                assert!(
                    escaped || target.starts_with(CITATION_LINK_SCHEME),
                    "foreign link in {linked:?}"
                );
            }
            // No unescaped '<'.
            let mut prev_bs = 0usize;
            for c in linked.chars() {
                if c == '<' {
                    assert!(prev_bs % 2 == 1, "unescaped < in {linked}");
                }
                prev_bs = if c == '\\' { prev_bs + 1 } else { 0 };
            }
        }
    }

    #[test]
    fn strips_citations_from_history() {
        assert_eq!(strip_citations("A [M1 03:25], B [12:00] and [M2]."), "A, B and.");
        assert_eq!(strip_citations("no refs [here]"), "no refs [here]");
    }
}
