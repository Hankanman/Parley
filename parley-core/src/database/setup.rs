use log::{info, warn};

use super::manager::DatabaseManager;
use super::repositories::meeting::MeetingsRepository;

/// Outcome of [`prepare_database_on_startup`] for the Tauri shell to act on:
/// either this is the first launch (nothing to manage yet — the shell
/// notifies the frontend once its window/listeners are ready) or the
/// database is open and ready to be registered as app state.
pub enum StartupOutcome {
    FirstLaunch,
    Initialized(DatabaseManager),
}

/// Tauri-free core of database startup: first-launch detection and, on a
/// normal launch, opening the database and sweeping any meeting row a
/// previous run left `"recording"` (crash marker, issue #57 slice 2) to
/// `"interrupted"` so the recovery dialog can find it via a plain status
/// query. The Tauri shell (`initialize_database_on_startup` in
/// `lib.rs`/`setup_commands.rs`) manages the returned `DatabaseManager` as
/// app state and emits `first-launch-detected` on `StartupOutcome::FirstLaunch`.
pub async fn prepare_database_on_startup() -> Result<StartupOutcome, String> {
    // Check if this is the first launch (no database exists yet)
    let is_first_launch = DatabaseManager::is_first_launch()
        .await
        .map_err(|e| format!("Failed to check first launch status: {}", e))?;

    if is_first_launch {
        info!("First launch detected - will notify window when ready");
        return Ok(StartupOutcome::FirstLaunch);
    }

    // Normal flow - initialize database immediately
    let db_manager = DatabaseManager::new_default()
        .await
        .map_err(|e| format!("Failed to initialize database manager: {}", e))?;

    let pool = db_manager.pool().clone();
    info!("Database initialized successfully");

    // Crash marker (issue #57 slice 2): a meeting row still "recording"
    // at this point predates this very process — the app that created
    // it never reached `stop_recording`'s finalisation (a crash, kill,
    // or forced shutdown). Sweep them to "interrupted" once, here, so
    // the recovery dialog can find them via a plain status query.
    match MeetingsRepository::mark_stale_recording_meetings_interrupted(&pool).await {
        Ok(0) => {}
        Ok(n) => info!(
            "Marked {} meeting(s) left 'recording' by a previous run as 'interrupted'",
            n
        ),
        Err(e) => warn!("Failed to sweep stale 'recording' meetings at startup: {}", e),
    }

    Ok(StartupOutcome::Initialized(db_manager))
}
