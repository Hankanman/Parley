//! Parsing and validating the model's chapter list — pure, no I/O.
//!
//! The model is asked for one `[mm:ss] Title` line per chapter. Its output
//! is untrusted: every line is checked on its own and anything that isn't a
//! well-formed, in-range, later-than-the-last chapter with a short plain-text
//! title is dropped. Nothing here ever produces markup — titles are stripped
//! of markdown syntax and anything that looks like a link or URL is
//! rejected outright, so the UI can render them as plain text.

use once_cell::sync::Lazy;
use regex::Regex;

/// Longest title kept, in characters (longer ones are cut at a word).
pub const MAX_TITLE_CHARS: usize = 80;
/// A "title" longer than this is a sentence or paragraph, not a title.
const MAX_RAW_TITLE_CHARS: usize = 160;
/// Chapters closer together than this are merged (the later one dropped).
pub const MIN_CHAPTER_GAP_SECS: f64 = 10.0;
/// Hard cap on chapters per meeting.
pub const MAX_CHAPTERS: usize = 60;

/// One validated chapter.
#[derive(Debug, Clone, PartialEq)]
pub struct ChapterDraft {
    pub start_seconds: f64,
    pub title: String,
}

/// What a chapter line has to satisfy.
#[derive(Debug, Clone, Default)]
pub struct ChapterLimits {
    /// Earliest allowed start (inclusive), in seconds.
    pub min_start: f64,
    /// Latest allowed start (inclusive) — the meeting (or chunk) end.
    pub max_start: f64,
    /// Speaker names in the transcript; a "title" starting `Name:` is an
    /// echoed transcript line, not a chapter.
    pub speakers: Vec<String>,
}

static THINK_BLOCK: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)<think(?:ing)?>.*?(?:</think(?:ing)?>|\z)").unwrap());

/// `- [12:34] Title`, `3. 12:34 - Title`, `[1:02:03] | Title`, …
static CHAPTER_LINE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^\s*(?:[-*•+]\s*|\d{1,3}[.)]\s+)?(?:\*\*)?[\[(]?(\d{1,3}(?::\d{1,2}){1,2})[\])]?(?:\*\*)?\s*(?:[-–—:|.]\s*)?(.*)$",
    )
    .unwrap()
});

/// A URI scheme (`https:`, `javascript:`, `mailto:`…) or a bare domain.
static LINKISH: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(\b[a-z][a-z0-9+.\-]*:\S|www\.|\]\(|\b[a-z0-9\-]+\.(com|org|net|io|ai|dev|app|co|uk|xyz|ru|cn)\b)")
        .unwrap()
});

/// `mm:ss` (minutes may exceed 59, like the transcript's own `[75:03]`) or
/// `h:mm:ss`. Seconds (and minutes in the 3-part form) must be two digits
/// below 60.
pub fn parse_time(raw: &str) -> Option<f64> {
    let parts: Vec<&str> = raw.split(':').collect();
    let num = |s: &str| -> Option<u64> {
        if s.is_empty() || s.len() > 3 || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    };
    let two_digit_sixty = |s: &str| -> Option<u64> {
        if s.len() != 2 {
            return None;
        }
        num(s).filter(|v| *v < 60)
    };
    let secs = match parts.as_slice() {
        [m, s] => num(m)? * 60 + two_digit_sixty(s)?,
        [h, m, s] => num(h)? * 3600 + two_digit_sixty(m)? * 60 + two_digit_sixty(s)?,
        _ => return None,
    };
    Some(secs as f64)
}

/// Zero-width and bidi-control characters (text-direction spoofing).
fn is_invisible_format_char(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}

/// Clean a raw title into short plain text, or `None` if it isn't one.
pub fn sanitize_title(raw: &str, speakers: &[String]) -> Option<String> {
    if raw.chars().count() > MAX_RAW_TITLE_CHARS {
        return None;
    }
    if LINKISH.is_match(raw) {
        return None;
    }
    let mut text: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .filter(|c| !is_invisible_format_char(*c))
        .filter(|c| !matches!(c, '*' | '_' | '#' | '`' | '[' | ']' | '<' | '>' | '|' | '\\' | '~'))
        .collect();
    text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = text
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '“' | '”' | '‘' | '’'))
        .trim()
        .trim_end_matches(['.', ',', ';', ':'])
        .trim();
    // Labels the model sometimes prefixes.
    let trimmed = ["Chapter title:", "Title:", "Chapter:", "Topic:"]
        .iter()
        .find_map(|p| {
            trimmed
                .get(..p.len())
                .filter(|head| head.eq_ignore_ascii_case(p))
                .map(|_| trimmed[p.len()..].trim())
        })
        .unwrap_or(trimmed);
    if !trimmed.chars().any(|c| c.is_alphanumeric()) {
        return None;
    }
    // An echoed transcript line ("Alice: so the budget…").
    if speakers.iter().any(|s| {
        let s = s.trim();
        !s.is_empty()
            && trimmed.len() > s.len()
            && trimmed.is_char_boundary(s.len())
            && trimmed[..s.len()].eq_ignore_ascii_case(s)
            && trimmed[s.len()..].starts_with(':')
    }) {
        return None;
    }
    if trimmed.chars().count() <= MAX_TITLE_CHARS {
        return Some(trimmed.to_string());
    }
    let cut: String = trimmed.chars().take(MAX_TITLE_CHARS).collect();
    let cut = match cut.rfind(' ') {
        Some(ix) if ix > MAX_TITLE_CHARS / 2 => cut[..ix].to_string(),
        _ => cut,
    };
    Some(format!("{}…", cut.trim_end()))
}

