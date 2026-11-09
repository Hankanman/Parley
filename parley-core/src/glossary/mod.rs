//! Custom vocabulary / glossary for transcription (issue #83).
//!
//! A user-managed list of terms — names, products, acronyms — each with an
//! optional "sounds like" hint, stored as one JSON blob in `app_settings`
//! under [`KEY_TRANSCRIPTION_GLOSSARY`].
//!
//! It's used in two ways, both on the **batch** paths only (post-recording
//! auto-refine, manual retranscribe, and audio import — everything that
//! goes through `audio::common::run_batch_transcription`):
//!
//! 1. **Whisper initial prompt** ([`prompt`]): the terms are spelled out in
//!    a short natural sentence ("Glossary: Kingfisher, SamPro, TMU.")
//!    placed ahead of the rolling context tail, so whisper's decoder is
//!    biased toward those spellings. Truncated by priority to fit whisper's
//!    224-token prompt window.
//! 2. **Spelling correction** ([`correction`], opt-in): a deterministic,
//!    conservative fuzzy-replace of near-misses of glossary terms in the
//!    finished segment text.
//!
//! The live transcription worker deliberately passes no prompt at all (on
//! short live VAD chunks whisper.cpp readily regurgitates its prompt — see
//! the note in `audio/transcription/worker.rs`), so the glossary doesn't
//! touch the live path.
//!
//! **Whisper only.** GigaAM and Parakeet do not consume the glossary prompt
//! or correction pass. A future hotword integration should read the same
//! [`Glossary`].

pub mod correction;
pub mod prompt;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::database::repositories::setting::SettingsRepository;

/// `app_settings` key holding the serialized [`Glossary`].
pub const KEY_TRANSCRIPTION_GLOSSARY: &str = "transcription_glossary";

/// One glossary entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct GlossaryTerm {
    /// Stable id so UI rows keep their identity across edits/removals.
    pub id: String,
    /// The canonical spelling, exactly as it should appear in transcripts.
    pub term: String,
    /// Optional comma-separated phonetic spellings of how the term tends
    /// to be misheard ("cooper netties" for "Kubernetes"). Only used by the
    /// correction pass — whisper's prompt just gets the canonical term.
    pub sounds_like: String,
}

impl Default for GlossaryTerm {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            term: String::new(),
            sounds_like: String::new(),
        }
    }
}

impl GlossaryTerm {
    pub fn new(term: String, sounds_like: String) -> Self {
        Self {
            term,
            sounds_like,
            ..Default::default()
        }
    }

    /// The "sounds like" hints, split on `,`/`;`, trimmed, blanks dropped.
    pub fn aliases(&self) -> Vec<String> {
        self.sounds_like
            .split([',', ';'])
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|s| !s.is_empty())
            .collect()
    }
}

/// The whole glossary setting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct Glossary {
    pub terms: Vec<GlossaryTerm>,
    /// Also prompt whisper with stored voice-profile names and, for a
    /// meeting linked to a calendar event, its attendees/organizer. Off by
    /// default. Names only feed the prompt, never the correction pass
    /// (fuzzy-correcting toward every attendee name is too aggressive).
    pub include_known_names: bool,
    /// Run the deterministic spelling-correction pass. Off by default.
    pub correct_spellings: bool,
}

