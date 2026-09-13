//! The tokio runtime `parley-core` runs on, exposed to GPUI as a global.
//!
//! GPUI drives the UI on its own executor; everything in the core (sqlx,
//! the audio pipeline, transcription, reqwest) expects tokio. Core work is
//! spawned onto this runtime and awaited from GPUI tasks — a tokio
//! `JoinHandle` is an ordinary future, so `cx.spawn(async move |this, cx| {
//! let r = io.spawn(work).await; this.update(cx, ...) })` bridges the two.

use std::future::Future;
use std::sync::Arc;

use gpui_kit::{App, Global};
use tokio::runtime::{Handle, Runtime};
use tokio::task::JoinHandle;

#[derive(Clone)]
pub struct Io {
    runtime: Arc<Runtime>,
}

impl Global for Io {}

impl Io {
    pub fn new() -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("parley-io")
            .build()?;
        Ok(Self {
            runtime: Arc::new(runtime),
        })
    }

    pub fn global(cx: &App) -> Self {
        cx.global::<Io>().clone()
    }

    pub fn handle(&self) -> Handle {
        self.runtime.handle().clone()
    }

    /// Spawn core work on the tokio runtime.
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.runtime.spawn(future)
    }

    /// Run `future` to completion on the runtime, blocking the calling
    /// thread. Only for startup/shutdown, never from a render path.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }
}
