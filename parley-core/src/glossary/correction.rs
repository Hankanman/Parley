//! Deterministic glossary spelling correction.
//!
//! An optional post-pass (off by default) over finished transcript text
//! that rewrites near-misses of glossary terms to their canonical
//! spelling: "Sam Pro" -> "SamPro", "tmu" -> "TMU", "Kingfischer" ->
//! "Kingfisher", and — via a term's "sounds like" hints — "cooper netties"
//! -> "Kubernetes".
//!
//! ## Why not an LLM?
//!
//! The issue suggested an LLM pass constrained to "fix only misspellings of
//! these glossary terms". We deliberately do it deterministically instead:
//!
//! - **Bounded by construction.** The only edit this module can ever make
//!   is "replace a short span of words with a glossary term". An LLM asked
//!   to do the same can (and small local models do) rephrase, drop or
//!   summarise text; validating its output back down to that one edit
//!   type is the same matching problem solved here, plus a model call.
//! - **Local-first and free.** No summary provider needs to be configured,
//!   nothing leaves the machine, and it runs in microseconds per segment,
//!   so it can be applied to every segment of a multi-hour import.
//! - **Testable and predictable.** The same input always produces the
//!   same output, and every rule below is covered by unit tests.
//!
//! ## Matching rules (conservative on purpose)
//!
//! A *window* of 1..=N consecutive words is compared with each *variant*
//! (a term's canonical spelling, or one of its "sounds like" hints) by its
//! normalised key: lowercase alphanumerics only, spaces removed.
//!
//! 1. **Exact key match** -> replace. Handles casing ("tmu"), spacing
//!    ("Sam Pro"/"SamPro"), punctuation ("T.M.U."), and hints. The window
//!    may differ from the variant's word count by at most one word.
//! 2. **Plural/possessive** of an exact match keeps its suffix
//!    ("kingfishers" -> "Kingfishers", "SamPro's").
//! 3. **Fuzzy match** only when *all* of:
//!    - the window has exactly the variant's word count;
//!    - the variant key is at least 5 characters (short words and
//!      acronyms only ever match exactly — too many real words are one
//!      edit away from "TMU" or "Kate");
//!    - a single-word window isn't a common English word;
//!    - the edit distance (optimal string alignment) is within 1 for keys
//!      of 5–7 chars or 2 for 8+, and the window starts with the same
//!      letter as the variant **or** has the same phonetic key — in which
//!      case one extra edit is tolerated.
//!
//! Windows never span sentence punctuation (a word ending in `.`, `,`,
//! `?`, ...), so a correction can't glue two clauses together.

use super::GlossaryTerm;

/// Longest window (in words) considered. Hints like "cooper netties" are
/// two words; four leaves room for longer product names.
const MAX_WINDOW_WORDS: usize = 4;

/// Below this key length, only exact matches are accepted.
const MIN_FUZZY_KEY_LEN: usize = 5;

/// Frequent English words of 5+ letters that must never be fuzzily
/// "corrected" into a glossary term on their own (e.g. a term "Thera" must
/// not eat "there"). Exact matches are unaffected — if the user adds
/// "Could" to their glossary, that's on them.
const COMMON_WORDS: &[&str] = &[
    "about",
    "above",
    "after",
    "again",
    "against",
    "along",
    "already",
    "also",
    "always",
    "among",
    "another",
    "answer",
    "anything",
    "around",
    "asked",
    "basically",
    "because",
    "become",
    "before",
    "being",
    "believe",
    "below",
    "better",
    "between",
    "bring",
    "build",
    "called",
    "cannot",
    "carry",
    "cause",
    "certain",
    "change",
    "check",
    "clear",
    "close",
    "could",
    "course",
    "doing",
    "during",
    "early",
    "either",
    "enough",
    "every",
    "everyone",
    "everything",
    "example",
    "exactly",
    "fine",
    "first",
    "follow",
    "found",
    "further",
    "going",
    "gonna",
    "great",
    "group",
    "guess",
    "happen",
    "having",
    "heard",
    "issue",
    "issues",
    "later",
    "least",
    "leave",
    "little",
    "looking",
    "maybe",
    "means",
    "might",
    "money",
    "months",
    "never",
    "night",
    "nothing",
    "number",
    "often",
    "other",
    "others",
    "people",
    "person",
    "place",
    "plans",
    "point",
    "pretty",
    "probably",
    "problem",
    "process",
    "quite",
    "really",
    "right",
    "saying",
    "second",
    "seems",
    "should",
    "since",
    "small",
    "something",
    "sorry",
    "sound",
    "sounds",
    "start",
    "still",
    "story",
    "stuff",
    "sure",
    "taking",
    "talking",
    "thank",
    "thanks",
    "their",
    "there",
    "these",
    "thing",
    "things",
    "think",
    "third",
    "those",
    "though",
    "thought",
    "three",
    "through",
    "today",
    "together",
    "tomorrow",
    "totally",
    "trying",
    "under",
    "until",
    "using",
    "usually",
    "versus",
    "wanna",
    "wanted",
    "watch",
    "water",
    "where",
    "whether",
    "which",
    "while",
    "whole",
    "would",
    "write",
    "wrong",
    "years",
    "yesterday",
    "young",
    "yours",
];

