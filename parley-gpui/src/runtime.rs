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

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Context, Poll, Wake, Waker};
    use std::time::Duration;

    /// Poll `future` on the current thread outside any tokio worker — how
    /// GPUI's executor polls the futures in `cx.spawn`.
    fn poll_off_runtime<F: Future>(future: F) -> F::Output {
        struct Unpark(std::thread::Thread);
        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                return output;
            }
            std::thread::park();
        }
    }

    #[test]
    fn tokio_timers_work_off_runtime_inside_the_entered_context() {
        let io = Io::new().unwrap();
        let _context = io.handle().enter();
        poll_off_runtime(tokio::time::sleep(Duration::from_millis(10)));
    }

    /// The crash from a release build: a query awaited directly on GPUI's
    /// executor while the pool's only connection was busy. Waiting for a
    /// connection starts a tokio timer, which panicked without a context.
    #[test]
    fn sqlx_waiting_for_a_connection_works_off_runtime_inside_the_entered_context() {
        let io = Io::new().unwrap();
        let pool = io
            .block_on(
                sqlx::sqlite::SqlitePoolOptions::new()
                    .max_connections(1)
                    .connect("sqlite::memory:"),
            )
            .unwrap();
        let held = io.block_on(pool.acquire()).unwrap();

        let handle = io.handle();
        let releaser = std::thread::spawn(move || {
            let _context = handle.enter();
            std::thread::sleep(Duration::from_millis(50));
            drop(held);
        });

        let _context = io.handle().enter();
        let one: i64 = poll_off_runtime(sqlx::query_scalar("SELECT 1").fetch_one(&pool)).unwrap();
        releaser.join().unwrap();
        assert_eq!(one, 1);
    }
}
