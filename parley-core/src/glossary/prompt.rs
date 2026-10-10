//! Whisper initial-prompt construction for the custom vocabulary.
//!
//! Whisper conditions its decoder on an "initial prompt" — text it treats
//! as the transcript immediately preceding the audio. Spelling a term there
//! makes the decoder much more likely to produce that spelling, which is
//! the whole trick behind glossary support.
//!
//! The constraint is size: whisper.cpp keeps at most `n_text_ctx / 2`
//! (224) prompt tokens and, crucially, keeps the *last* ones — anything
//! past the budget is silently cut from the **front**. The batch path also
//! appends a rolling tail of already-transcribed text after the glossary,
//! so a long tail would push the glossary out first. Two budgets prevent
//! that:
//!
//! - the glossary sentence itself is capped at [`GLOSSARY_TOKEN_BUDGET`],
//!   filled in priority order (manual terms first, then voice-profile
//!   names, then calendar attendees — see `glossary::prepare`);
//! - the context tail gets whatever is left of [`TOTAL_PROMPT_TOKEN_BUDGET`]
//!   ([`compose`] trims it from the front, whole words at a time).
//!
//! We don't have whisper's BPE tokenizer here without a loaded model, so
//! token counts are a deliberately pessimistic estimate
//! ([`estimate_tokens`]): proper nouns and acronyms — exactly what a
//! glossary is full of — tokenize far worse than ordinary English prose.

/// Hard ceiling whisper.cpp applies to prompt tokens (`n_text_ctx / 2`).
pub const WHISPER_PROMPT_TOKEN_LIMIT: usize = 224;

/// Budget for the whole prompt (glossary + context tail). Kept below
/// [`WHISPER_PROMPT_TOKEN_LIMIT`] to absorb estimation error.
pub const TOTAL_PROMPT_TOKEN_BUDGET: usize = 200;

/// Budget for the glossary sentence alone, leaving roughly half the prompt
/// for the rolling context tail (which carries casing/punctuation
/// consistency between segments).
pub const GLOSSARY_TOKEN_BUDGET: usize = 96;

/// Context-tail length (bytes) used when there's no glossary — the
/// long-standing batch-path behaviour.
pub const DEFAULT_TAIL_CHARS: usize = 600;

/// Leading label of the glossary sentence. Also what
/// [`strip_prompt_echo`] looks for when whisper regurgitates the prompt.
pub const PREFIX: &str = "Glossary:";

/// Pessimistic token estimate for `text`: per whitespace-separated word,
/// one token per three ASCII characters (rounded up, minimum one), and one
/// token per non-ASCII character. Ordinary English averages ~4 chars per
/// token, so this over-counts prose — the point is to never under-count
/// the names and acronyms a glossary is made of.
pub fn estimate_tokens(text: &str) -> usize {
    text.split_whitespace()
        .map(|word| {
            let ascii = word.chars().filter(|c| c.is_ascii()).count();
            let non_ascii = word.chars().count() - ascii;
            ascii.div_ceil(3).max(if non_ascii == 0 { 1 } else { 0 }) + non_ascii
        })
        .sum()
}

/// Result of [`build_glossary_prompt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltPrompt {
    /// The sentence to hand whisper, e.g. `"Glossary: Kingfisher, TMU."`.
    pub text: String,
    /// Terms that made it into `text`, in order.
    pub included: Vec<String>,
    /// Terms left out because the budget ran out. Blank and duplicate
    /// terms are skipped silently and don't appear here.
    pub dropped: Vec<String>,
}

/// Normalise a term for the prompt: no NULs/newlines (whisper-rs panics on
/// interior NULs), no commas (they're the list separator), collapsed
/// whitespace, no trailing sentence punctuation.
fn sanitize_term(term: &str) -> String {
    let cleaned: String = term
        .chars()
        .map(|c| if c == ',' || c.is_control() { ' ' } else { c })
        .collect();
    cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(['.', ';', ':'])
        .to_string()
}

/// Build the glossary sentence from `terms`, which must already be in
/// priority order (highest first). Duplicates (case-insensitive) and blank
/// terms are skipped. Each term is added if it still fits within
/// `token_budget`; a term that doesn't fit is dropped but later (shorter,
/// lower-priority) terms are still tried, so the budget is used fully
/// without ever displacing a higher-priority term.
///
/// Returns `None` when no term fits (or none were given).
pub fn build_glossary_prompt<S: AsRef<str>>(terms: &[S], token_budget: usize) -> Option<BuiltPrompt> {
    let mut seen = std::collections::HashSet::new();
    let mut included: Vec<String> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();

    // "Glossary:" plus the closing "." — counted once up front.
    let mut used = estimate_tokens(PREFIX) + 1;

    for raw in terms {
        let term = sanitize_term(raw.as_ref());
        if term.is_empty() || !seen.insert(term.to_lowercase()) {
            continue;
        }
        // +1 for the ", " separator.
        let cost = estimate_tokens(&term) + 1;
        if used + cost <= token_budget {
            used += cost;
            included.push(term);
        } else {
            dropped.push(term);
        }
    }

    if included.is_empty() {
        return None;
    }

    Some(BuiltPrompt {
        text: format!("{} {}.", PREFIX, included.join(", ")),
        included,
        dropped,
    })
}