/// One spelling the corrector can recognise, mapped to the canonical term.
#[derive(Debug, Clone)]
struct Variant {
    canonical: String,
    key: String,
    phonetic: String,
    words: usize,
}

/// A word of the input, split into leading punctuation, the core, and
/// trailing punctuation, so replacements keep the surrounding punctuation.
#[derive(Debug, Clone)]
struct Token<'a> {
    lead: &'a str,
    core: &'a str,
    trail: &'a str,
}

impl<'a> Token<'a> {
    fn parse(word: &'a str) -> Self {
        let start = word.find(|c: char| c.is_alphanumeric());
        let end = word
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_alphanumeric())
            .map(|(i, c)| i + c.len_utf8());
        match (start, end) {
            (Some(s), Some(e)) if s < e => Token {
                lead: &word[..s],
                core: &word[s..e],
                trail: &word[e..],
            },
            _ => Token {
                lead: word,
                core: "",
                trail: "",
            },
        }
    }

    /// A window may not continue past a word followed by punctuation.
    fn ends_clause(&self) -> bool {
        !self.trail.is_empty()
    }

    fn starts_clause(&self) -> bool {
        !self.lead.is_empty()
    }
}

/// A single replacement made by [`Corrector::correct`], for logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    pub from: String,
    pub to: String,
}

/// Builds the variant table once per batch; [`Corrector::correct`] is then
/// cheap per segment.
#[derive(Debug, Clone)]
pub struct Corrector {
    variants: Vec<Variant>,
}

/// Lowercase alphanumerics only (spaces and punctuation removed).
pub fn normalize_key(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Full-length Soundex-style phonetic key: letters mapped to their
/// consonant class (vowels and h/w/y dropped), adjacent duplicates
/// collapsed. Unlike classic Soundex the first letter is coded too (so
/// "Cate"/"Kate" agree) and the key isn't truncated to four characters
/// (truncation makes long words collide far too easily).
pub fn phonetic_key(key: &str) -> String {
    let mut out = String::new();
    let mut last: Option<char> = None;
    for c in key.chars() {
        let code = match c {
            'b' | 'f' | 'p' | 'v' => Some('1'),
            'c' | 'g' | 'j' | 'k' | 'q' | 's' | 'x' | 'z' => Some('2'),
            'd' | 't' => Some('3'),
            'l' => Some('4'),
            'm' | 'n' => Some('5'),
            'r' => Some('6'),
            '0'..='9' => Some(c),
            // Vowels separate repeated consonant codes; h/w/y don't.
            'a' | 'e' | 'i' | 'o' | 'u' => {
                last = None;
                None
            }
            _ => None,
        };
        if let Some(code) = code {
            if last != Some(code) {
                out.push(code);
            }
            last = Some(code);
        }
    }
    out
}

/// Optimal-string-alignment edit distance (Levenshtein + adjacent
/// transposition), over chars.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=m {
        d[0][j] = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[n][m]
}

fn allowed_distance(key_len: usize) -> usize {
    match key_len {
        0..MIN_FUZZY_KEY_LEN => 0,
        MIN_FUZZY_KEY_LEN..=7 => 1,
        _ => 2,
    }
}

