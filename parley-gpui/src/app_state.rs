//! Process-wide services the views reach through `cx.global::<AppServices>()`.

use std::sync::{Arc, RwLock};

use gpui_kit::{App, Entity, Global};
use parley_core::audio::recording_service::RecordingContext;
use parley_core::database::manager::DatabaseManager;
use parley_core::events::SharedEventSink;
use parley_core::summary::summary_engine::ModelManagerState;

use crate::core_events::CoreEvents;
use crate::runtime::Io;

/// The database slot. `None` on a first launch until onboarding creates it;
/// filled in later via [`AppServices::set_db`] without a restart — an
/// `Arc<RwLock<..>>` so it can be handed to `main.rs`'s shutdown path (which
/// needs to see whatever onboarding installed) while every other reader goes
/// through `AppServices::pool()` / `recording_context()`.
pub type DbSlot = Arc<RwLock<Option<DatabaseManager>>>;

pub struct AppServices {
    pub io: Io,
    /// Where core code emits UI events (a [`crate::core_events::GpuiSink`]).
    pub sink: SharedEventSink,
    /// Re-emits core events as GPUI events; views subscribe to this.
    pub core_events: Entity<CoreEvents>,
    /// `None` on a first launch until onboarding creates the database.
    pub db: DbSlot,
    /// Shared slot for the built-in-AI (summary) model manager — the same
    /// `ModelManagerState` the Tauri shell manages via `tauri::State`.
    /// Lazily initialized on first use by `service::ensure_manager`.
    pub builtin_manager: ModelManagerState,
}

impl Global for AppServices {}

impl AppServices {
    pub fn global(cx: &App) -> &Self {
        cx.global::<AppServices>()
    }

    pub fn pool(&self) -> Option<sqlx::SqlitePool> {
        self.db.read().unwrap().as_ref().map(|db| db.pool().clone())
    }

    /// True once a database has been opened or created (normal launch, or a
    /// first launch that has finished the onboarding DB-creation step).
    pub fn has_db(&self) -> bool {
        self.db.read().unwrap().is_some()
    }

    /// Install a freshly-created (or opened) database, making it visible to
    /// every subsequent `pool()` / `recording_context()` call without a
    /// restart. Called once, at the end of onboarding's setup step.
    pub fn set_db(&self, db: DatabaseManager) {
        *self.db.write().unwrap() = Some(db);
    }

    /// Clone of the shared slot backing the built-in-AI model manager, for
    /// passing into `summary_engine::service` calls off the main thread.
    pub fn builtin_manager_slot(
        &self,
    ) -> std::sync::Arc<
        tokio::sync::Mutex<Option<std::sync::Arc<parley_core::summary::summary_engine::model_manager::ModelManager>>>,
    > {
        self.builtin_manager.0.clone()
    }

    /// Context for `parley_core::audio::recording_service` calls.
    pub fn recording_context(&self) -> RecordingContext {
        RecordingContext {
            sink: self.sink.clone(),
            pool: self.pool(),
        }
    }
}
