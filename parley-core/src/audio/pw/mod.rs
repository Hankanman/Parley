//! Native PipeWire capture layer (Linux).
//!
//! This replaces the previous cpal-ALSA + pactl + `PIPEWIRE_NODE` env-var
//! stack. Everything talks to PipeWire directly:
//!
//! - **Enumeration** reads Audio/Source and Audio/Sink nodes from the
//!   registry. Devices are identified by their stable `node.name` and
//!   displayed by `node.description` — no string-suffix conventions.
//! - **Capture** opens a PipeWire capture stream with `target.object`
//!   set to the chosen node. For sinks, `stream.capture.sink` makes
//!   PipeWire deliver the sink's monitor — no `.monitor` pseudo-sources,
//!   no env vars, no default-device indirection.
//! - **Format** is negotiated to interleaved f32 / 48 kHz / stereo; the
//!   PipeWire graph performs any resampling/channel mixing, so consumers
//!   always receive the same shape.
//!
//! Each stream runs its own PipeWire main loop on a dedicated thread
//! (PipeWire objects are not `Send`); samples are handed to a caller
//! provided callback from the loop thread.

use anyhow::{anyhow, Context as _, Result};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use pipewire as pw;
use pw::spa;

/// Sample shape every capture stream delivers, regardless of device.
pub const CAPTURE_RATE: u32 = 48_000;
pub const CAPTURE_CHANNELS: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PwDeviceKind {
    /// A real capture node (Audio/Source) — microphones, capture cards.
    Microphone,
    /// A playback node (Audio/Sink) captured via its monitor ports.
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PwDevice {
    /// Stable PipeWire `node.name` (e.g.
    /// `alsa_input.usb-RODE_Microphones_RODE_NT-USB-00.pro-input-0`).
    /// This is what selection, preferences, and capture use.
    pub id: String,
    /// Human-readable `node.description` for the picker.
    pub label: String,
    pub kind: PwDeviceKind,
}

/// Enumerate audio devices from the PipeWire registry.
///
/// Runs a short-lived main loop on the calling thread and returns once
/// the registry roundtrip completes (or errors after `timeout`).
pub fn enumerate_devices() -> Result<Vec<PwDevice>> {
    enumerate_devices_with_timeout(Duration::from_secs(3))
}

fn enumerate_devices_with_timeout(timeout: Duration) -> Result<Vec<PwDevice>> {
    let (tx, rx) = std_mpsc::channel::<Result<Vec<PwDevice>>>();

    // PipeWire objects are !Send; run the whole roundtrip on one thread.
    let worker = std::thread::Builder::new()
        .name("pw-enumerate".into())
        .spawn(move || {
            let _ = tx.send(enumerate_on_current_thread());
        })
        .context("failed to spawn pw-enumerate thread")?;

    match rx.recv_timeout(timeout) {
        Ok(result) => {
            let _ = worker.join();
            result
        }
        Err(_) => {
            // The worker may be blocked indefinitely inside the PipeWire
            // roundtrip (e.g. a stalled daemon). Don't join it — detach
            // and let it die with the process instead of hanging us.
            warn!("pw: device enumeration timed out; detaching worker thread");
            drop(worker);
            Err(anyhow!("PipeWire device enumeration timed out"))
        }
    }
}

fn enumerate_on_current_thread() -> Result<Vec<PwDevice>> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry()?;

    let devices: Rc<RefCell<Vec<PwDevice>>> = Rc::new(RefCell::new(Vec::new()));
    let devices_cb = devices.clone();

    let _reg_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != pw::types::ObjectType::Node {
                return;
            }
            let Some(props) = global.props else { return };
            let Some(class) = props.get("media.class") else {
                return;
            };
            let kind = match class {
                "Audio/Source" => PwDeviceKind::Microphone,
                "Audio/Sink" => PwDeviceKind::System,
                _ => return,
            };
            let Some(name) = props.get("node.name") else {
                return;
            };
            let label = props
                .get("node.description")
                .or_else(|| props.get("node.nick"))
                .unwrap_or(name)
                .to_string();
            devices_cb.borrow_mut().push(PwDevice {
                id: name.to_string(),
                label,
                kind,
            });
        })
        .register();

    // Roundtrip: when the server answers our sync, every global that
    // existed before it has been announced.
    let done = Rc::new(RefCell::new(false));
    let done_cb = done.clone();
    let mainloop_cb = mainloop.clone();
    let pending = core.sync(0)?;
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                *done_cb.borrow_mut() = true;
                mainloop_cb.quit();
            }
        })
        .register();

    mainloop.run();

    if !*done.borrow() {
        return Err(anyhow!("PipeWire registry roundtrip did not complete"));
    }

    let mut out = devices.borrow().clone();
    // Stable ordering: microphones first, then sinks, alphabetical labels.
    out.sort_by(|a, b| (a.kind == PwDeviceKind::System, &a.label).cmp(&(b.kind == PwDeviceKind::System, &b.label)));
    debug!("pw: enumerated {} audio nodes", out.len());
    Ok(out)
}

