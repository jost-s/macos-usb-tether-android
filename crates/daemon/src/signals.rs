//! Shutdown on SIGINT/SIGTERM.
//!
//! Signals only flip a flag; the main loop then unwinds normally so routes and
//! DNS are restored by the usual Drop path.

use std::sync::atomic::{AtomicBool, Ordering};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn handler(_signal: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

pub fn install() {
    // SAFETY: the handler only stores to an atomic, which is signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
        // A closed ctl socket must not kill the daemon.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

pub fn requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}
