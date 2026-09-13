//! Parley desktop app on GPUI. Links `parley-core` directly — no webview,
//! no IPC. Startup and shutdown are shared with the Tauri shell through
//! `parley_core::bootstrap`.

mod app_state;
mod core_events;
mod fonts;
mod notifications;
mod recovery;
mod root;
mod runtime;
mod shell;
mod tray;
mod ui;
mod window;
mod views;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use gpui_kit::*;
use parley_core::bootstrap;
use parley_core::database::setup::StartupOutcome;
use parley_core::events::SharedEventSink;

use app_state::AppServices;
use core_events::CoreEvents;
use runtime::Io;

/// Set once a close/quit request has started finishing the recording, so a
/// second request (mashing close, or close + tray Quit) doesn't start another.
static QUIT_STARTED: AtomicBool = AtomicBool::new(false);

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(
        "info,whisper_rs=warn,zbus=warn,tracing=warn,wgpu_hal=warn,wgpu_core=warn,naga=warn,\
         wgpu_hal::vulkan::instance=error,gpui_component::theme::mono_font=error",
    ))
    .init();

    let io = Io::new().expect("failed to start the tokio runtime");
    let (sink, events_rx) = core_events::channel();
    let sink: SharedEventSink = Arc::new(sink);

    // Same order as the Tauri shell: paths, then the database (blocking, so
    // everything after it sees the pool), then background init with the pool.
    bootstrap::init_paths();
    let db = match io.block_on(bootstrap::prepare_database()) {
        Ok(StartupOutcome::Initialized(db)) => Some(db),
        Ok(StartupOutcome::FirstLaunch) => {
            log::info!("First launch: no database yet — the onboarding flow will create one");
            None
        }
        Err(e) => {
            log::error!("Database startup failed: {}", e);
            None
        }
    };
    let model_manager = Arc::new(tokio::sync::Mutex::new(None));
    {
        let _guard = io.handle().enter();
        bootstrap::spawn_background_init(
            sink.clone(),
            db.as_ref().map(|db| db.pool().clone()),
            model_manager.clone(),
        );
    }
    // Same slot `spawn_background_init` pre-warms above — the Settings
    // page's built-in-AI model manager (`views/settings/summary.rs`) reads
    // and writes through this, exactly like the Tauri shell's
    // `tauri::State<ModelManagerState>`.
    let builtin_manager = parley_core::summary::summary_engine::ModelManagerState(model_manager);
    // `AppServices::set_db` fills this in later, without a restart, once
    // first-launch onboarding creates the database (see `root.rs` /
    // `views/onboarding`). Wrapped in a lock (rather than moved into
    // `AppServices` by value) so the shutdown path below can still see
    // whatever onboarding installed after the run loop exits.
    let db: app_state::DbSlot = Arc::new(RwLock::new(db));

    let app = gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .with_quit_mode(QuitMode::Explicit);

    let db_for_shutdown = db.clone();
    let io_for_shutdown = io.clone();

    // Core futures should go through `io.spawn`, but anything awaited
    // directly on GPUI's executor (this thread) that reaches for tokio —
    // sqlx starts a tokio timer whenever it has to wait for a pooled
    // connection — would panic with "this functionality requires a Tokio
    // context" and take the app down. Entering the runtime here makes that
    // a non-event for the whole run loop: the work itself still happens on
    // the runtime's worker threads.
    let runtime_handle = io.handle();
    let runtime_context = runtime_handle.enter();
    app.run(move |cx| {
        gpui_kit::init(cx);
        zorite_editor::bind_keys(cx);
        // After `gpui_kit::init` (which sets the Theme global), before any
        // text is laid out.
        fonts::apply_system_mono_font(cx);

        let core_events = cx.new(|cx| CoreEvents::new(events_rx, cx));
        cx.set_global(io.clone());
        cx.set_global(AppServices {
            io: io.clone(),
            sink: sink.clone(),
            core_events,
            db,
            builtin_manager,
        });

        cx.spawn(async move |cx| {
            let _ = cx.update(|cx| {
                // Opens the window and registers its theme + close handler;
                // also stores the `MainWindow` handle the tray reaches for.
                if let Err(e) = window::open_main_window(cx) {
                    log::error!("Failed to open the main window: {}", e);
                }

                if let Err(e) = tray::install(cx) {
                    log::warn!(
                        "Tray unavailable (no StatusNotifierItem host?), continuing without one: {}",
                        e
                    );
                }
                notifications::start_meeting_reminder_loop(cx);
            });
        })
        .detach();
    });

    drop(runtime_context);

    // The run loop has exited: release the DB, sidecar and Whisper model.
    // Read the slot fresh here (rather than the `db` captured before
    // `app.run`) so a database created during onboarding still gets cleaned
    // up even though it didn't exist at process startup.
    let db_guard = db_for_shutdown.read().unwrap();
    io_for_shutdown.block_on(bootstrap::shutdown(db_guard.as_ref()));
    drop(db_guard);
    log::info!("Application cleanup complete");

    // Exit immediately, skipping C/C++ static destructors — same reason as
    // the Tauri shell (GPU driver / whisper.cpp teardown can crash on exit).
    unsafe { libc::_exit(0) };
}

/// Finish any active recording (full stop flow, so nothing unflushed is
/// lost), then quit. Shared by window close and the tray's Quit.
pub fn request_quit(cx: &mut App) {
    if QUIT_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let services = AppServices::global(cx);
    let io = services.io.clone();
    let ctx = services.recording_context();
    cx.spawn(async move |cx| {
        let _ = io.spawn(bootstrap::finish_recording_for_exit(ctx)).await;
        cx.update(|cx| {
            tray::shutdown(cx);
            cx.quit();
        });
    })
    .detach();
}