/// Target for a capture stream.
#[derive(Debug, Clone)]
pub enum CaptureTarget {
    /// Default source (microphone) as chosen in the system.
    DefaultMicrophone,
    /// Default sink's monitor.
    DefaultSystem,
    /// A specific node by `node.name`.
    Node { id: String, kind: PwDeviceKind },
}

impl CaptureTarget {
    fn is_sink_capture(&self) -> bool {
        matches!(
            self,
            CaptureTarget::DefaultSystem
                | CaptureTarget::Node {
                    kind: PwDeviceKind::System,
                    ..
                }
        )
    }

    fn node_name(&self) -> Option<&str> {
        match self {
            CaptureTarget::Node { id, .. } => Some(id),
            _ => None,
        }
    }
}

enum LoopCommand {
    Terminate,
}

/// Lifecycle events reported from a capture stream's loop thread, in
/// addition to samples. These let callers notice a stream dying
/// silently (device unplugged, PipeWire error) instead of just seeing
/// `on_samples` stop firing.
#[derive(Debug, Clone)]
pub enum PwStreamEvent {
    /// The stream transitioned to `Error` after having been connected
    /// (`Streaming`/`Paused`). Carries the PipeWire error string.
    Error(String),
    /// The stream transitioned to `Unconnected` after having been
    /// connected — typically means the underlying device disappeared.
    Disconnected,
    /// The loop thread is exiting for any reason (including a clean
    /// `stop()`/`Drop`). Always sent last, after `Error`/`Disconnected`
    /// if either fired first.
    Ended,
}

/// A running PipeWire capture stream on its own loop thread.
///
/// `on_samples` is invoked from the loop thread with interleaved f32
/// frames at [`CAPTURE_RATE`] Hz / [`CAPTURE_CHANNELS`] channels.
/// Dropping (or calling [`stop`]) shuts the loop down and joins the
/// thread.
pub struct PwCaptureStream {
    quit_tx: Option<pw::channel::Sender<LoopCommand>>,
    handle: Option<JoinHandle<()>>,
    pub target_desc: String,
}

