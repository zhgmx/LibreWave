//! Process termination notification for the daemon control thread.
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

static REGISTERED: AtomicBool = AtomicBool::new(false);
static REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn notify_shutdown(_: libc::c_int) {
    // The signal handler performs only a lock-free atomic store. It never
    // allocates, logs, acquires a lock, or releases an audio resource.
    REQUESTED.store(true, Ordering::Relaxed);
}

/// Owns one process-wide SIGINT/SIGTERM registration and restores prior handlers.
pub struct ShutdownSignals {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

impl ShutdownSignals {
    /// Install notifications before starting daemon worker threads.
    /// # Errors
    /// Rejects a second registration or a failed operating-system operation.
    pub fn install() -> io::Result<Self> {
        if REGISTERED.compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return Err(io::Error::other("daemon termination signals are already registered"));
        }
        REQUESTED.store(false, Ordering::Relaxed);
        let mut guard = Self { previous: Vec::with_capacity(2) };
        for signal in [libc::SIGINT, libc::SIGTERM] {
            // SAFETY: sigaction is a C value whose zeroed storage is valid;
            // sigemptyset initializes its mask before registration.
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = notify_shutdown as *const () as usize;
            // SAFETY: the mask is writable storage in action.
            if unsafe { libc::sigemptyset(&raw mut action.sa_mask) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: previous is writable sigaction storage; both pointers
            // remain live through this call, and signal is SIGINT or SIGTERM.
            let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
            if unsafe { libc::sigaction(signal, &raw const action, &raw mut previous) } != 0 {
                return Err(io::Error::last_os_error());
            }
            guard.previous.push((signal, previous));
        }
        Ok(guard)
    }

    /// Read the signal notification on the control thread.
    #[must_use]
    pub fn requested(&self) -> bool {
        REQUESTED.load(Ordering::Relaxed)
    }
}

impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        for (signal, previous) in self.previous.iter().rev() {
            // SAFETY: restore the exact previous action captured by this guard.
            unsafe { libc::sigaction(*signal, std::ptr::from_ref(previous), std::ptr::null_mut()) };
        }
        REGISTERED.store(false, Ordering::Relaxed);
    }
}
