//! The thin PipeWire layer: watch the registry for capture-stream nodes and
//! report them as [`WatchEvent`]s. Everything interesting (matching,
//! debouncing) is pure and lives in `rules` / `debounce`.
//!
//! Follows the `audio::pw` patterns: its own thread and main loop
//! (PipeWire objects are `!Send`), and a detach-don't-join shutdown so a
//! stalled daemon can never hang the caller.
//!
//! Registry *globals* for nodes only carry a few properties (`node.name`,
//! `media.class`, `client.id` — no `application.process.binary`), so each
//! `Stream/Input/Audio` node is bound to read its full info props. The
//! owning client's global props (`application.name`, `pipewire.sec.pid`) are
//! merged in underneath as a fallback.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use log::{debug, info, warn};
use pipewire as pw;
use pw::types::ObjectType;

use super::rules::{classify, MeetingApp, CAPTURE_STREAM_CLASS};

/// What the PipeWire thread observed.
#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// Stream node `node_id` exists and belongs to `app`.
    Present { node_id: u32, app: MeetingApp },
    /// Stream node `node_id` is gone (or no longer matches).
    Gone { node_id: u32 },
    /// The PipeWire connection dropped; every stream is gone.
    Reset,
}

/// How often the loop checks the stop flag.
const STOP_POLL: Duration = Duration::from_millis(250);
/// Back-off between reconnect attempts after the daemon goes away.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Run the watcher until `stop` is set, reconnecting if the PipeWire
/// connection drops. Blocks; call on a dedicated thread.
pub fn run(tx: Sender<WatchEvent>, stop: Arc<AtomicBool>) {
    pw::init();
    let own_pid = std::process::id();
    while !stop.load(Ordering::SeqCst) {
        match watch_once(&tx, &stop, own_pid) {
            Ok(()) => {}
            Err(e) => warn!("meeting-detect: PipeWire watch failed: {e}"),
        }
        if tx.send(WatchEvent::Reset).is_err() || stop.load(Ordering::SeqCst) {
            break;
        }
        info!("meeting-detect: PipeWire connection lost; retrying in {RECONNECT_DELAY:?}");
        let mut waited = Duration::ZERO;
        while waited < RECONNECT_DELAY && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(STOP_POLL);
            waited += STOP_POLL;
        }
    }
    debug!("meeting-detect: watcher thread exiting");
}

/// Bound node proxies (kept alive so their info listeners keep firing).
type BoundNodes = HashMap<u32, (pw::node::Node, pw::node::NodeListener)>;

fn watch_once(tx: &Sender<WatchEvent>, stop: &Arc<AtomicBool>, own_pid: u32) -> Result<()> {
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    // Quit on a fatal core error (daemon gone) so `run` can reconnect.
    let mainloop_err = mainloop.downgrade();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _seq, res, message| {
            warn!("meeting-detect: PipeWire core error id={id} res={res}: {message}");
            if id == pw::core::PW_ID_CORE {
                if let Some(mainloop) = mainloop_err.upgrade() {
                    mainloop.quit();
                }
            }
        })
        .register();

    // Poll the stop flag from inside the loop.
    let mainloop_stop = mainloop.downgrade();
    let stop_flag = stop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop_flag.load(Ordering::SeqCst) {
            if let Some(mainloop) = mainloop_stop.upgrade() {
                mainloop.quit();
            }
        }
    });
    timer.update_timer(Some(STOP_POLL), Some(STOP_POLL));

    let clients: Rc<RefCell<HashMap<u32, HashMap<String, String>>>> = Rc::default();
    let nodes: Rc<RefCell<BoundNodes>> = Rc::default();
    // Nodes we've reported as Present, so removal only reports those.
    let reported: Rc<RefCell<std::collections::HashSet<u32>>> = Rc::default();

    let registry_weak = registry.downgrade();
    let clients_add = clients.clone();
    let nodes_add = nodes.clone();
    let reported_add = reported.clone();
    let tx_add = tx.clone();

    let clients_rm = clients.clone();
    let nodes_rm = nodes.clone();
    let reported_rm = reported.clone();
    let tx_rm = tx.clone();

    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props else { return };
            match global.type_ {
                ObjectType::Client => {
                    clients_add.borrow_mut().insert(global.id, dict_to_map(props));
                }
                ObjectType::Node => {
                    if props.get("media.class") != Some(CAPTURE_STREAM_CLASS) {
                        return;
                    }
                    let Some(registry) = registry_weak.upgrade() else { return };
                    let node: pw::node::Node = match registry.bind(global) {
                        Ok(node) => node,
                        Err(e) => {
                            debug!("meeting-detect: failed to bind node {}: {e}", global.id);
                            return;
                        }
                    };
                    let clients = clients_add.clone();
                    let reported = reported_add.clone();
                    let tx = tx_add.clone();
                    let listener = node
                        .add_listener_local()
                        .info(move |info| {
                            let Some(node_props) = info.props() else { return };
                            let mut merged = dict_to_map(node_props);
                            let client_id = merged
                                .get("client.id")
                                .and_then(|id| id.parse::<u32>().ok());
                            if let Some(client_props) =
                                client_id.and_then(|id| clients.borrow().get(&id).cloned())
                            {
                                for (k, v) in client_props {
                                    merged.entry(k).or_insert(v);
                                }
                            }
                            fill_binary_from_proc(&mut merged);
                            let node_id = info.id();
                            match classify(&merged, own_pid) {
                                Some(app) => {
                                    debug!(
                                        "meeting-detect: node {node_id} is a {} capture stream",
                                        app.name
                                    );
                                    reported.borrow_mut().insert(node_id);
                                    let _ = tx.send(WatchEvent::Present { node_id, app });
                                }
                                None => {
                                    if reported.borrow_mut().remove(&node_id) {
                                        let _ = tx.send(WatchEvent::Gone { node_id });
                                    }
                                }
                            }
                        })
                        .register();
                    nodes_add.borrow_mut().insert(global.id, (node, listener));
                }
                _ => {}
            }
        })
        .global_remove(move |id| {
            clients_rm.borrow_mut().remove(&id);
            nodes_rm.borrow_mut().remove(&id);
            if reported_rm.borrow_mut().remove(&id) {
                let _ = tx_rm.send(WatchEvent::Gone { node_id: id });
            }
        })
        .register();

    info!("meeting-detect: watching PipeWire for meeting-app capture streams");
    mainloop.run();

    // Drop proxies before the core/context they belong to.
    nodes.borrow_mut().clear();
    Ok(())
}

