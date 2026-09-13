//! Pure, unit-tested logic for the onboarding flow: the 3-step state
//! machine (mirrors `frontend/src/contexts/OnboardingContext.tsx`'s
//! `goNext`/`goPrevious`/`goToStep`) and small formatting/gating helpers the
//! GPUI view builds on.

/// The 3-step onboarding flow (system-recommended models, no separate
/// permissions step on Linux) — same steps as
/// `frontend/src/components/onboarding/OnboardingFlow.tsx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Welcome,
    SetupOverview,
    DownloadProgress,
}

impl Step {
    /// 1-based step number, matching `OnboardingStatus.current_step` as
    /// persisted by the Tauri flow (so a status saved by one shell can be
    /// resumed by the other).
    pub fn number(self) -> u8 {
        match self {
            Step::Welcome => 1,
            Step::SetupOverview => 2,
            Step::DownloadProgress => 3,
        }
    }

    /// Clamp a persisted (or otherwise untrusted) step number into a valid
    /// `Step`, the same clamping `OnboardingContext.tsx`'s `verifyModelStatus`
    /// does (`currentStep > 3` -> download-progress).
    pub fn clamp(n: u8) -> Step {
        match n {
            0 | 1 => Step::Welcome,
            2 => Step::SetupOverview,
            _ => Step::DownloadProgress,
        }
    }
}

/// `goNext` — never advances past the last step.
pub fn next_step(step: Step) -> Step {
    match step {
        Step::Welcome => Step::SetupOverview,
        Step::SetupOverview => Step::DownloadProgress,
        Step::DownloadProgress => Step::DownloadProgress,
    }
}

/// `goPrevious` — never retreats past the first step.
pub fn previous_step(step: Step) -> Step {
    match step {
        Step::Welcome => Step::Welcome,
        Step::SetupOverview => Step::Welcome,
        Step::DownloadProgress => Step::SetupOverview,
    }
}

/// Default local Whisper model downloaded during onboarding — same
/// quantized turbo variant as `ONBOARDING_WHISPER_MODEL` in
/// `OnboardingContext.tsx`.
pub const ONBOARDING_WHISPER_MODEL: &str = "large-v3-turbo-q5_0";

/// Approximate on-disk size shown next to the summary-model download card,
/// matching `DownloadProgressStep.tsx`'s `recommendedModel === "gemma3:4b" ?
/// "~2.5 GB" : "~806 MB"`.
pub fn summary_model_size_label(model: &str) -> &'static str {
    if model == "gemma3:4b" { "~2.5 GB" } else { "~806 MB" }
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
        assert_eq!(next_step(Step::SetupOverview), Step::DownloadProgress);
        assert_eq!(next_step(Step::DownloadProgress), Step::DownloadProgress);
    }

    #[test]
    fn previous_step_retreats_and_stops_at_the_start() {
        assert_eq!(previous_step(Step::DownloadProgress), Step::SetupOverview);
        assert_eq!(previous_step(Step::SetupOverview), Step::Welcome);
        assert_eq!(previous_step(Step::Welcome), Step::Welcome);
    }

    #[test]
    fn step_number_round_trips_through_clamp() {
        for step in [Step::Welcome, Step::SetupOverview, Step::DownloadProgress] {
            assert_eq!(Step::clamp(step.number()), step);
        }
    }

    #[test]
    fn clamp_treats_zero_as_the_first_step() {
        assert_eq!(Step::clamp(0), Step::Welcome);
    }

    #[test]
    fn clamp_treats_anything_past_three_as_the_last_step() {
        assert_eq!(Step::clamp(4), Step::DownloadProgress);
        assert_eq!(Step::clamp(255), Step::DownloadProgress);
    }

    #[test]
    fn summary_model_size_label_matches_react_copy() {
        assert_eq!(summary_model_size_label("gemma3:4b"), "~2.5 GB");
        assert_eq!(summary_model_size_label("gemma3:1b"), "~806 MB");
        assert_eq!(summary_model_size_label("unknown-model"), "~806 MB");
    }

    #[test]
    fn can_continue_requires_whisper_downloaded_and_not_completing() {
        assert!(can_continue(true, false));
        assert!(!can_continue(false, false));
        assert!(!can_continue(true, true));
        assert!(!can_continue(false, true));
    }
}