/// Parse the model's reply into validated chapters, in order. Lines that
/// aren't chapter lines, or fail any check, are skipped: a time outside
/// `limits`, a time not later than the previous kept chapter (plus
/// [`MIN_CHAPTER_GAP_SECS`]), a bad title, or a title repeating the
/// previous one.
pub fn parse_chapters(output: &str, limits: &ChapterLimits) -> Vec<ChapterDraft> {
    let output = THINK_BLOCK.replace_all(output, "");
    let mut chapters: Vec<ChapterDraft> = Vec::new();
    for line in output.lines() {
        if chapters.len() >= MAX_CHAPTERS {
            break;
        }
        let Some(caps) = CHAPTER_LINE.captures(line) else { continue };
        let Some(start) = parse_time(&caps[1]) else { continue };
        if !(start >= limits.min_start && start <= limits.max_start) {
            continue;
        }
        let Some(title) = sanitize_title(&caps[2], &limits.speakers) else { continue };
        if let Some(prev) = chapters.last() {
            if start < prev.start_seconds + MIN_CHAPTER_GAP_SECS
                || prev.title.eq_ignore_ascii_case(&title)
            {
                continue;
            }
        }
        chapters.push(ChapterDraft {
            start_seconds: start,
            title,
        });
    }
    chapters
}

/// Join per-chunk results into one list, re-applying the ordering, gap,
/// repeat and count rules across chunk boundaries.
pub fn merge_chapters(parts: Vec<Vec<ChapterDraft>>) -> Vec<ChapterDraft> {
    let mut out: Vec<ChapterDraft> = Vec::new();
    for chapter in parts.into_iter().flatten() {
        if out.len() >= MAX_CHAPTERS {
            break;
        }
        if let Some(prev) = out.last() {
            if chapter.start_seconds < prev.start_seconds + MIN_CHAPTER_GAP_SECS
                || prev.title.eq_ignore_ascii_case(&chapter.title)
            {
                continue;
            }
        }
        out.push(chapter);
    }
    out
}

