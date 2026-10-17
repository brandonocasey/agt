//! Runs a tool to completion with its output captured, an optional timeout and an optional
//! cancel flag. The child leads its own process group, so a timeout or cancel stops the tool's
//! own children too and no one keeps the output pipes open.

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(5);

#[derive(Default)]
pub struct Options<'a> {
    pub env: &'a [(&'a str, &'a str)],
    pub timeout: Option<Duration>,
    pub cancel: Option<&'a AtomicBool>,
}

#[derive(Debug, Default)]
pub struct Run {
    /// The exit status, `None` when the tool did not start or a signal ended it.
    pub code: Option<i32>,
    /// The name of the signal that ended the tool, such as `SIGTERM`.
    pub signal: Option<&'static str>,
    pub stdout: String,
    pub stderr: String,
    /// Why the tool did not start, as an errno name such as `ENOENT`.
    pub spawn_error: Option<&'static str>,
    pub timed_out: bool,
    /// The last moment the tool was observed still running, `None` when it never was.
    pub last_running: Option<Instant>,
}

fn signal_name(signal: i32) -> &'static str {
    match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGABRT => "SIGABRT",
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        _ => "SIGUNKNOWN",
    }
}

fn errno_name(kind: std::io::ErrorKind) -> &'static str {
    match kind {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::PermissionDenied => "EACCES",
        _ => "EIO",
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

fn terminate(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: kill(2) takes plain integers; a negative pid addresses the child's group.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
    }
}

pub fn run<S: AsRef<OsStr>>(program: &OsStr, args: &[S], options: &Options) -> Run {
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(options.env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Run {
                spawn_error: Some(errno_name(error.kind())),
                ..Run::default()
            };
        }
    };
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let started = Instant::now();
    let mut timed_out = false;
    let mut stopped = false;
    let mut last_running = None;
    let status = loop {
        // Read the time first: the child is known to be running at any time before a
        // `try_wait` that finds it still running.
        let now = Instant::now();
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        last_running = Some(now);
        let cancelled = options
            .cancel
            .is_some_and(|flag| flag.load(Ordering::SeqCst));
        let late = options
            .timeout
            .is_some_and(|limit| now.duration_since(started) >= limit);
        if (cancelled || late) && !stopped {
            stopped = true;
            timed_out = late && !cancelled;
            terminate(child.id());
        }
        // Wake exactly when the timeout is due, so a tool still running then is seen so.
        let nap = options.timeout.filter(|_| !stopped).map_or(POLL, |limit| {
            limit
                .saturating_sub(now.duration_since(started))
                .clamp(Duration::from_micros(100), POLL)
        });
        thread::sleep(nap);
    };
    let join = |handle: Option<thread::JoinHandle<String>>| {
        handle.and_then(|h| h.join().ok()).unwrap_or_default()
    };
    Run {
        code: status.and_then(|s| s.code()),
        signal: status.and_then(|s| s.signal()).map(signal_name),
        stdout: join(stdout),
        stderr: join(stderr),
        spawn_error: None,
        timed_out,
        last_running,
    }
}