/// How a window matched a variant; lower is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MatchQuality {
    Exact,
    Suffixed,
    Fuzzy(usize),
}

impl Corrector {
    /// Build a corrector from glossary terms. Returns `None` when there's
    /// nothing usable to match against.
    pub fn new(terms: &[GlossaryTerm]) -> Option<Self> {
        let mut variants = Vec::new();
        for term in terms {
            let canonical = term.term.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalize_key(&canonical).chars().count() < 2 {
                continue;
            }
            let spellings = std::iter::once(canonical.clone()).chain(term.aliases());
            for spelling in spellings {
                let key = normalize_key(&spelling);
                if key.chars().count() < 2 {
                    continue;
                }
                variants.push(Variant {
                    canonical: canonical.clone(),
                    phonetic: phonetic_key(&key),
                    words: spelling.split_whitespace().count().max(1),
                    key,
                });
            }
        }
        // Longer keys first, so "Kingfisher Cloud" beats "Kingfisher" on ties.
        variants.sort_by(|a, b| b.key.len().cmp(&a.key.len()));
        (!variants.is_empty()).then_some(Self { variants })
    }

    fn match_window(&self, tokens: &[Token]) -> Option<(MatchQuality, &Variant, &'static str)> {
        let n = tokens.len();
        let mut joined_core = String::new();
        for t in tokens {
            joined_core.push_str(t.core);
        }
        let window_key = normalize_key(&joined_core);
        if window_key.is_empty() {
            return None;
        }
        // Possessive suffix on the last word is kept and re-attached.
        let last_core = tokens[n - 1].core;
        // Matched against the original bytes (both cases spelled out) so
        // the stem slice below always lands on a char boundary.
        let possessive = ["'s", "'S", "’s", "’S"]
            .into_iter()
            .find(|s| last_core.ends_with(s) && last_core.len() > s.len());

        let mut best: Option<(MatchQuality, &Variant, &'static str)> = None;

        for v in &self.variants {
            let word_diff = n.abs_diff(v.words);
            let candidate = if window_key == v.key && word_diff <= 1 {
                Some((MatchQuality::Exact, ""))
            } else if let Some(suffix) = possessive.filter(|_| word_diff <= 1).and_then(|p| {
                let stem = normalize_key(&last_core[..last_core.len() - p.len()]);
                let prefix: String = tokens[..n - 1]
                    .iter()
                    .map(|t| normalize_key(t.core))
                    .collect();
                (format!("{prefix}{stem}") == v.key).then_some(p)
            }) {
                Some((MatchQuality::Suffixed, suffix))
            } else if word_diff <= 1
                && v.key.chars().count() >= 3
                && window_key.strip_suffix('s') == Some(v.key.as_str())
            {
                Some((MatchQuality::Suffixed, "s"))
            } else if n == v.words {
                self.fuzzy(&window_key, v, n)
                    .map(|d| (MatchQuality::Fuzzy(d), ""))
            } else {
                None
            };
            if let Some((q, suffix)) = candidate {
                if best.as_ref().is_none_or(|(bq, _, _)| q < *bq) {
                    best = Some((q, v, suffix));
                }
                if q == MatchQuality::Exact {
                    break;
                }
            }
        }
        best
    }

    fn fuzzy(&self, window_key: &str, v: &Variant, n: usize) -> Option<usize> {
        let len = v.key.chars().count();
        let allowed = allowed_distance(len);
        if allowed == 0 {
            return None;
        }
        if n == 1 && COMMON_WORDS.contains(&window_key) {
            return None;
        }
        let same_initial = window_key.chars().next() == v.key.chars().next();
        let same_sound = phonetic_key(window_key) == v.phonetic;
        let limit = match (same_initial, same_sound) {
            (_, true) => allowed + 1,
            (true, false) => allowed,
            (false, false) => return None,
        };
        if window_key.chars().count().abs_diff(len) > limit {
            return None;
        }
        let d = edit_distance(window_key, &v.key);
        (d <= limit).then_some(d)
    }

    /// Correct `text`, returning the new text and the replacements made.
    /// When nothing matches, the original text is returned unchanged
    /// (byte-for-byte, including its whitespace).
    pub fn correct_with_report(&self, text: &str) -> (String, Vec<Replacement>) {
        let words: Vec<&str> = text.split_whitespace().collect();
        let tokens: Vec<Token> = words.iter().map(|w| Token::parse(w)).collect();
        let mut out: Vec<String> = Vec::with_capacity(words.len());
        let mut replacements = Vec::new();

        let mut i = 0;
        while i < tokens.len() {
            let mut matched = None;
            if !tokens[i].core.is_empty() {
                for n in (1..=MAX_WINDOW_WORDS.min(tokens.len() - i)).rev() {
                    let window = &tokens[i..i + n];
                    // No window may cross clause punctuation or include an
                    // empty (punctuation-only) token.
                    let crosses = window[..n - 1].iter().any(Token::ends_clause)
                        || window[1..].iter().any(Token::starts_clause)
                        || window.iter().any(|t| t.core.is_empty());
                    if crosses {
                        continue;
                    }
                    if let Some(m) = self.match_window(window) {
                        matched = Some((n, m));
                        break;
                    }
                }
            }

            match matched {
                Some((n, (_, variant, suffix))) => {
                    let original: String = words[i..i + n].join(" ");
                    let replaced = format!(
                        "{}{}{}{}",
                        tokens[i].lead,
                        variant.canonical,
                        suffix,
                        tokens[i + n - 1].trail
                    );
                    if replaced != original {
                        replacements.push(Replacement {
                            from: original,
                            to: replaced.clone(),
                        });
                    }
                    out.push(replaced);
                    i += n;
                }
                None => {
                    out.push(words[i].to_string());
                    i += 1;
                }
            }
        }

        if replacements.is_empty() {
            (text.to_string(), replacements)
        } else {
            (out.join(" "), replacements)
        }
    }

    /// Correct `text`; see [`Corrector::correct_with_report`].
    pub fn correct(&self, text: &str) -> String {
        self.correct_with_report(text).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corrector(terms: &[(&str, &str)]) -> Corrector {
        let terms: Vec<GlossaryTerm> = terms
            .iter()
            .map(|(t, s)| GlossaryTerm::new(t.to_string(), s.to_string()))
            .collect();
        Corrector::new(&terms).unwrap()
    }

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("abcd", "abdc"), 1); // transposition
        assert_eq!(edit_distance("same", "same"), 0);
    }