/// How many (estimated) tokens of context tail fit alongside
/// `glossary_prompt` without pushing the total past
/// [`TOTAL_PROMPT_TOKEN_BUDGET`].
pub fn tail_token_budget(glossary_prompt: &str) -> usize {
    TOTAL_PROMPT_TOKEN_BUDGET.saturating_sub(estimate_tokens(glossary_prompt))
}

/// Keep at most the last `max_chars` bytes of `tail`, cut forward to a
/// word boundary so whisper never sees half a word. Used when there's no
/// glossary (the long-standing batch-path behaviour).
fn trim_tail_chars(tail: &str, max_chars: usize) -> &str {
    let tail = tail.trim();
    if tail.len() <= max_chars {
        return tail;
    }
    let mut cut = tail.len() - max_chars;
    while !tail.is_char_boundary(cut) {
        cut += 1;
    }
    let rest = &tail[cut..];
    // Drop the (probably partial) first word unless we cut exactly at a space.
    if tail[..cut].ends_with(char::is_whitespace) {
        rest.trim_start()
    } else {
        match rest.find(char::is_whitespace) {
            Some(i) => rest[i..].trim_start(),
            None => "",
        }
    }
}

/// Keep the longest run of whole trailing words of `tail` whose estimated
/// token count fits in `max_tokens`.
fn trim_tail_tokens(tail: &str, max_tokens: usize) -> &str {
    let tail = tail.trim();
    let mut used = 0usize;
    let mut start = tail.len();
    let mut remaining = tail;
    while let Some(word) = remaining.split_whitespace().next_back() {
        let cost = estimate_tokens(word);
        if used + cost > max_tokens {
            break;
        }
        used += cost;
        // `remaining` is trimmed, so the last word sits at its very end.
        start = remaining.len() - word.len();
        remaining = remaining[..start].trim_end();
    }
    &tail[start..]
}

/// Assemble the full initial prompt for one batch segment: the glossary
/// sentence first, then as much of the rolling context tail as the budget
/// allows. Returns `None` when there's nothing to prompt with.
pub fn compose(glossary_prompt: Option<&str>, context_tail: &str) -> Option<String> {
    match glossary_prompt.filter(|g| !g.is_empty()) {
        None => {
            let tail = trim_tail_chars(context_tail, DEFAULT_TAIL_CHARS);
            (!tail.is_empty()).then(|| tail.to_string())
        }
        Some(g) => {
            let tail = trim_tail_tokens(context_tail, tail_token_budget(g));
            if tail.is_empty() {
                Some(g.to_string())
            } else {
                Some(format!("{} {}", g, tail))
            }
        }
    }
}

