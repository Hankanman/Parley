//! UI-agnostic event emission.
//!
//! Core code (audio pipeline, transcription, summaries, downloads) reports
//! progress to the UI through [`EventSink`] instead of taking a Tauri
//! `AppHandle` just to call `emit`. The Tauri shell implements the trait for
//! `AppHandle<R>`, so call sites that still hold an `AppHandle` can pass
//! `&app` wherever a `&dyn EventSink` is expected; a future non-webview shell
//! (GPUI) supplies its own implementation that routes events into its state
//! entities.
//!
//! Event names and payload shapes are the frontend contract — they are the
//! strings the React side `listen()`s for — so changing a sink must never
//! change what is emitted.

use serde::Serialize;
use std::sync::{Arc, Mutex};

/// Destination for named, JSON-serialisable events.
pub trait EventSink: Send + Sync {
    /// Emit `event` with an already-serialised payload.
    fn emit_value(&self, event: &str, payload: serde_json::Value) -> Result<(), String>;
}

/// Shared, cloneable handle to an [`EventSink`] for long-lived tasks.
pub type SharedEventSink = Arc<dyn EventSink>;

/// Typed convenience over [`EventSink::emit_value`]. Named `emit_event` rather
/// than `emit` so it never collides with `tauri::Emitter::emit` when both
/// traits are in scope.
pub trait EventSinkExt {
    fn emit_event<T: Serialize + ?Sized>(&self, event: &str, payload: &T) -> Result<(), String>;
}

impl<S: EventSink + ?Sized> EventSinkExt for S {
    fn emit_event<T: Serialize + ?Sized>(&self, event: &str, payload: &T) -> Result<(), String> {
        let value = serde_json::to_value(payload).map_err(|e| e.to_string())?;
        self.emit_value(event, value)
    }
}

impl<S: EventSink + ?Sized> EventSink for Arc<S> {
    fn emit_value(&self, event: &str, payload: serde_json::Value) -> Result<(), String> {
        (**self).emit_value(event, payload)
    }
}

// --- Sinks for tests and headless use --------------------------------------

/// Discards every event.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl EventSink for NullSink {
    fn emit_value(&self, _event: &str, _payload: serde_json::Value) -> Result<(), String> {
        Ok(())
    }
}

/// Records every event in order; for asserting on emissions in tests.
#[derive(Debug, Default)]
pub struct RecordingSink {
    events: Mutex<Vec<(String, serde_json::Value)>>,
}

impl RecordingSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of every `(event, payload)` emitted so far.
    pub fn events(&self) -> Vec<(String, serde_json::Value)> {
        self.events.lock().unwrap().clone()
    }

    /// Payloads emitted under `event`, in order.
    pub fn payloads(&self, event: &str) -> Vec<serde_json::Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == event)
            .map(|(_, payload)| payload.clone())
            .collect()
    }
}

impl EventSink for RecordingSink {
    fn emit_value(&self, event: &str, payload: serde_json::Value) -> Result<(), String> {
        self.events.lock().unwrap().push((event.to_string(), payload));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Payload {
        n: u32,
    }

    #[test]
    fn recording_sink_captures_typed_payloads_in_order() {
        let sink = RecordingSink::new();
        sink.emit_event("a", &Payload { n: 1 }).unwrap();
        sink.emit_event("b", "text").unwrap();
        sink.emit_event("a", &Payload { n: 2 }).unwrap();

        assert_eq!(sink.events().len(), 3);
        assert_eq!(
            sink.payloads("a"),
            vec![serde_json::json!({"n": 1}), serde_json::json!({"n": 2})]
        );
    }

    #[test]
    fn shared_sink_forwards_through_arc() {
        let sink = Arc::new(RecordingSink::new());
        let shared: SharedEventSink = sink.clone();
        shared.emit_event("x", &42).unwrap();
        assert_eq!(sink.payloads("x"), vec![serde_json::json!(42)]);
    }
}