    #[test]
    fn phonetic_key_codes_first_letter_and_collapses() {
        assert_eq!(phonetic_key("cate"), phonetic_key("kate"));
        assert_eq!(phonetic_key("kingfisher"), phonetic_key("kingfischer"));
        assert_ne!(phonetic_key("kingfisher"), phonetic_key("kingdom"));
    }

    #[test]
    fn empty_glossary_builds_no_corrector() {
        assert!(Corrector::new(&[]).is_none());
        assert!(Corrector::new(&[GlossaryTerm::new("a".into(), String::new())]).is_none());
    }

    #[test]
    fn fixes_case_spacing_and_punctuation_exactly() {
        let c = corrector(&[("TMU", ""), ("SamPro", ""), ("ECA", "")]);
        assert_eq!(c.correct("the tmu is down"), "the TMU is down");
        assert_eq!(c.correct("we use Sam Pro daily"), "we use SamPro daily");
        assert_eq!(c.correct("ask the T.M.U. team"), "ask the TMU. team");
        assert_eq!(c.correct("Is it ECA?"), "Is it ECA?");
        assert_eq!(c.correct("(sampro), right"), "(SamPro), right");
    }

    #[test]
    fn keeps_plural_and_possessive_suffixes() {
        let c = corrector(&[("Kingfisher", ""), ("SamPro", "")]);
        assert_eq!(c.correct("two kingfishers here"), "two Kingfishers here");
        assert_eq!(c.correct("sampro's roadmap"), "SamPro's roadmap");
    }

    #[test]
    fn fuzzy_fixes_near_misses_of_long_terms() {
        let c = corrector(&[("Kingfisher", ""), ("Grafana", "")]);
        assert_eq!(
            c.correct("the Kingfischer project"),
            "the Kingfisher project"
        );
        assert_eq!(c.correct("open grafanna now"), "open Grafana now");
    }

