//! Slow fake tools and the checks that agt leaves no process behind.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{Env, executable};

/// A tool that starts a child, records its pid and then sleeps for a minute.
pub fn slow_tool(env: &Env) -> (String, PathBuf) {
    let tool = env.path("slow-forge.sh");
    let pid = env.path("slow.pid");
    executable(
        &tool,
        format!(
            "#!/bin/sh\nsleep 60 &\necho $! > '{}'\nwait\n",
            pid.display()
        ),
    );
    (tool.display().to_string(), pid)
}

pub fn assert_gone(pid: &Path) {
    let pid: i32 = fs::read_to_string(pid).unwrap().trim().parse().unwrap();
    for _ in 0..100 {
        // SAFETY: signal 0 only asks whether the process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("process {pid} is still running");
}

/// Sends `signal` to `agt` once the tool it waits on has recorded its child in `pid`, and checks
/// that agt died of that signal and left neither the tool nor its child behind.
pub fn assert_signal_kills_the_tool(mut agt: std::process::Child, signal: i32, pid: &Path) {
    let started = Instant::now();
    while fs::read_to_string(pid).map_or(true, |text| text.trim().is_empty()) {
        assert!(started.elapsed() < Duration::from_secs(20), "no child");
        std::thread::sleep(Duration::from_millis(20));
    }
    let id = i32::try_from(agt.id()).unwrap();
    // SAFETY: plain kill(2) of the agt process this test started.
    unsafe { libc::kill(id, signal) };
    let status = agt.wait().unwrap();
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(signal)
    );
    assert_gone(pid);
}
