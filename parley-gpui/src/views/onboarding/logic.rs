//! Pure, unit-tested logic for the onboarding flow: the step state machine
//! (next / previous / go-to-step) and small formatting/gating helpers the
//! GPUI view builds on.

/// The onboarding steps, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Welcome,
    SetupOverview,
    /// Transcription language. Model downloads start when this step opens.
    Language,
    DownloadProgress,
    /// Optional prompt to record a voice-match profile.
    VoiceMatch,
}

impl Step {
    pub const ALL: [Step; 5] = [
        Step::Welcome,
        Step::SetupOverview,
        Step::Language,
        Step::DownloadProgress,
        Step::VoiceMatch,
    ];

    /// 1-based step number, as persisted in `OnboardingStatus.current_step`.
    /// A status saved before the language step existed has 3 for the
    /// download step, which now resumes one step earlier, at the language
    /// choice; the downloads start from there anyway.
    pub fn number(self) -> u8 {
        match self {
            Step::Welcome => 1,
            Step::SetupOverview => 2,
            Step::Language => 3,
            Step::DownloadProgress => 4,
            Step::VoiceMatch => 5,
        }
    }

    /// Clamp a persisted (or otherwise untrusted) step number into a valid
    /// `Step`.
    pub fn clamp(n: u8) -> Step {
        match n {
            0 | 1 => Step::Welcome,
            2 => Step::SetupOverview,
            3 => Step::Language,
            4 => Step::DownloadProgress,
            _ => Step::VoiceMatch,
        }
    }

    /// Whether model downloads should be running on this step: from the
    /// language step on (the database exists by then).
    pub fn downloads_running(self) -> bool {
        self.number() >= Step::Language.number()
    }
}

/// Never advances past the last step.
pub fn next_step(step: Step) -> Step {
    let i = Step::ALL.iter().position(|s| *s == step).unwrap_or(0);
    Step::ALL[(i + 1).min(Step::ALL.len() - 1)]
}

/// Never retreats past the first step.
pub fn previous_step(step: Step) -> Step {
    let i = Step::ALL.iter().position(|s| *s == step).unwrap_or(0);
    Step::ALL[i.saturating_sub(1)]
}

/// Default local Whisper model downloaded during onboarding — same
/// quantized turbo variant as `ONBOARDING_WHISPER_MODEL` in
/// `OnboardingContext.tsx`.
pub const ONBOARDING_WHISPER_MODEL: &str = "large-v3-turbo-q5_0";

/// Approximate download size shown next to the summary-model download card
/// (the GGUFs in `summary_engine::models`: 1019 MB and 2374 MB).
pub fn summary_model_size_label(model: &str) -> &'static str {
    if model == "gemma3:4b" { "~2.3 GB" } else { "~1.0 GB" }
}

/// Whether the "Continue" button on the download step should be enabled:
/// the Whisper model must be downloaded (the summary model is allowed to
/// keep downloading in the background, same gating as the React step's
/// `disabled={!parakeetDownloaded || isCompleting}`).
pub fn can_continue(whisper_downloaded: bool, completing: bool) -> bool {
    whisper_downloaded && !completing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_step_advances_and_stops_at_the_end() {
        assert_eq!(next_step(Step::Welcome), Step::SetupOverview);
        assert_eq!(next_step(Step::SetupOverview), Step::Language);
        assert_eq!(next_step(Step::Language), Step::DownloadProgress);
        assert_eq!(next_step(Step::DownloadProgress), Step::VoiceMatch);
        assert_eq!(next_step(Step::VoiceMatch), Step::VoiceMatch);
    }

    #[test]
    fn previous_step_retreats_and_stops_at_the_start() {
        assert_eq!(previous_step(Step::VoiceMatch), Step::DownloadProgress);
        assert_eq!(previous_step(Step::Language), Step::SetupOverview);
        assert_eq!(previous_step(Step::Welcome), Step::Welcome);
    }

    #[test]
    fn step_number_round_trips_through_clamp() {
        for step in Step::ALL {
            assert_eq!(Step::clamp(step.number()), step);
        }
    }

    #[test]
    fn clamp_treats_zero_as_the_first_step_and_overflow_as_the_last() {
        assert_eq!(Step::clamp(0), Step::Welcome);
        assert_eq!(Step::clamp(255), Step::VoiceMatch);
    }

    #[test]
    fn downloads_run_from_the_language_step_on() {
        assert!(!Step::SetupOverview.downloads_running());
        assert!(Step::Language.downloads_running());
        assert!(Step::VoiceMatch.downloads_running());
    }

    #[test]
    fn summary_model_size_label_matches_react_copy() {
        assert_eq!(summary_model_size_label("gemma3:4b"), "~2.3 GB");
        assert_eq!(summary_model_size_label("gemma3:1b"), "~1.0 GB");
        assert_eq!(summary_model_size_label("unknown-model"), "~1.0 GB");
    }

    #[test]
    fn can_continue_requires_whisper_downloaded_and_not_completing() {
        assert!(can_continue(true, false));
        assert!(!can_continue(false, false));
        assert!(!can_continue(true, true));
        assert!(!can_continue(false, true));
    }
}
