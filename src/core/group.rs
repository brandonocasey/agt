//! Keeps a child that leads its own process group from outliving agt. Such a child is out of
//! the terminal's foreground job, so Ctrl-C (or a kill of agt's group) stops only agt and the
//! tool's own children (ssh, a remote helper) would run on.

use std::sync::atomic::{AtomicI32, Ordering};

/// The group to kill when a signal arrives; 0 when none is watched. One at a time: the waits
/// that use this run one after another.
static GROUP: AtomicI32 = AtomicI32::new(0);

const SIGNALS: [i32; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

extern "C" fn on_signal(signal: i32) {
    let group = GROUP.load(Ordering::SeqCst);
    // SAFETY: `kill`, `signal` and `raise` are async-signal-safe and take plain integers. A
    // negative pid signals the process group the watched child leads.
    unsafe {
        if group > 0 {
            libc::kill(-group, libc::SIGKILL);
        }
        // Die of the signal as agt would have, once this handler returns.
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// While alive, SIGINT, SIGTERM and SIGHUP, unless agt ignores them, kill the group `pid` leads
/// and then end agt as the signal would have. Dropping it restores the handlers agt had.
pub struct Watch {
    previous: [libc::sighandler_t; SIGNALS.len()],
}

impl Watch {
    pub fn new(pid: u32) -> Option<Watch> {
        let group = i32::try_from(pid).ok()?;
        GROUP.store(group, Ordering::SeqCst);
        let handler = on_signal as extern "C" fn(i32) as libc::sighandler_t;
        let mut previous = [libc::SIG_DFL; SIGNALS.len()];
        for (slot, signal) in previous.iter_mut().zip(SIGNALS) {
            // A signal agt was started to ignore (`nohup`, a background job) stays ignored.
            if ignored(signal) {
                *slot = libc::SIG_IGN;
                continue;
            }
            // SAFETY: the handler only calls async-signal-safe functions and reads an atomic.
            *slot = unsafe { libc::signal(signal, handler) };
        }
        Some(Watch { previous })
    }
}

/// Whether `signal` is ignored now, read without changing it.
fn ignored(signal: i32) -> bool {
    // SAFETY: a zeroed `sigaction` is a valid output buffer, and a null new action only reads.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        libc::sigaction(signal, std::ptr::null(), &mut current) == 0
            && current.sa_sigaction == libc::SIG_IGN
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        for (handler, signal) in self.previous.iter().zip(SIGNALS) {
            // SAFETY: puts back what `signal` returned for this signal.
            unsafe { libc::signal(signal, *handler) };
        }
        GROUP.store(0, Ordering::SeqCst);
    }
}