    #[test]
    fn sounds_like_hints_map_to_canonical() {
        let c = corrector(&[("Kubernetes", "cooper netties, cube natives")]);
        assert_eq!(
            c.correct("deploy to cooper netties today"),
            "deploy to Kubernetes today"
        );
        assert_eq!(
            c.correct("deploy to cube natives."),
            "deploy to Kubernetes."
        );
        // Fuzzy against a multi-word hint, same word count.
        assert_eq!(c.correct("on cooper nettys"), "on Kubernetes");
    }

    #[test]
    fn short_terms_never_fuzzy_match() {
        let c = corrector(&[("TMU", ""), ("Kate", "")]);
        assert_eq!(c.correct("the tmo and the emu"), "the tmo and the emu");
        assert_eq!(c.correct("we ate late"), "we ate late");
    }

    #[test]
    fn common_words_are_protected_from_fuzzy() {
        let c = corrector(&[("Thera", ""), ("Woulda", "")]);
        assert_eq!(
            c.correct("there it is, would you"),
            "there it is, would you"
        );
        // ...but an exact (case-only) match still applies.
        assert_eq!(c.correct("thera is here"), "Thera is here");
    }

    #[test]
    fn unrelated_words_are_left_alone() {
        let c = corrector(&[("Kingfisher", ""), ("Grafana", ""), ("SamPro", "")]);
        let text = "Let's review the quarterly numbers and the hiring plan.";
        assert_eq!(c.correct(text), text);
        // Different initial and different sound: no fuzzy match.
        assert_eq!(c.correct("a ringfisher"), "a ringfisher");
    }

    #[test]
    fn windows_do_not_cross_punctuation() {
        let c = corrector(&[("SamPro", "")]);
        assert_eq!(c.correct("I'm Sam. Pro tip"), "I'm Sam. Pro tip");
        assert_eq!(c.correct("Sam, pro"), "Sam, pro");
    }

    #[test]
    fn unchanged_text_is_returned_verbatim() {
        let c = corrector(&[("Kingfisher", "")]);
        let text = "  double  spaced   text ";
        assert_eq!(c.correct(text), text);
    }

    #[test]
    fn report_lists_replacements() {
        let c = corrector(&[("TMU", ""), ("Kingfisher", "")]);
        let (out, reps) = c.correct_with_report("the tmu and Kingfisher and kingfischer");
        assert_eq!(out, "the TMU and Kingfisher and Kingfisher");
        assert_eq!(
            reps,
            vec![
                Replacement {
                    from: "tmu".into(),
                    to: "TMU".into()
                },
                Replacement {
                    from: "kingfischer".into(),
                    to: "Kingfisher".into()
                },
            ]
        );
    }

    #[test]
    fn prefers_longer_terms() {
        let c = corrector(&[("Kingfisher", ""), ("Kingfisher Cloud", "")]);
        assert_eq!(
            c.correct("on kingfisher cloud today"),
            "on Kingfisher Cloud today"
        );
        assert_eq!(c.correct("on kingfisher today"), "on Kingfisher today");
    }

    #[test]
    fn hostile_inputs_never_panic() {
        let long = "x".repeat(10_000);
        let c = corrector(&[
            ("Zoë", "zo ee"),
            ("🚀Rocket", ""),
            ("İstanbul", ""),
            ("ẞtraße", ""),
            (&long, ""),
            ("", "only hint"),
        ]);
        for text in [
            "",
            "   ",
            "🚀rocket's 🚀🚀 launch",
            "İSTANBUL'S İ̇stanbul's",
            "ẞTRASSE's straße’S",
            "'s ’s ' ’",
            "Zoë's zoë’S ZOË'S",
            &long,
            &format!("{long}'s {long}s"),
            "a\u{0}b \u{200b}zero width",
        ] {
            let _ = c.correct(text);
        }
        assert_eq!(c.correct("ZOË'S cat"), "Zoë'S cat");
    }

    #[test]
    fn handles_unicode_without_panicking() {
        let c = corrector(&[("Zoë", ""), ("Müller", "")]);
        assert_eq!(c.correct("ask zoë and müller"), "ask Zoë and Müller");
        assert_eq!(c.correct("東京 is fine — ok"), "東京 is fine — ok");
    }
}