/// Whisper occasionally regurgitates its prompt on quiet or very short
/// audio. Remove any echoed glossary sentence — from a case-insensitive
/// `"Glossary:"` up to and including the next full stop (or end of text) —
/// so it never lands in the transcript. Text without the marker is
/// returned unchanged.
pub fn strip_prompt_echo(text: &str) -> String {
    let marker = PREFIX.to_lowercase();
    let mut out = text.to_string();
    loop {
        let lower = out.to_lowercase();
        // `to_lowercase` can change byte lengths for some non-ASCII text;
        // only trust the index when the lengths agree.
        if lower.len() != out.len() {
            return out;
        }
        let Some(start) = lower.find(&marker) else {
            return out;
        };
        let end = out[start..]
            .find('.')
            .map(|i| start + i + 1)
            .unwrap_or(out.len());
        out.replace_range(start..end, " ");
        out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_is_pessimistic_for_names() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("TMU"), 1);
        assert_eq!(estimate_tokens("Kingfisher"), 4);
        assert_eq!(estimate_tokens("SamPro, TMU."), 3 + 2);
        // Non-ASCII counts one per char.
        assert_eq!(estimate_tokens("Zoë"), 1 + 1);
        assert_eq!(estimate_tokens("東京"), 2);
    }

    #[test]
    fn builds_natural_sentence() {
        let built =
            build_glossary_prompt(&["Kingfisher", "SamPro", "TMU", "ECA"], GLOSSARY_TOKEN_BUDGET)
                .unwrap();
        assert_eq!(built.text, "Glossary: Kingfisher, SamPro, TMU, ECA.");
        assert_eq!(built.included.len(), 4);
        assert!(built.dropped.is_empty());
    }

    #[test]
    fn empty_and_duplicate_terms_are_skipped() {
        assert!(build_glossary_prompt::<&str>(&[], 100).is_none());
        assert!(build_glossary_prompt(&["  ", ""], 100).is_none());
        let built = build_glossary_prompt(&["TMU", "tmu", " TMU "], 100).unwrap();
        assert_eq!(built.text, "Glossary: TMU.");
        assert!(built.dropped.is_empty());
    }

    #[test]
    fn terms_are_sanitized() {
        let built = build_glossary_prompt(&["Acme,\nInc.", "a\0b"], 100).unwrap();
        assert_eq!(built.text, "Glossary: Acme Inc, a b.");
    }

    #[test]
    fn truncates_by_priority_and_backfills_with_shorter_terms() {
        // Budget 10: prefix (3) + "." (1) = 4 used up front.
        // "Kingfisher" costs 4+1=5 -> fits (9). "Supercalifragilistic"
        // costs 7+1 -> dropped. "TMU" costs 2 -> 11 > 10, dropped too.
        let built =
            build_glossary_prompt(&["Kingfisher", "Supercalifragilistic", "TMU"], 10).unwrap();
        assert_eq!(built.included, vec!["Kingfisher"]);
        assert_eq!(built.dropped, vec!["Supercalifragilistic", "TMU"]);

        // A shorter later term fills leftover budget after a long one is dropped.
        let built =
            build_glossary_prompt(&["Kingfisher", "Supercalifragilistic", "AB"], 11).unwrap();
        assert_eq!(built.included, vec!["Kingfisher", "AB"]);
        assert_eq!(built.dropped, vec!["Supercalifragilistic"]);
    }

    #[test]
    fn huge_glossary_stays_within_budget() {
        let terms: Vec<String> = (0..500).map(|i| format!("Projectname{}", i)).collect();
        let built = build_glossary_prompt(&terms, GLOSSARY_TOKEN_BUDGET).unwrap();
        assert!(estimate_tokens(&built.text) <= GLOSSARY_TOKEN_BUDGET);
        assert!(!built.dropped.is_empty());
        // Priority order preserved: the first terms are the ones kept.
        assert_eq!(built.included[0], "Projectname0");
        assert_eq!(built.included.len() + built.dropped.len(), 500);
    }

    #[test]
    fn tail_budget_shrinks_with_glossary() {
        let g = "Glossary: Kingfisher, SamPro, TMU, ECA.";
        assert_eq!(tail_token_budget(g), TOTAL_PROMPT_TOKEN_BUDGET - estimate_tokens(g));
        assert_eq!(trim_tail_tokens("aaa bbb ccc", 2), "bbb ccc");
        assert_eq!(trim_tail_tokens("aaa bbb ccc", 0), "");
        assert_eq!(trim_tail_tokens("  aaa  ", 5), "aaa");
        assert_eq!(trim_tail_tokens("dog dog dog", 2), "dog dog");
    }

    #[test]
    fn compose_without_glossary_keeps_legacy_char_cap() {
        assert_eq!(compose(None, ""), None);
        assert_eq!(compose(None, "  hello there "), Some("hello there".to_string()));
        let tail = "word ".repeat(300);
        let prompt = compose(None, &tail).unwrap();
        assert!(prompt.len() <= DEFAULT_TAIL_CHARS);
        assert!(prompt.starts_with("word"));
    }

    #[test]
    fn compose_puts_glossary_first_and_bounds_total() {
        assert_eq!(compose(Some("Glossary: TMU."), ""), Some("Glossary: TMU.".to_string()));

        let g = build_glossary_prompt(
            &(0..200).map(|i| format!("Term{}", i)).collect::<Vec<_>>(),
            GLOSSARY_TOKEN_BUDGET,
        )
        .unwrap()
        .text;
        let tail = "the quick brown fox jumps over the lazy dog ".repeat(40);
        let prompt = compose(Some(&g), &tail).unwrap();
        assert!(prompt.starts_with(&g));
        assert!(estimate_tokens(&prompt) <= TOTAL_PROMPT_TOKEN_BUDGET);
        // The tail kept is the *end* of the context, in whole words.
        assert!(prompt.ends_with("lazy dog"));
        let kept_tail = prompt[g.len()..].trim_start();
        assert!(!kept_tail.is_empty());
        assert!(tail.trim().ends_with(kept_tail));
        assert!(tail.contains(&format!(" {}", kept_tail)));
    }

    #[test]
    fn trim_tail_handles_multibyte_and_exact_space_cuts() {
        assert_eq!(trim_tail_chars("héllo wörld", 6), "wörld");
        assert_eq!(trim_tail_chars("abc def", 3), "def");
        assert_eq!(trim_tail_chars("abcdef", 3), "");
    }

    #[test]
    fn strips_echoed_glossary() {
        assert_eq!(strip_prompt_echo("Hello there."), "Hello there.");
        assert_eq!(strip_prompt_echo("Glossary: Kingfisher, TMU."), "");
        assert_eq!(
            strip_prompt_echo("So glossary: Kingfisher, TMU. Let's begin."),
            "So Let's begin."
        );
        assert_eq!(strip_prompt_echo("Glossary: Kingfisher, TMU"), "");
    }
}