impl Glossary {
    /// Manual terms in priority (list) order, trimmed, blanks dropped.
    pub fn term_strings(&self) -> Vec<String> {
        self.terms
            .iter()
            .map(|t| t.term.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect()
    }
}

/// Load the glossary; missing or unreadable settings yield an empty one.
pub async fn load(pool: &SqlitePool) -> Glossary {
    match SettingsRepository::get_setting::<Glossary>(pool, KEY_TRANSCRIPTION_GLOSSARY).await {
        Ok(Some(g)) => g,
        Ok(None) => Glossary::default(),
        Err(e) => {
            log::warn!("glossary: failed to load, using empty glossary: {}", e);
            Glossary::default()
        }
    }
}

/// Persist the glossary.
pub async fn save(pool: &SqlitePool, glossary: &Glossary) -> Result<(), sqlx::Error> {
    SettingsRepository::set_setting(pool, KEY_TRANSCRIPTION_GLOSSARY, glossary).await
}

/// Everything a batch transcription run needs from the glossary, resolved
/// once up front.
#[derive(Debug, Clone, Default)]
pub struct PreparedGlossary {
    /// The glossary sentence for whisper's initial prompt, if any terms fit.
    pub prompt: Option<String>,
    /// The spelling corrector, if enabled and there are terms.
    pub corrector: Option<correction::Corrector>,
}

impl PreparedGlossary {
    /// Build from an already-loaded glossary plus extra (lower-priority)
    /// names. Pure, so it's unit-testable without a database.
    pub fn build(glossary: &Glossary, extra_names: &[String]) -> Self {
        let mut terms = glossary.term_strings();
        if glossary.include_known_names {
            terms.extend(extra_names.iter().cloned());
        }
        let built = prompt::build_glossary_prompt(&terms, prompt::GLOSSARY_TOKEN_BUDGET);
        if let Some(b) = &built {
            if !b.dropped.is_empty() {
                // Counts only — terms are often client/person names.
                log::info!(
                    "glossary: {} term(s) left out of the whisper prompt (budget full)",
                    b.dropped.len()
                );
            }
        }
        let corrector = if glossary.correct_spellings {
            correction::Corrector::new(&glossary.terms)
        } else {
            None
        };
        Self {
            prompt: built.map(|b| b.text),
            corrector,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.prompt.is_none() && self.corrector.is_none()
    }

    /// The initial prompt for one segment: glossary first, then the
    /// rolling context tail, within whisper's budget.
    pub fn compose_prompt(&self, context_tail: &str) -> Option<String> {
        prompt::compose(self.prompt.as_deref(), context_tail)
    }

    /// Post-process one segment's text: drop any regurgitated glossary
    /// sentence, then (if enabled) correct near-miss spellings.
    pub fn clean_output(&self, text: &str) -> String {
        let text = if self.prompt.is_some() {
            prompt::strip_prompt_echo(text)
        } else {
            text.to_string()
        };
        match &self.corrector {
            Some(c) => {
                let (out, reps) = c.correct_with_report(&text);
                if !reps.is_empty() {
                    // Count only — never log terms or transcript text.
                    log::debug!("glossary: corrected {} span(s) in segment", reps.len());
                }
                out
            }
            None => text,
        }
    }
}

/// Names to add to the prompt when `include_known_names` is on: stored
/// voice-profile names, then (for a meeting linked to a calendar event) the
/// organizer and attendees. Email-only attendees are skipped — an address
/// isn't something people say.
async fn known_names(pool: &SqlitePool, meeting_id: Option<&str>) -> Vec<String> {
    let mut names = Vec::new();

    match crate::database::repositories::voice_profile::VoiceProfilesRepository::list_all(pool)
        .await
    {
        Ok(profiles) => names.extend(profiles.into_iter().map(|p| p.name)),
        Err(e) => log::warn!("glossary: failed to list voice profiles: {}", e),
    }

    if let Some(meeting_id) = meeting_id {
        match crate::calendar::repository::CalendarRepository::get_event_for_meeting(
            pool, meeting_id,
        )
        .await
        {
            Ok(Some(event)) => {
                names.extend(event.organizer_name.clone());
                names.extend(event.attendees.iter().filter_map(|a| a.name.clone()));
            }
            Ok(None) => {}
            Err(e) => log::warn!("glossary: failed to load calendar event: {}", e),
        }
    }

    names
        .into_iter()
        .map(|n| n.trim().trim_matches('"').trim().to_string())
        .filter(|n| !n.is_empty() && !n.contains('@') && !is_placeholder_speaker_name(n))
        .collect()
}

/// Auto-generated diarization labels ("Speaker 3") aren't real names.
fn is_placeholder_speaker_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower
        .strip_prefix("speaker")
        .map(|rest| rest.trim().chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(false)
}

/// Load and resolve the glossary for a batch run. `meeting_id` is the
/// meeting being (re)transcribed, used to find calendar attendees; pass
/// `None` for imports (a brand-new meeting has no calendar link).
/// Never fails — any problem just yields an empty glossary.
pub async fn prepare(pool: Option<&SqlitePool>, meeting_id: Option<&str>) -> PreparedGlossary {
    let Some(pool) = pool else {
        return PreparedGlossary::default();
    };
    let glossary = load(pool).await;
    if glossary.terms.is_empty() && !glossary.include_known_names {
        return PreparedGlossary::default();
    }
    let extra = if glossary.include_known_names {
        known_names(pool, meeting_id).await
    } else {
        Vec::new()
    };
    let prepared = PreparedGlossary::build(&glossary, &extra);
    if let Some(p) = &prepared.prompt {
        log::info!(
            "glossary: whisper prompt prefix ready (~{} est. tokens, correction {})",
            prompt::estimate_tokens(p),
            if prepared.corrector.is_some() {
                "on"
            } else {
                "off"
            }
        );
    }
    prepared
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glossary(terms: &[&str], names: bool, correct: bool) -> Glossary {
        Glossary {
            terms: terms
                .iter()
                .map(|t| GlossaryTerm::new(t.to_string(), String::new()))
                .collect(),
            include_known_names: names,
            correct_spellings: correct,
        }
    }

    #[test]
    fn serde_roundtrip_and_defaults() {
        let g: Glossary = serde_json::from_str("{}").unwrap();
        assert_eq!(g, Glossary::default());
        assert!(!g.include_known_names && !g.correct_spellings);

        // Terms missing an id get a fresh one.
        let g: Glossary = serde_json::from_str(r#"{"terms":[{"term":"TMU"}]}"#).unwrap();
        assert_eq!(g.terms[0].term, "TMU");
        assert!(!g.terms[0].id.is_empty());

        let json = serde_json::to_string(&g).unwrap();
        assert_eq!(serde_json::from_str::<Glossary>(&json).unwrap(), g);
    }

    #[test]
    fn aliases_split_and_trim() {
        let t = GlossaryTerm::new(
            "Kubernetes".into(),
            " cooper  netties ; cube natives,, ".into(),
        );
        assert_eq!(t.aliases(), vec!["cooper netties", "cube natives"]);
    }

    #[test]
    fn names_only_added_when_enabled_and_after_manual_terms() {
        let names = vec!["Siobhan Murphy".to_string()];
        let off = PreparedGlossary::build(&glossary(&["TMU"], false, false), &names);
        assert_eq!(off.prompt.as_deref(), Some("Glossary: TMU."));
        assert!(off.corrector.is_none());

        let on = PreparedGlossary::build(&glossary(&["TMU"], true, false), &names);
        assert_eq!(on.prompt.as_deref(), Some("Glossary: TMU, Siobhan Murphy."));
    }

    #[test]
    fn empty_glossary_is_inert() {
        let p = PreparedGlossary::build(&Glossary::default(), &[]);
        assert!(p.is_empty());
        assert_eq!(p.compose_prompt(""), None);
        assert_eq!(p.compose_prompt("prior text"), Some("prior text".into()));
        assert_eq!(p.clean_output("Glossary: x. hi"), "Glossary: x. hi");
    }

    #[test]
    fn clean_output_strips_echo_and_corrects() {
        let p = PreparedGlossary::build(&glossary(&["Kingfisher", "TMU"], false, true), &[]);
        assert_eq!(
            p.clean_output("Glossary: Kingfisher, TMU. the tmu runs kingfischer"),
            "the TMU runs Kingfisher"
        );
        let no_fix = PreparedGlossary::build(&glossary(&["TMU"], false, false), &[]);
        assert_eq!(no_fix.clean_output("the tmu"), "the tmu");
    }

    #[test]
    fn placeholder_speaker_names_are_filtered() {
        assert!(is_placeholder_speaker_name("Speaker 3"));
        assert!(is_placeholder_speaker_name("speaker12"));
        assert!(!is_placeholder_speaker_name("Speakerman"));
        assert!(!is_placeholder_speaker_name("Alice"));
    }
}
