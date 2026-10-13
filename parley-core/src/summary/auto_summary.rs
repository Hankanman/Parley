//! Automatic summary generation for a just-finished recording.
//!
//! Gated by the `isAutoSummary` flag in the `ui_config` setting (written by
//! the Settings > Summary page). Triggered from
//! [`crate::audio::retranscription::spawn_auto_refine`] once the post-meeting
//! refine pass has finished (or been skipped), so the summary reads the
//! refined transcript when there is one. Uses the same model config, default
//! template and `process_transcript_background` entry point as the meeting
//! view's "Generate" button, so the same `summary-stream` events drive an
//! open meeting page.

use crate::database::models::MeetingTranscript;
use crate::database::repositories::meeting::MeetingsRepository;
use crate::database::repositories::setting::{
    SettingsRepository, KEY_DEFAULT_SUMMARY_TEMPLATE, KEY_UI_CONFIG,
};
use crate::events::SharedEventSink;
use crate::summary::service::SummaryService;
use anyhow::{anyhow, Result};
use sqlx::SqlitePool;
use tracing::{info, warn};

const FALLBACK_TEMPLATE: &str = "standard_meeting";

/// Fire-and-forget wrapper: logs and carries on if anything goes wrong.
pub async fn maybe_auto_summarize(
    sink: SharedEventSink,
    pool: Option<SqlitePool>,
    meeting_id: String,
) {
    let Some(pool) = pool else {
        return;
    };
    match run(sink, pool, &meeting_id).await {
        Ok(true) => info!("Auto-summary finished for meeting {}", meeting_id),
        Ok(false) => {}
        Err(e) => warn!("Auto-summary skipped for meeting {}: {}", meeting_id, e),
    }
}

/// Returns `Ok(false)` when auto-summary is switched off.
async fn run(sink: SharedEventSink, pool: SqlitePool, meeting_id: &str) -> Result<bool> {
    let ui_config = SettingsRepository::get_setting_json(&pool, KEY_UI_CONFIG).await?;
    if !is_auto_summary_enabled(ui_config.as_deref()) {
        return Ok(false);
    }

    let details = MeetingsRepository::get_meeting(&pool, meeting_id)
        .await?
        .ok_or_else(|| anyhow!("meeting not found"))?;
    let text = build_transcript_text(&details.transcripts);
    if text.trim().is_empty() {
        return Err(anyhow!("transcript is empty"));
    }

    let config = SettingsRepository::get_model_config(&pool)
        .await?
        .ok_or_else(|| anyhow!("no summary model configured"))?;

    // Stored with `set_setting::<String>`, i.e. as a JSON string — read it
    // back typed so the quotes don't end up in the template id.
    let configured_template =
        SettingsRepository::get_setting::<String>(&pool, KEY_DEFAULT_SUMMARY_TEMPLATE).await?;
    let template_id = resolve_template(configured_template.as_deref());

    info!(
        "Auto-summary starting for meeting {} ({} / {})",
        meeting_id, config.provider, config.model
    );
    SummaryService::process_transcript_background(
        sink,
        pool,
        meeting_id.to_string(),
        text,
        config.provider,
        config.model,
        String::new(),
        template_id,
    )
    .await;
    Ok(true)
}

fn is_auto_summary_enabled(ui_config: Option<&str>) -> bool {
    ui_config
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|v| v.get("isAutoSummary").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

/// The configured default template if it exists, else `standard_meeting`
/// (same resolution the meeting view applies).
fn resolve_template(configured: Option<&str>) -> String {
    let available = crate::summary::templates::list_template_ids();
    if let Some(id) = configured {
        if available.iter().any(|a| a == id) {
            return id.to_string();
        }
    }
    if available.iter().any(|a| a == FALLBACK_TEMPLATE) {
        return FALLBACK_TEMPLATE.to_string();
    }
    available
        .into_iter()
        .next()
        .unwrap_or_else(|| FALLBACK_TEMPLATE.to_string())
}

/// One `<timestamp> <text>` line per segment — identical to the meeting
/// view's `format::build_transcript_text`.
fn build_transcript_text(transcripts: &[MeetingTranscript]) -> String {
    transcripts
        .iter()
        .map(|t| {
            let stamp = match t.audio_start_time {
                Some(s) if s.is_finite() && s >= 0.0 => {
                    let total = s.floor() as u64;
                    format!("[{:02}:{:02}]", total / 60, total % 60)
                }
                _ => t.timestamp.clone(),
            };
            format!("{} {}", stamp, t.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_summary_flag_parsing() {
        assert!(is_auto_summary_enabled(Some(r#"{"isAutoSummary":true}"#)));
        assert!(!is_auto_summary_enabled(Some(r#"{"isAutoSummary":false}"#)));
        assert!(!is_auto_summary_enabled(Some(r#"{"other":1}"#)));
        assert!(!is_auto_summary_enabled(Some("not json")));
        assert!(!is_auto_summary_enabled(None));
    }
}
