//! In-process delivery of finished transcript segments.
//!
//! The transcription worker publishes every accepted [`TranscriptUpdate`]
//! here in addition to emitting it to the UI. Persistence (the SQLite
//! transcript writer and the `RecordingSaver` copy) subscribes here rather
//! than listening to its own `transcript-update` UI event, so the data path
//! never goes through the UI event bus: delivery is synchronous, ordered, and
//! complete as soon as the worker has drained — no grace period needed for an
//! event loop to catch up.
//!
//! There is at most one subscriber — the active recording session.

use super::transcription::TranscriptUpdate;
use std::sync::{Arc, RwLock};

type Subscriber = Arc<dyn Fn(&TranscriptUpdate) + Send + Sync>;

static SUBSCRIBER: RwLock<Option<Subscriber>> = RwLock::new(None);

/// Install `f` as the subscriber. Returns `true` if it replaced a stale one
/// left behind by a session that never reached [`unsubscribe`] (error-path
/// stop, crash recovery) — replacing it is what keeps segments from being
/// persisted twice.
pub fn subscribe(f: impl Fn(&TranscriptUpdate) + Send + Sync + 'static) -> bool {
    SUBSCRIBER.write().unwrap().replace(Arc::new(f)).is_some()
}

/// Remove the subscriber. Returns `true` if one was installed.
pub fn unsubscribe() -> bool {
    SUBSCRIBER.write().unwrap().take().is_some()
}

/// Deliver `update` to the subscriber, if any, on the calling thread.
pub fn publish(update: &TranscriptUpdate) {
    // Clone the Arc out so the subscriber runs without holding the lock.
    let subscriber = SUBSCRIBER.read().unwrap().clone();
    if let Some(subscriber) = subscriber {
        subscriber(update);
    }
}
