//! Exit-status watcher: the one race-free way to tell a crash from a
//! normal quit on macOS.
//!
//! For every watched pid, a kqueue `EVFILT_PROC` filter delivers the
//! process's wait status on exit (`NOTE_EXITSTATUS`). A status that ends
//! in a signal (SIGSEGV, SIGKILL, …) means the app crashed or was killed;
//! a plain `exit()` means the app chose to quit. Keep-alive uses this to
//! respect deliberate quits unless always-running behavior is enabled.
//!
//! One background thread blocks in `kevent` forever — no polling, no idle
//! wake-ups. Registrations happen from the main thread on the same kqueue,
//! which is explicitly thread-safe.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitKind {
    /// The process ended via a signal (crash, force quit, kill).
    Crashed,
    /// The process exited on its own.
    Clean,
}

pub struct ProcExitWatcher {
    kq: libc::c_int,
    exits: Arc<Mutex<HashMap<libc::pid_t, ExitKind>>>,
}

pub type ProcExitHandle = Arc<ProcExitWatcher>;

// The kqueue descriptor is used from the main thread (registration) and
// the watcher thread (waiting); kevent is thread-safe per queue.
unsafe impl Send for ProcExitWatcher {}
unsafe impl Sync for ProcExitWatcher {}

impl ProcExitWatcher {
    pub fn spawn() -> io::Result<ProcExitHandle> {
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        let exits: Arc<Mutex<HashMap<libc::pid_t, ExitKind>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let thread_exits = exits.clone();
        std::thread::Builder::new()
            .name("rustquit-proc-exit".to_string())
            .spawn(move || {
                loop {
                    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
                    let n = unsafe {
                        libc::kevent(kq, std::ptr::null(), 0, &mut event, 1, std::ptr::null())
                    };
                    if n < 0 {
                        let err = io::Error::last_os_error();
                        if err.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        tracing::error!(%err, "proc-exit kqueue failed; watcher stopped");
                        return;
                    }
                    if n == 0 || event.filter != libc::EVFILT_PROC {
                        continue;
                    }
                    let pid = event.ident as libc::pid_t;
                    let status = event.data as libc::c_int;
                    let kind = if libc::WIFSIGNALED(status) {
                        ExitKind::Crashed
                    } else {
                        ExitKind::Clean
                    };
                    tracing::debug!(pid, status, ?kind, "watched process exited");
                    if let Ok(mut exits) = thread_exits.lock() {
                        exits.insert(pid, kind);
                    }
                }
            })?;

        Ok(Arc::new(ProcExitWatcher { kq, exits }))
    }

    /// Starts watching a process. Safe to call repeatedly for the same
    /// pid; a pid that is already gone is simply never reported.
    pub fn watch(&self, pid: libc::pid_t) {
        let change = libc::kevent {
            ident: pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT | libc::NOTE_EXITSTATUS,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let result = unsafe {
            libc::kevent(
                self.kq,
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if result < 0 {
            // ESRCH: the process is already gone — nothing to watch.
            tracing::debug!(pid, err = %io::Error::last_os_error(), "cannot watch process exit");
        }
    }

    /// Consumes the recorded exit kind for a pid, if its exit was seen.
    pub fn take_exit_kind(&self, pid: libc::pid_t) -> Option<ExitKind> {
        self.exits.lock().ok()?.remove(&pid)
    }
}