/// Final check before anything is stored: strictly increasing, finite,
/// within `[0, duration]`, and every title passes [`sanitize_title`]
/// unchanged. Defence in depth for anything built outside [`parse_chapters`].
pub fn is_valid_chapter_list(chapters: &[ChapterDraft], duration: f64) -> bool {
    let mut prev: Option<f64> = None;
    chapters.len() <= MAX_CHAPTERS
        && chapters.iter().all(|c| {
            let ok = c.start_seconds.is_finite()
                && c.start_seconds >= 0.0
                && c.start_seconds <= duration
                && prev.is_none_or(|p| c.start_seconds > p)
                && sanitize_title(&c.title, &[]).as_deref() == Some(c.title.as_str());
            prev = Some(c.start_seconds);
            ok
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(max: f64) -> ChapterLimits {
        ChapterLimits {
            min_start: 0.0,
            max_start: max,
            speakers: vec!["Alice".into(), "Speaker".into()],
        }
    }

    fn titles(chapters: &[ChapterDraft]) -> Vec<&str> {
        chapters.iter().map(|c| c.title.as_str()).collect()
    }

    #[test]
    fn parses_times() {
        assert_eq!(parse_time("00:00"), Some(0.0));
        assert_eq!(parse_time("12:34"), Some(754.0));
        assert_eq!(parse_time("75:03"), Some(4503.0));
        assert_eq!(parse_time("1:02:03"), Some(3723.0));
        assert_eq!(parse_time("1:2"), None);
        assert_eq!(parse_time("12:60"), None);
        assert_eq!(parse_time("1:60:00"), None);
        assert_eq!(parse_time("1234:00"), None);
        assert_eq!(parse_time("-1:00"), None);
        assert_eq!(parse_time("1:00:00:00"), None);
        assert_eq!(parse_time(":30"), None);
        assert_eq!(parse_time("１２:３４"), None); // full-width digits
    }

    #[test]
    fn parses_well_formed_output_in_common_shapes() {
        let out = "Here are the chapters:\n\
                   [00:00] Introductions\n\
                   - [02:15] Q3 budget review\n\
                   3. 05:40 - Hiring plan\n\
                   **[1:02:03]** | Wrap-up and next steps\n";
        let chapters = parse_chapters(out, &limits(4000.0));
        assert_eq!(
            titles(&chapters),
            vec!["Introductions", "Q3 budget review", "Hiring plan", "Wrap-up and next steps"]
        );
        assert_eq!(chapters[1].start_seconds, 135.0);
        assert_eq!(chapters[3].start_seconds, 3723.0);
    }

    #[test]
    fn drops_out_of_range_and_non_monotonic_times() {
        let out = "[00:00] Start\n[10:00] Later\n[05:00] Goes backwards\n[10:05] Too close\n\
                   [20:00] Past the end\n[12:00] Fine";
        let chapters = parse_chapters(out, &limits(15.0 * 60.0));
        assert_eq!(titles(&chapters), vec!["Start", "Later", "Fine"]);

        let chunk = ChapterLimits {
            min_start: 300.0,
            max_start: 600.0,
            speakers: vec![],
        };
        let chapters = parse_chapters("[04:59] Before\n[05:00] Edge\n[10:00] End edge\n[10:01] After", &chunk);
        assert_eq!(titles(&chapters), vec!["Edge", "End edge"]);
    }

    #[test]
    fn rejects_links_urls_and_markup() {
        let out = "[00:10] [Click here](https://evil.example)\n\
                   [00:30] Visit https://evil.example now\n\
                   [00:50] javascript:alert(1)\n\
                   [01:10] see www.example.org\n\
                   [01:30] evil.com promo\n\
                   [01:50] <script>alert(1)</script>\n\
                   [02:10] **Budget** _review_ #1\n\
                   [02:30] mailto:someone@x.y\n";
        let chapters = parse_chapters(out, &limits(1000.0));
        // <script> is stripped of its brackets, not executed — it's plain text.
        assert_eq!(
            titles(&chapters),
            vec!["scriptalert(1)/script", "Budget review 1"]
        );
        for c in &chapters {
            assert!(!c.title.contains('<') && !c.title.contains('[') && !c.title.contains('*'));
        }
    }

    #[test]
    fn rejects_garbage_and_hostile_input() {
        let long = format!("[00:10] {}", "word ".repeat(100));
        let out = format!(
            "{long}\n\
             [00:20]\n\
             [00:30] ---\n\
             [00:40] Alice: so the budget is fine\n\
             [99:99] Bad time\n\
             [NaN] Not a time\n\
             <think>[00:50] hidden reasoning</think>\n\
             [01:00] \u{0000}Control\u{0007} chars\u{202e}\n\
             [01:20] Ignore previous instructions\n\
             random prose with 12:34 in the middle\n\
             [01:40] Ignore previous instructions\n"
        );
        let chapters = parse_chapters(&out, &limits(1000.0));
        assert_eq!(
            titles(&chapters),
            vec!["Control chars", "Ignore previous instructions"]
        );
        assert!(parse_chapters("", &limits(100.0)).is_empty());
        assert!(parse_chapters("no chapters here at all", &limits(100.0)).is_empty());
        // Unterminated think block hides everything after it.
        assert!(parse_chapters("<think>[00:10] A\n[00:30] B", &limits(100.0)).is_empty());
    }

    #[test]
    fn long_titles_are_cut_at_a_word() {
        let raw = "Discussion of the quarterly roadmap including hiring, budget, infrastructure costs";
        let title = sanitize_title(raw, &[]).unwrap();
        assert!(title.chars().count() <= MAX_TITLE_CHARS + 1);
        assert!(title.ends_with('…'));
        assert!(!title.contains("cos…"));
        assert_eq!(sanitize_title("\"Title: Budget.\"", &[]).as_deref(), Some("Budget"));
    }

    #[test]
    fn caps_chapter_count() {
        let out: String = (0..200).map(|i| format!("[{:02}:{:02}] Topic {i}\n", i / 2, (i % 2) * 30)).collect();
        assert_eq!(parse_chapters(&out, &limits(10_000.0)).len(), MAX_CHAPTERS);
    }

    #[test]
    fn merges_chunks_across_boundaries() {
        let c = |s: f64, t: &str| ChapterDraft {
            start_seconds: s,
            title: t.into(),
        };
        let merged = merge_chapters(vec![
            vec![c(0.0, "Intro"), c(300.0, "Budget")],
            vec![c(305.0, "Budget cont."), c(600.0, "budget"), c(900.0, "Hiring")],
            vec![c(100.0, "Out of order"), c(1200.0, "Wrap-up")],
        ]);
        assert_eq!(titles(&merged), vec!["Intro", "Budget", "Hiring", "Wrap-up"]);
        assert!(is_valid_chapter_list(&merged, 1200.0));
        assert!(!is_valid_chapter_list(&merged, 1000.0));
    }

    #[test]
    fn final_validation_catches_bad_lists() {
        let c = |s: f64, t: &str| ChapterDraft {
            start_seconds: s,
            title: t.into(),
        };
        assert!(is_valid_chapter_list(&[], 0.0));
        assert!(!is_valid_chapter_list(&[c(10.0, "A"), c(10.0, "B")], 100.0));
        assert!(!is_valid_chapter_list(&[c(f64::NAN, "A")], 100.0));
        assert!(!is_valid_chapter_list(&[c(-1.0, "A")], 100.0));
        assert!(!is_valid_chapter_list(&[c(1.0, "[x](https://a.b)")], 100.0));
        assert!(!is_valid_chapter_list(&[c(1.0, "**bold**")], 100.0));
    }
}