/// Native PipeWire clients don't put `application.process.binary` in the
/// node's props (pulse-API clients — most browsers and Electron apps — do).
/// Fill it from `/proc/<pid>/comm` when we know the pid.
fn fill_binary_from_proc(props: &mut HashMap<String, String>) {
    if props.contains_key("application.process.binary") {
        return;
    }
    let Some(pid) = props
        .get("application.process.id")
        .or_else(|| props.get("pipewire.sec.pid"))
        .cloned()
    else {
        return;
    };
    if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        let comm = comm.trim();
        if !comm.is_empty() {
            props.insert("application.process.binary".into(), comm.to_string());
        }
    }
}

fn dict_to_map(dict: &pw::spa::utils::dict::DictRef) -> HashMap<String, String> {
    dict.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// One-shot listing of the capture streams that are present right now and
/// how they classify — for the ignored live smoke test and debugging.
/// Returns `(node_id, merged props, classification)`.
pub fn snapshot_streams(
    timeout: Duration,
) -> Result<Vec<(u32, HashMap<String, String>, Option<MeetingApp>)>> {
    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<(u32, HashMap<String, String>)>();
    // Detached on timeout, like `pw::enumerate_devices`.
    std::thread::Builder::new()
        .name("pw-meeting-snapshot".into())
        .spawn(move || {
            if let Err(e) = list_capture_streams(raw_tx) {
                warn!("meeting-detect: snapshot failed: {e}");
            }
        })?;
    let deadline = std::time::Instant::now() + timeout;
    let own_pid = std::process::id();
    let mut latest: HashMap<u32, HashMap<String, String>> = HashMap::new();
    // Ends when the worker finishes (sender dropped) or at the deadline.
    while let Ok((id, props)) =
        raw_rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
    {
        latest.insert(id, props);
    }
    let mut out: Vec<_> = latest
        .into_iter()
        .map(|(id, props)| {
            let app = classify(&props, own_pid);
            (id, props, app)
        })
        .collect();
    out.sort_by_key(|(id, _, _)| *id);
    Ok(out)
}

/// Roundtrip the registry, bind every capture stream, and send each one's
/// merged props once. Used only by [`snapshot_streams`].
fn list_capture_streams(out: Sender<(u32, HashMap<String, String>)>) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let clients: Rc<RefCell<HashMap<u32, HashMap<String, String>>>> = Rc::default();
    let nodes: Rc<RefCell<BoundNodes>> = Rc::default();
    let registry_weak = registry.downgrade();
    let clients_add = clients.clone();
    let nodes_add = nodes.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props else { return };
            match global.type_ {
                ObjectType::Client => {
                    clients_add.borrow_mut().insert(global.id, dict_to_map(props));
                }
                ObjectType::Node if props.get("media.class") == Some(CAPTURE_STREAM_CLASS) => {
                    let Some(registry) = registry_weak.upgrade() else { return };
                    let Ok(node) = registry.bind::<pw::node::Node, _>(global) else { return };
                    let clients = clients_add.clone();
                    let out = out.clone();
                    let listener = node
                        .add_listener_local()
                        .info(move |info| {
                            let Some(node_props) = info.props() else { return };
                            let mut merged = dict_to_map(node_props);
                            if let Some(client_props) = merged
                                .get("client.id")
                                .and_then(|id| id.parse::<u32>().ok())
                                .and_then(|id| clients.borrow().get(&id).cloned())
                            {
                                for (k, v) in client_props {
                                    merged.entry(k).or_insert(v);
                                }
                            }
                            fill_binary_from_proc(&mut merged);
                            let _ = out.send((info.id(), merged));
                        })
                        .register();
                    nodes_add.borrow_mut().insert(global.id, (node, listener));
                }
                _ => {}
            }
        })
        .register();

    // Two roundtrips: the first announces globals (and issues binds), the
    // second flushes the bound nodes' initial info events.
    for _ in 0..2 {
        let done = Rc::new(std::cell::Cell::new(false));
        let done_cb = done.clone();
        let mainloop_cb = mainloop.clone();
        let pending = core.sync(0)?;
        let _l = core
            .add_listener_local()
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    done_cb.set(true);
                    mainloop_cb.quit();
                }
            })
            .register();
        mainloop.run();
    }
    nodes.borrow_mut().clear();
    Ok(())
}
