//! Persistent logs and crash breadcrumbs.
//!
//! Launched from the desktop entry, Parley's stderr only reaches the user
//! journal, and the SIGSEGVs of 2026-09-15/16 left nothing but a one-frame
//! `coredumpctl` entry (issue #91). So every log line is also appended to
//! `~/.local/state/parley/logs/parley.log` (XDG state dir, kept out of the
//! data dir so it can't interfere with the legacy data-dir migration), the
//! previous run's log is kept as `parley.log.1` once it passes 10 MB, panics
//! are logged with a backtrace, and fatal signals write one line to the log
//! before the default action runs (so systemd-coredump still gets its core).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};

/// Rotate once the log passes this size (checked at startup only).
const ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Raw fd of the open log file for the signal handler (-1 = none).
static LOG_FD: AtomicI32 = AtomicI32::new(-1);

/// Where logs live: `$XDG_STATE_HOME/parley/logs` (`~/.local/state/...`).
pub fn log_dir() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|d| d.join("parley").join("logs"))
}

/// Open (rotating first if it's too big) the log file in `dir`.
fn open_log_file(dir: &PathBuf) -> io::Result<File> {
    fs::create_dir_all(dir)?;
    let path = dir.join("parley.log");
    if fs::metadata(&path).map(|m| m.len() > ROTATE_BYTES).unwrap_or(false) {
        let _ = fs::rename(&path, dir.join("parley.log.1"));
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Writes every log record to stderr and, when available, the log file.
struct Tee {
    file: Option<File>,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        if let Some(file) = self.file.as_mut() {
            // A full disk mustn't take logging (or the app) down with it.
            let _ = file.write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        if let Some(file) = self.file.as_mut() {
            let _ = file.flush();
        }
        Ok(())
    }
}

/// Initialise `env_logger` writing to stderr + the log file, then install
/// the panic hook and fatal-signal breadcrumbs. Call once, first thing.
pub fn init(default_filter: &str) {
    let dir = log_dir();
    let file = dir.as_ref().and_then(|d| open_log_file(d).ok());
    if let Some(f) = file.as_ref() {
        LOG_FD.store(f.as_raw_fd(), Ordering::SeqCst);
    }
    // The fd stays valid for the process lifetime: the Tee owning the File
    // lives inside the global logger, which is never dropped.
    let has_file = file.is_some();

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
        .target(env_logger::Target::Pipe(Box::new(Tee { file })))
        .init();

    if let (true, Some(d)) = (has_file, dir.as_ref()) {
        log::info!(
            "Parley {} starting; logging to {}",
            env!("CARGO_PKG_VERSION"),
            d.join("parley.log").display()
        );
    }

    install_panic_hook();
    install_fatal_signal_handlers();
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let backtrace = std::backtrace::Backtrace::force_capture();
        log::error!(
            "PANIC on thread '{}': {}\n{}",
            thread.name().unwrap_or("<unnamed>"),
            info,
            backtrace
        );
        previous(info);
    }));
}

const FATAL_SIGNALS: [libc::c_int; 5] =
    [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGFPE, libc::SIGABRT];

/// Async-signal-safe: only `write(2)` on a pre-opened fd, then the handler
/// resets itself (`SA_RESETHAND`) and re-raises so the default action — and
/// systemd-coredump — still happen.
extern "C" fn on_fatal_signal(sig: libc::c_int) {
    let fd = LOG_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let name: &[u8] = match sig {
            libc::SIGSEGV => b"SIGSEGV",
            libc::SIGBUS => b"SIGBUS",
            libc::SIGILL => b"SIGILL",
            libc::SIGFPE => b"SIGFPE",
            libc::SIGABRT => b"SIGABRT",
            _ => b"fatal signal",
        };
        let parts: [&[u8]; 3] = [
            b"FATAL: Parley received ",
            name,
            b"; native crash, see `coredumpctl info parley` for the stack\n",
        ];
        for part in parts {
            unsafe { libc::write(fd, part.as_ptr().cast(), part.len()) };
        }
    }
    unsafe { libc::raise(sig) };
}

fn install_fatal_signal_handlers() {
    if LOG_FD.load(Ordering::SeqCst) < 0 {
        return;
    }
    for sig in FATAL_SIGNALS {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            let handler: extern "C" fn(libc::c_int) = on_fatal_signal;
            action.sa_sigaction = handler as libc::sighandler_t;
            // ONSTACK: run on the alternate signal stack Rust sets up per
            // thread, so a stack-overflow SIGSEGV can still be reported.
            action.sa_flags = libc::SA_RESETHAND | libc::SA_ONSTACK;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_oversized_log_and_keeps_one_previous() {
        let dir = std::env::temp_dir().join(format!("parley-log-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let big = vec![b'x'; (ROTATE_BYTES + 1) as usize];
        fs::write(dir.join("parley.log"), &big).unwrap();

        let mut f = open_log_file(&dir).unwrap();
        f.write_all(b"fresh\n").unwrap();

        assert_eq!(fs::read(dir.join("parley.log")).unwrap(), b"fresh\n");
        assert_eq!(fs::metadata(dir.join("parley.log.1")).unwrap().len(), big.len() as u64);

        // Under the limit: appended, not rotated.
        drop(f);
        let mut f = open_log_file(&dir).unwrap();
        f.write_all(b"more\n").unwrap();
        assert_eq!(fs::read(dir.join("parley.log")).unwrap(), b"fresh\nmore\n");
        let _ = fs::remove_dir_all(&dir);
    }
}
