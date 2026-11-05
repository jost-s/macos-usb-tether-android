//! Shutdown on SIGINT/SIGTERM, and reopening the service log on SIGHUP.
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
        // Via a pointer rather than straight to an integer: casting a function
        // item directly is what `function_casts_as_integer` warns about.
        let handler = handler as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
        // A closed ctl socket must not kill the daemon.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

pub fn requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// newsyslog sends SIGHUP after moving the log aside.
pub fn reopen_log_on_hangup() {
    // SAFETY: the handler only calls async-signal-safe functions.
    unsafe {
        libc::signal(libc::SIGHUP, reopen_log as *const () as libc::sighandler_t);
    }
}

extern "C" fn reopen_log(_signal: libc::c_int) {
    // SAFETY: open, dup2 and close are async-signal-safe, and the path is static.
    unsafe {
        let fd = libc::open(
            crate::service::LOG_PATH_C.as_ptr(),
            libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT,
            0o644 as libc::c_uint,
        );
        if fd >= 0 {
            libc::dup2(fd, libc::STDOUT_FILENO);
            libc::dup2(fd, libc::STDERR_FILENO);
            libc::close(fd);
        }
    }
}
