//! The stderr guard redirects fd 2 only while engaged, also for children
//! (which is where the macOS malloc diagnostics come from), and restores the
//! original target on release and in the panic hook. Own test binary: it
//! rewires the process's fd 2.
#![cfg(unix)]

use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::process::Command;

use rness_tui::stderr_guard;

fn contents(file: &mut std::fs::File) -> String {
    let mut text = String::new();
    file.seek(std::io::SeekFrom::Start(0)).unwrap();
    file.read_to_string(&mut text).unwrap();
    text
}

fn child_writes_stderr(text: &str) {
    let status = Command::new("/bin/sh")
        .args(["-c", &format!("printf '{text}' >&2")])
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn stderr_goes_to_the_sink_only_while_engaged() {
    // Stand-in for the terminal: point fd 2 at a file we can read.
    let mut terminal = tempfile::tempfile().unwrap();
    let original = unsafe { libc::dup(2) };
    assert!(original >= 0);
    assert!(unsafe { libc::dup2(terminal.as_raw_fd(), 2) } >= 0);
    let mut sink = tempfile::tempfile().unwrap();

    std::io::stderr().write_all(b"before;").unwrap();
    stderr_guard::engage(&sink).unwrap();
    stderr_guard::engage(&sink).unwrap(); // idempotent
    std::io::stderr().write_all(b"own;").unwrap();
    child_writes_stderr("child;");
    stderr_guard::release();
    stderr_guard::release(); // idempotent
    std::io::stderr().write_all(b"after;").unwrap();
    child_writes_stderr("child-after;");

    // Editor handoff: release and engage again.
    stderr_guard::engage(&sink).unwrap();
    std::io::stderr().write_all(b"again;").unwrap();

    // A panic releases first, so its message reaches the terminal. libtest
    // captures the default hook's output, so the "previous" hook here writes
    // to fd 2 directly.
    std::panic::set_hook(Box::new(|_| {
        let text = b"boom-marker;";
        unsafe { libc::write(2, text.as_ptr().cast(), text.len()) };
    }));
    stderr_guard::release_on_panic();
    let _ = std::panic::catch_unwind(|| panic!("boom"));
    let _ = std::panic::take_hook();
    std::io::stderr().write_all(b"post-panic;").unwrap();

    // Put the real stderr back before asserting.
    unsafe {
        libc::dup2(original, 2);
        libc::close(original);
    }
    let shown = contents(&mut terminal);
    let hidden = contents(&mut sink);
    assert_eq!(hidden, "own;child;again;", "sink: {hidden:?}");
    assert_eq!(
        shown, "before;after;child-after;boom-marker;post-panic;",
        "terminal: {shown:?}"
    );
}