impl PwCaptureStream {
    /// Open a capture stream. `on_event` is invoked from the loop thread
    /// on lifecycle transitions (see [`PwStreamEvent`]) — in particular
    /// it is how a caller notices the stream died instead of just
    /// seeing `on_samples` stop firing.
    pub fn open(
        target: CaptureTarget,
        on_samples: Box<dyn FnMut(&[f32]) + Send>,
        on_event: Box<dyn Fn(PwStreamEvent) + Send>,
    ) -> Result<Self> {
        let (quit_tx, quit_rx) = pw::channel::channel::<LoopCommand>();
        // Reports either the successful connect or the first error back
        // to the caller so open() is synchronous and fallible.
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();
        // Set true once the loop thread has attached quit_rx to its main
        // loop — from that point on it is guaranteed to observe
        // LoopCommand::Terminate and exit, so it's safe to join. Before
        // that (e.g. stuck inside `context.connect_rc`), a join could
        // hang forever on a stalled PipeWire daemon.
        let loop_attached = Arc::new(AtomicBool::new(false));

        let target_desc = format!("{:?}", target);
        let thread_target = target.clone();
        let thread_loop_attached = loop_attached.clone();

        let handle = std::thread::Builder::new()
            .name("pw-capture".into())
            .spawn(move || {
                if let Err(e) = run_capture_loop(
                    thread_target,
                    on_samples,
                    on_event,
                    quit_rx,
                    &ready_tx,
                    thread_loop_attached,
                ) {
                    // If setup already succeeded this is a runtime error;
                    // otherwise ready_tx already carried it to open().
                    let _ = ready_tx.send(Err(e));
                }
            })
            .context("failed to spawn pw-capture thread")?;

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {
                info!("pw: capture stream running ({})", target_desc);
                Ok(Self {
                    quit_tx: Some(quit_tx),
                    handle: Some(handle),
                    target_desc,
                })
            }
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                // Loop thread never reported within the timeout. Ask it to
                // quit, but only join if it has actually attached quit_rx
                // to its main loop — otherwise it may be stuck deep inside
                // a PipeWire call (e.g. a stalled daemon) and never reach
                // the point where it would observe Terminate, which would
                // hang this join forever. Detach instead and let it die
                // with the process.
                let _ = quit_tx.send(LoopCommand::Terminate);
                if loop_attached.load(Ordering::SeqCst) {
                    let _ = handle.join();
                } else {
                    warn!(
                        "pw: capture stream setup timed out before the loop attached ({}); detaching loop thread",
                        target_desc
                    );
                    drop(handle);
                }
                Err(anyhow!("PipeWire capture stream setup timed out"))
            }
        }
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(tx) = self.quit_tx.take() {
            let _ = tx.send(LoopCommand::Terminate);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for PwCaptureStream {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_capture_loop(
    target: CaptureTarget,
    mut on_samples: Box<dyn FnMut(&[f32]) + Send>,
    on_event: Box<dyn Fn(PwStreamEvent) + Send>,
    quit_rx: pw::channel::Receiver<LoopCommand>,
    ready_tx: &std_mpsc::Sender<Result<()>>,
    loop_attached: Arc<AtomicBool>,
) -> Result<()> {
    // Always tell the caller this thread is exiting, however it got
    // here — this is what lets `AudioStreamManager` notice a stream
    // died and mark itself inactive.
    struct EndedGuard(Rc<dyn Fn(PwStreamEvent) + Send>);
    impl Drop for EndedGuard {
        fn drop(&mut self) {
            (self.0)(PwStreamEvent::Ended);
        }
    }

    // Single-threaded from here: wrap in Rc so both the state_changed
    // closure and the EndedGuard can hold a reference.
    let on_event: Rc<dyn Fn(PwStreamEvent) + Send> = Rc::from(on_event);
    let _ended_guard = EndedGuard(on_event.clone());

    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let mainloop_quit = mainloop.clone();
    let _quit_attach = quit_rx.attach(mainloop.loop_(), move |_| {
        mainloop_quit.quit();
    });
    // From here on the loop is guaranteed to see LoopCommand::Terminate,
    // so it's safe for open() to join this thread on a timeout.
    loop_attached.store(true, Ordering::SeqCst);

    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Communication",
        *pw::keys::APP_NAME => "Parley",
        *pw::keys::NODE_NAME => "parley-capture",
    };
    if target.is_sink_capture() {
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    }
    if let Some(node) = target.node_name() {
        props.insert(*pw::keys::TARGET_OBJECT, node);
    }

    let stream = pw::stream::StreamBox::new(&core, "parley-capture", props)?;

    let mainloop_err = mainloop.clone();
    // Tracks whether the stream has actually connected (reached
    // Streaming/Paused) so a later Error/Unconnected is reported as a
    // real failure rather than a normal part of initial negotiation.
    let reached_connected_cb = Rc::new(Cell::new(false));
    let on_event_state = on_event.clone();
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |_stream, _ud, _old, new| {
            match new {
                pw::stream::StreamState::Streaming | pw::stream::StreamState::Paused => {
                    reached_connected_cb.set(true);
                }
                pw::stream::StreamState::Error(e) => {
                    warn!("pw: capture stream error state: {}", e);
                    if reached_connected_cb.get() {
                        on_event_state(PwStreamEvent::Error(e.clone()));
                    }
                    mainloop_err.quit();
                }
                pw::stream::StreamState::Unconnected => {
                    if reached_connected_cb.get() {
                        warn!("pw: capture stream unconnected (device likely disappeared)");
                        on_event_state(PwStreamEvent::Disconnected);
                        mainloop_err.quit();
                    }
                }
                pw::stream::StreamState::Connecting => {}
            }
        })
        .process(move |stream, _ud| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let n_bytes = data.chunk().size() as usize;
            if n_bytes == 0 {
                return;
            }
            if let Some(bytes) = data.data() {
                let n_samples = n_bytes / std::mem::size_of::<f32>();
                let (_, samples, _) = unsafe { bytes[..n_bytes].align_to::<f32>() };
                // MAP_BUFFERS memory is f32-aligned in practice; the
                // align_to guard keeps us safe if it ever isn't.
                if samples.len() == n_samples {
                    on_samples(samples);
                }
            }
        })
        .register()?;

    // Fixed format: interleaved f32 stereo @ 48 kHz. The graph converts.
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(CAPTURE_RATE);
    audio_info.set_channels(CAPTURE_CHANNELS);
    let pod_object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let pod_bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(pod_object),
    )
    .map_err(|e| anyhow!("failed to serialize format pod: {:?}", e))?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&pod_bytes)
        .ok_or_else(|| anyhow!("failed to build format pod"))?];

    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    let _ = ready_tx.send(Ok(()));
    mainloop.run();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Requires a running PipeWire session — run explicitly:
    /// `cargo test -p parley-core --lib audio::pw::tests -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn enumerate_smoke() {
        let devices = enumerate_devices().expect("enumeration failed");
        println!("enumerated {} devices:", devices.len());
        for d in &devices {
            println!("  [{:?}] {} — {}", d.kind, d.label, d.id);
        }
        assert!(
            devices.iter().any(|d| d.kind == PwDeviceKind::Microphone),
            "expected at least one microphone node"
        );
    }

    #[test]
    #[ignore]
    fn capture_default_system_smoke() {
        let count = Arc::new(AtomicUsize::new(0));
        let count_cb = count.clone();
        let stream = PwCaptureStream::open(
            CaptureTarget::DefaultSystem,
            Box::new(move |samples| {
                count_cb.fetch_add(samples.len(), Ordering::Relaxed);
            }),
            Box::new(|_event| {}),
        )
        .expect("failed to open default system capture");
        std::thread::sleep(Duration::from_secs(2));
        stream.stop();
        let total = count.load(Ordering::Relaxed);
        println!("captured {} samples in 2s", total);
        // 2s of 48kHz stereo ≈ 192_000 samples; accept a generous lower
        // bound to absorb startup latency.
        assert!(total > 48_000, "expected ~192k samples, got {}", total);
    }
}
