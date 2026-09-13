//! Recording page: device pickers, start/pause/resume/stop controls, live
//! level meters, and the live transcript. See `view.rs` for the GPUI
//! entity and `logic.rs` for the pure, unit-tested logic it's built on
//! (transcript ordering/dedupe, elapsed-time formatting, phase -> button
//! enablement).
//!
//! Mirrors the React recording home: `frontend/src/app/page.tsx`,
//! `frontend/src/app/_components/recording-page/*`,
//! `frontend/src/contexts/{RecordingStateContext,TranscriptContext}.tsx`,
//! `frontend/src/hooks/useRecordingStart.ts`/`useRecordingStop.ts`.

mod logic;
mod view;

pub use view::RecordingView;
