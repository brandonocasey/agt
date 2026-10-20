use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

pub struct Captured {
    pub output: Output,
    // Callers decide whether the command's failure takes precedence over an input error.
    pub input: io::Result<()>,
}

/// Captures both streams while writing input concurrently. A deadline also bounds inherited
/// pipes and kills the process group on failure, including children of an exited command.
pub fn output(
    command: &mut Command,
    input: Option<&[u8]>,
    deadline: Option<Instant>,
) -> io::Result<Captured> {
    if deadline.is_some_and(|end| Instant::now() >= end) {
        return Err(timeout());
    }
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if deadline.is_some() {
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let _watch = deadline.and_then(|_| super::group::Watch::new(child.id()));
    let input = input.map(|bytes| {
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let bytes = bytes.to_vec();
        worker(move || match stdin.write_all(&bytes) {
            Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
            result => result,
        })
    });
    let stdout = reader(child.stdout.take().expect("stdout is piped"));
    let stderr = reader(child.stderr.take().expect("stderr is piped"));
    let result = collect(&mut child, input, stdout, stderr, deadline);
    finish(&mut child, result, deadline.is_some())
}

fn finish<T>(child: &mut Child, result: io::Result<T>, group: bool) -> io::Result<T> {
    if result.is_err() {
        if group && let Ok(pid) = i32::try_from(child.id()) {
            // SAFETY: the child leads this group, so only its descendants receive the signal.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

/// Runs a user-supplied command to completion with no deadline and returns its exit status. The
/// caller sets the output streams and everything else; stdin is null, or a pipe that receives
/// `input` when there is some (a command that exits without reading it is not an error).
pub fn status(command: &mut Command, input: Option<&[u8]>) -> io::Result<ExitStatus> {
    command.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = command.spawn()?;
    if let Some(bytes) = input
        && let Some(mut stdin) = child.stdin.take()
    {
        let _ = stdin.write_all(bytes);
    }
    child.wait()
}

/// Runs a check with the caller's output streams and kills its group if the deadline passes.
pub fn status_until(command: &mut Command, deadline: Instant) -> io::Result<ExitStatus> {
    if Instant::now() >= deadline {
        return Err(timeout());
    }
    command.stdin(Stdio::null()).process_group(0);
    let mut child = command.spawn()?;
    let _watch = super::group::Watch::new(child.id());
    let result = wait(&mut child, Some(deadline));
    finish(&mut child, result, true)
}

/// The longest `--timeout` or `--interval` in seconds (one year), so a deadline never overflows an `Instant`.
pub const MAX_WAIT_SECS: u64 = 31_536_000;

/// What `poll_until` ended with.
pub enum Polled<T> {
    Done(T),
    TimedOut,
}

/// Calls `attempt` with the deadline until it returns `Some`, sleeping `interval` between calls
/// but never past `timeout` from now. `attempt` MUST finish by the deadline it is given.
pub fn poll_until<T, E>(
    timeout: Duration,
    interval: Duration,
    mut attempt: impl FnMut(Instant) -> Result<Option<T>, E>,
) -> Result<Polled<T>, E> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = attempt(deadline)? {
            return Ok(Polled::Done(value));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(Polled::TimedOut);
        }
        std::thread::sleep(interval.min(remaining));
    }
}

fn wait(child: &mut Child, deadline: Option<Instant>) -> io::Result<ExitStatus> {
    let Some(end) = deadline else {
        return child.wait();
    };
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timeout());
        }
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(5).min(remaining));
    }
}

fn collect(
    child: &mut Child,
    input: Option<Receiver<io::Result<()>>>,
    stdout: Receiver<io::Result<Vec<u8>>>,
    stderr: Receiver<io::Result<Vec<u8>>>,
    deadline: Option<Instant>,
) -> io::Result<Captured> {
    let status = wait(child, deadline)?;
    Ok(Captured {
        output: Output {
            status,
            stdout: receive(stdout, deadline)??,
            stderr: receive(stderr, deadline)??,
        },
        input: input
            .map(|input| receive(input, deadline))
            .transpose()?
            .unwrap_or(Ok(())),
    })
}

fn worker<T: Send + 'static>(run: impl FnOnce() -> T + Send + 'static) -> Receiver<T> {
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(run());
    });
    receive
}

fn reader(mut pipe: impl Read + Send + 'static) -> Receiver<io::Result<Vec<u8>>> {
    worker(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).map(|_| bytes)
    })
}

fn receive<T>(receiver: Receiver<T>, deadline: Option<Instant>) -> io::Result<T> {
    match deadline {
        Some(end) => receiver
            .recv_timeout(end.saturating_duration_since(Instant::now()))
            .map_err(|error| match error {
                RecvTimeoutError::Timeout => timeout(),
                RecvTimeoutError::Disconnected => {
                    io::Error::other("subprocess I/O worker panicked")
                }
            }),
        None => receiver
            .recv()
            .map_err(|_| io::Error::other("subprocess I/O worker panicked")),
    }
}

fn timeout() -> io::Error {
    io::Error::new(ErrorKind::TimedOut, "subprocess deadline exceeded")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::core::testing::Sandbox;

    #[test]
    fn input_and_output_larger_than_pipes_do_not_deadlock() {
        let root = Sandbox::with_repo();
        let input = vec![b'i'; 256 * 1024];
        let mut command = root.command("sh");
        command.args([
            "-c",
            "dd if=/dev/zero bs=65536 count=4 2>/dev/null; cat; printf error >&2",
        ]);
        let captured = output(&mut command, Some(&input), None).unwrap();
        captured.input.unwrap();
        assert!(captured.output.status.success());
        assert_eq!(captured.output.stdout.len(), 512 * 1024);
        assert_eq!(&captured.output.stdout[256 * 1024..], input);
        assert_eq!(captured.output.stderr, b"error");
    }

    #[test]
    fn a_child_exiting_before_reading_input_keeps_its_exit_status() {
        let root = Sandbox::with_repo();
        let mut command = root.command("sh");
        command.args(["-c", "printf refused >&2; exit 7"]);
        let captured = output(&mut command, Some(&vec![b'x'; 256 * 1024]), None).unwrap();
        captured.input.unwrap();
        assert_eq!(captured.output.status.code(), Some(7));
        assert_eq!(captured.output.stderr, b"refused");
    }

    #[test]
    fn deadlines_cover_inherited_pipes_and_expired_commands_never_start() {
        let root = Sandbox::with_repo();
        let mut expired = root.command("sh");
        expired.args(["-c", "touch should-not-exist"]);
        let error = output(&mut expired, None, Some(Instant::now()))
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        assert!(!root.path().join("repo/should-not-exist").exists());

        let mut held = root.command("sh");
        held.args(["-c", "sleep 60 & printf '%s' \"$!\" > child.pid"]);
        let started = Instant::now();
        let error = output(&mut held, None, Some(started + Duration::from_millis(400)))
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid = fs::read_to_string(root.path().join("repo/child.pid"))
            .unwrap()
            .parse()
            .unwrap();
        for _ in 0..100 {
            if crate::core::agents::read(pid).is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("child {pid} survived the deadline");
    }
}
