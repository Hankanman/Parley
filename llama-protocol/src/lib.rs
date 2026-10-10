//! Wire protocol between the Tauri app and the `llama-helper` sidecar.
//!
//! Newline-delimited JSON over the child process's stdin/stdout: the app
//! writes one [`Request`] per line, the helper answers with zero or more
//! [`Response::Chunk`] lines followed by exactly one terminal line
//! ([`Response::Response`], [`Response::Pong`], [`Response::Goodbye`], or
//! [`Response::Error`]).
//!
//! Both sides depend on this crate, so the two ends of the pipe can never
//! drift apart silently — serde ignores unknown fields on deserialize, which
//! means a field added to only one side would be dropped without any error.

use serde::{Deserialize, Serialize};

/// App → helper.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Generate {
        prompt: String,
        max_tokens: Option<i32>,
        context_size: Option<u32>,
        model_path: Option<String>,
        // Sampling parameters
        temperature: Option<f32>,
        top_k: Option<i32>,
        top_p: Option<f32>,
        stop_tokens: Option<Vec<String>>,
        /// Optional JSON Schema the output must conform to. The helper
        /// converts it to a llama.cpp GBNF grammar and constrains sampling
        /// with it, so the model can only emit matching JSON.
        /// `#[serde(default)]` keeps requests without the field valid; an
        /// older helper ignores it and samples freely, which the app's
        /// parse-and-fallback path tolerates.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        json_schema: Option<String>,
    },
    Ping,
    Shutdown,
}

/// Helper → app.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Incremental generation output, streamed while a `Generate` request is
    /// being served. Zero or more of these precede the terminal `Response`
    /// line; concatenating their `text` yields a prefix of the final text
    /// (the helper holds back a small tail that might turn out to be a stop
    /// token).
    Chunk { text: String },
    /// Terminal line of a `Generate` exchange. `text` is the complete final
    /// output (authoritative — it supersedes any streamed chunks).
    Response { text: String, error: Option<String> },
    Pong,
    Goodbye,
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_request_round_trips() {
        let request = Request::Generate {
            prompt: "test prompt".to_string(),
            max_tokens: Some(512),
            context_size: Some(2048),
            model_path: Some("/path/to/model.gguf".to_string()),
            temperature: Some(1.0),
            top_k: Some(64),
            top_p: Some(0.95),
            stop_tokens: Some(vec!["<end_of_turn>".to_string()]),
            json_schema: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"generate\""));
        assert!(json.contains("\"prompt\":\"test prompt\""));
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::Generate { prompt, max_tokens, .. } => {
                assert_eq!(prompt, "test prompt");
                assert_eq!(max_tokens, Some(512));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn generate_request_without_json_schema_still_parses() {
        // A request from an app predating the field must keep working.
        let json = r#"{"type":"generate","prompt":"p","max_tokens":null,"context_size":null,"model_path":null,"temperature":null,"top_k":null,"top_p":null,"stop_tokens":null}"#;
        match serde_json::from_str::<Request>(json).unwrap() {
            Request::Generate { json_schema, .. } => assert!(json_schema.is_none()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn generate_request_carries_json_schema() {
        let request = Request::Generate {
            prompt: "p".to_string(),
            max_tokens: None,
            context_size: None,
            model_path: None,
            temperature: None,
            top_k: None,
            top_p: None,
            stop_tokens: None,
            json_schema: Some(r#"{"type":"object"}"#.to_string()),
        };
        let json = serde_json::to_string(&request).unwrap();
        match serde_json::from_str::<Request>(&json).unwrap() {
            Request::Generate { json_schema, .. } => {
                assert_eq!(json_schema.as_deref(), Some(r#"{"type":"object"}"#))
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn response_round_trips() {
        let json = r#"{"type":"response","text":"generated text","error":null}"#;
        match serde_json::from_str::<Response>(json).unwrap() {
            Response::Response { text, error } => {
                assert_eq!(text, "generated text");
                assert!(error.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn chunk_round_trips_with_embedded_newline() {
        let chunk = Response::Chunk {
            text: "line one\nline two".to_string(),
        };
        // Newlines must be escaped so a chunk always stays a single line on
        // the wire.
        let json = serde_json::to_string(&chunk).unwrap();
        assert!(!json.contains('\n'));
        match serde_json::from_str::<Response>(&json).unwrap() {
            Response::Chunk { text } => assert_eq!(text, "line one\nline two"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn error_response_round_trips() {
        let json = r#"{"type":"error","message":"something went wrong"}"#;
        match serde_json::from_str::<Response>(json).unwrap() {
            Response::Error { message } => assert_eq!(message, "something went wrong"),
            _ => panic!("wrong variant"),
        }
    }
}
