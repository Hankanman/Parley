use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct MeetingModel {
    pub id: String,
    pub title: String,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
    pub folder_path: Option<String>,
    /// Lifecycle status: "recording" | "completed" | "interrupted". Rows
    /// created before this column existed default to "completed" (see the
    /// migration). Owned entirely by the Rust recording lifecycle (issue
    /// #57 slice 2) — the frontend never writes it.
    #[serde(default = "default_meeting_status")]
    pub status: String,
    pub completed_at: Option<DateTimeUtc>,
    pub duration_seconds: Option<f64>,
    pub audio_path: Option<String>,
}

fn default_meeting_status() -> String {
    "completed".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type)]
#[sqlx(transparent)]
pub struct DateTimeUtc(pub DateTime<Utc>);

impl From<NaiveDateTime> for DateTimeUtc {
    fn from(naive: NaiveDateTime) -> Self {
        DateTimeUtc(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
    }
}

// Renamed from TranscriptSegment to Transcript to match the table name
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Transcript {
    pub id: String,
    pub meeting_id: String,
    pub transcript: String,
    pub timestamp: String,
    // Recording-relative timestamps for audio-transcript synchronization
    pub audio_start_time: Option<f64>,
    pub audio_end_time: Option<f64>,
    pub duration: Option<f64>,
    /// Human-readable speaker label ("Me", "Speaker 1", or stored profile name).
    pub speaker: Option<String>,
    /// Foreign key to `voice_profiles.id` when this transcript matched a
    /// stored profile; null otherwise.
    pub voice_profile_id: Option<String>,
    /// Audio-stream this segment came from: "mic" or "system". Null for older
    /// rows and imports; used to play the matching channel of the stereo
    /// recording (mic = left, system = right).
    pub source: Option<String>,
    /// Whisper confidence score (0.0-1.0) from the live-recording path. Null
    /// for older rows, imports, and retranscription.
    pub confidence: Option<f32>,
}

/// Stored speaker voice profile used to recognize returning speakers across meetings.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct VoiceProfile {
    pub id: String,
    pub name: String,
    /// Optional contact email for the speaker. Populated when the user links
    /// a profile to a known contact; nullable since not every named speaker
    /// will have an email available.
    pub email: Option<String>,
    /// Packed little-endian f32 speaker-embedding centroid.
    pub embedding: Vec<u8>,
    pub embedding_dim: i64,
    pub sample_count: i64,
    /// True for the single profile holding the local user's own enrolled
    /// voice. At most one row has this set (enforced by a partial unique
    /// index); it is what makes the user's mic speech render as "Me" instead
    /// of "Speaker N" once a diarizer is loaded.
    pub is_self: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// A single tracked action item belonging to a meeting.
///
/// Rows are created either by the post-summary extraction pass
/// (`source = "summary"`), by the user in the UI (`source = "manual"`), or by
/// an agent over the MCP server (`source = "agent"`). Only the `summary` rows
/// are owned by the extractor — a re-extraction replaces them and leaves the
/// other two provenances untouched (see `ActionItemsRepository::replace_summary_items`).
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ActionItem {
    pub id: String,
    pub meeting_id: String,
    pub text: String,
    /// Named owner when one could be determined ("Seb to send the deck").
    pub assignee: Option<String>,
    /// Free-text due hint exactly as spoken ("by Friday"). Deliberately not
    /// parsed into a date — the phrasing is what the user recognizes.
    pub due_hint: Option<String>,
    /// `"open"` | `"done"`.
    pub status: String,
    /// `"summary"` | `"manual"` | `"agent"`.
    pub source: String,
    /// Opaque id in an external tracker once pushed there; NULL until then.
    pub external_ref: Option<String>,
    /// Recording-relative seconds of the transcript segment this item was
    /// extracted from, when the transcript-sourced extractor could ground it.
    /// NULL for legacy/summary-sourced items and manual/agent entries.
    pub source_start_secs: Option<f64>,
    pub source_end_secs: Option<f64>,
    /// The transcript sentence that triggered the item (for verification).
    pub source_quote: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}

/// A free-text note attached to a meeting, written by the user or an agent.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct MeetingNote {
    pub id: String,
    pub meeting_id: String,
    pub body: String,
    /// `"manual"` | `"agent"`.
    pub source: String,
    pub created_at: String,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct SummaryProcess {
    pub meeting_id: String,
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub error: Option<String>,
    pub result: Option<String>, // JSON
    pub start_time: Option<chrono::DateTime<chrono::Utc>>,
    pub end_time: Option<chrono::DateTime<chrono::Utc>>,
    pub chunk_count: i64,
    pub processing_time: f64,
    pub metadata: Option<String>,      // JSON
    pub result_backup: Option<String>, // Backup of result before regeneration
    pub result_backup_timestamp: Option<chrono::DateTime<chrono::Utc>>, // When backup was created
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Setting {
    pub id: String,
    pub provider: String,
    pub model: String,
    #[sqlx(rename = "whisperModel")]
    #[serde(rename = "whisperModel")]
    pub whisper_model: String,
    #[sqlx(rename = "groqApiKey")]
    #[serde(rename = "groqApiKey")]
    pub groq_api_key: Option<String>,
    #[sqlx(rename = "openaiApiKey")]
    #[serde(rename = "openaiApiKey")]
    pub openai_api_key: Option<String>,
    #[sqlx(rename = "anthropicApiKey")]
    #[serde(rename = "anthropicApiKey")]
    pub anthropic_api_key: Option<String>,
    #[sqlx(rename = "ollamaApiKey")]
    #[serde(rename = "ollamaApiKey")]
    pub ollama_api_key: Option<String>,
    #[sqlx(rename = "openRouterApiKey")]
    #[serde(rename = "openRouterApiKey")]
    pub open_router_api_key: Option<String>,
    #[sqlx(rename = "ollamaEndpoint")]
    #[serde(rename = "ollamaEndpoint")]
    pub ollama_endpoint: Option<String>,
    /// Custom OpenAI-compatible endpoint configuration stored as JSON
    #[sqlx(rename = "customOpenAIConfig")]
    #[serde(rename = "customOpenAIConfig")]
    pub custom_openai_config: Option<String>,
}

impl Setting {
    /// Parse the custom OpenAI config from JSON string
    pub fn get_custom_openai_config(&self) -> Option<crate::summary::CustomOpenAIConfig> {
        self.custom_openai_config
            .as_ref()
            .and_then(|json| serde_json::from_str(json).ok())
    }
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct TranscriptSetting {
    pub id: String,
    pub provider: String,
    pub model: String,
    #[sqlx(rename = "whisperApiKey")]
    #[serde(rename = "whisperApiKey")]
    pub whisper_api_key: Option<String>,
    #[sqlx(rename = "deepgramApiKey")]
    #[serde(rename = "deepgramApiKey")]
    pub deepgram_api_key: Option<String>,
    #[sqlx(rename = "elevenLabsApiKey")]
    #[serde(rename = "elevenLabsApiKey")]
    pub eleven_labs_api_key: Option<String>,
    #[sqlx(rename = "groqApiKey")]
    #[serde(rename = "groqApiKey")]
    pub groq_api_key: Option<String>,
    #[sqlx(rename = "openaiApiKey")]
    #[serde(rename = "openaiApiKey")]
    pub openai_api_key: Option<String>,
}

// --- Command-facing DTOs built by the repositories ------------------------
// Serialised straight to the frontend, so field names and serde attributes
// are part of the UI contract.

#[derive(Debug, Serialize, Deserialize)]
pub struct TranscriptSearchResult {
    pub id: String,
    pub title: String,
    #[serde(rename = "matchContext")]
    pub match_context: String,
    pub timestamp: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MeetingDetails {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub transcripts: Vec<MeetingTranscript>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingTranscript {
    pub id: String,
    pub text: String,
    pub timestamp: String,
    // Recording-relative timestamps for audio-transcript synchronization
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_start_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_end_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_profile_id: Option<String>,
    /// Audio stream ("mic" | "system") this segment came from, for source-aware
    /// per-segment playback. Null for older rows / imports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Whisper confidence score (0.0-1.0), when persisted from the
    /// live-recording path. Null for older rows, imports, and
    /// retranscription — the frontend's `ConfidenceIndicator` renders
    /// nothing in that case, and the GPUI mirror does the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}
