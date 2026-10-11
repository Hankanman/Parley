//! Recording page: device pickers, start/pause/resume/stop controls, live
//! level meters, and the live transcript. See `view.rs` for the GPUI
//! entity and `logic.rs` for the pure, unit-tested logic it's built on
//! (transcript ordering/dedupe, elapsed-time formatting, phase -> button
//! enablement).

mod logic;
mod view;

pub use view::RecordingView;
