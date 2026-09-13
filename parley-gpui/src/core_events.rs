//! Core → UI events.
//!
//! [`GpuiSink`] is this shell's `EventSink`: core code emits named JSON
//! events into an unbounded channel (cheap, callable from any thread,
//! including PipeWire's real-time thread via the pipeline). A GPUI task on
//! the [`CoreEvents`] entity drains the channel and re-emits each one as a
//! GPUI event, so views subscribe with `cx.subscribe(&core_events, ...)` and
//! decode the payloads they care about with [`CoreEvent::decode`]. Event
//! names/payloads are the same contract the Tauri frontend listens to.

use gpui_kit::{Context, EventEmitter};
use parley_core::events::EventSink;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

#[derive(Debug, Clone)]
pub struct CoreEvent {
    pub name: String,
    pub payload: serde_json::Value,
}

impl CoreEvent {
    /// Decode the payload as `T`, logging (not panicking) on a shape mismatch.
    pub fn decode<T: DeserializeOwned>(&self) -> Option<T> {
        match serde_json::from_value(self.payload.clone()) {
            Ok(value) => Some(value),
            Err(e) => {
                log::warn!("Undecodable '{}' payload: {}", self.name, e);
                None
            }
        }
    }
}

#[derive(Clone)]
pub struct GpuiSink {
    tx: UnboundedSender<CoreEvent>,
}

impl EventSink for GpuiSink {
    fn emit_value(&self, event: &str, payload: serde_json::Value) -> Result<(), String> {
        self.tx
            .send(CoreEvent {
                name: event.to_string(),
                payload,
            })
            .map_err(|_| "UI event channel closed".to_string())
    }
}

/// Create the sink and the receiving end the [`CoreEvents`] entity drains.
pub fn channel() -> (GpuiSink, UnboundedReceiver<CoreEvent>) {
    let (tx, rx) = unbounded_channel();
    (GpuiSink { tx }, rx)
}

/// Re-emits every core event as a GPUI event. One per app.
pub struct CoreEvents;

impl EventEmitter<CoreEvent> for CoreEvents {}

impl CoreEvents {
    pub fn new(mut rx: UnboundedReceiver<CoreEvent>, cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            while let Some(event) = rx.recv().await {
                if this.update(cx, |_, cx| cx.emit(event)).is_err() {
                    break;
                }
            }
        })
        .detach();
        Self
    }
}
