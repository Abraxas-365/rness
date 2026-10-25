//! Keeps stray stderr bytes off the screen while the TUI owns the terminal.
//!
//! Anything written to fd 2 paints straight into the viewport, bypassing the
//! renderer: library warnings, children that inherit stderr, and macOS
//! libmalloc diagnostics printed by a forked child before it execs
//! (`rness(pid) MallocStackLogging: can't turn off malloc stack logging…`).
//! While engaged, fd 2 points at a sink (the log file); [`release`] puts the
//! terminal back. The TUI releases before handing the terminal to an editor
//! and on exit, and a panic hook releases before the panic message prints.
//!
//! State is one atomic fd, so [`release`] is lock-free and safe from a panic
//! hook. Engage/release are idempotent.

use std::fs::File;

#[cfg(unix)]
static SAVED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// Point fd 2 at `sink`, remembering the current target. No-op if already
/// engaged.
#[cfg(unix)]
pub fn engage(sink: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::sync::atomic::Ordering;
    if SAVED.load(Ordering::SeqCst) >= 0 {
        return Ok(());
    }
    // SAFETY: plain fd syscalls on descriptors we own or fd 2.
    unsafe {
        let saved = libc::fcntl(libc::STDERR_FILENO, libc::F_DUPFD_CLOEXEC, 3);
        if saved < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::dup2(sink.as_raw_fd(), libc::STDERR_FILENO) < 0 {
            let error = std::io::Error::last_os_error();
            libc::close(saved);
            return Err(error);
        }
        SAVED.store(saved, Ordering::SeqCst);
    }
    Ok(())
}

/// Restore fd 2 to what it was before [`engage`]. No-op when not engaged.
#[cfg(unix)]
pub fn release() {
    let saved = SAVED.swap(-1, std::sync::atomic::Ordering::SeqCst);
    if saved >= 0 {
        // SAFETY: `saved` is the duplicate taken by `engage`, owned here.
        unsafe {
            libc::dup2(saved, libc::STDERR_FILENO);
            libc::close(saved);
        }
    }
}

#[cfg(not(unix))]
pub fn engage(_sink: &File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
pub fn release() {}

/// Install (once) a panic hook that releases before the previous hooks run,
/// so the panic message reaches the terminal, not the sink.
pub fn release_on_panic() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            release();
            previous(info);
        }));
    });
}
