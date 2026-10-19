use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
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
    if result.is_err() {
        if deadline.is_some()
            && let Ok(pid) = i32::try_from(child.id())
        {
            // SAFETY: the child leads this group, so only its descendants receive the signal.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn collect(
    child: &mut Child,
    input: Option<Receiver<io::Result<()>>>,
    stdout: Receiver<io::Result<Vec<u8>>>,
    stderr: Receiver<io::Result<Vec<u8>>>,
    deadline: Option<Instant>,
) -> io::Result<Captured> {
    let status = match deadline {
        None => child.wait()?,
        Some(end) => loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            let remaining = end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(timeout());
            }
            std::thread::sleep(Duration::from_millis(5).min(remaining));
        },
    };
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

    fn command(root: &std::path::Path, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root.join("home"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("AGT_GH_BIN", root.join("no-forge"))
            .env("AGT_GLAB_BIN", root.join("no-forge"))
            .current_dir(root.join("repo"));
        command
    }

    fn environment() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for dir in ["home", "state", "config", "cache", "repo"] {
            fs::create_dir(root.path().join(dir)).unwrap();
        }
        assert!(
            command(root.path(), "git")
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        root
    }

    #[test]
    fn input_and_output_larger_than_pipes_do_not_deadlock() {
        let root = environment();
        let input = vec![b'i'; 256 * 1024];
        let mut command = command(root.path(), "sh");
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
        let root = environment();
        let mut command = command(root.path(), "sh");
        command.args(["-c", "printf refused >&2; exit 7"]);
        let captured = output(&mut command, Some(&vec![b'x'; 256 * 1024]), None).unwrap();
        captured.input.unwrap();
        assert_eq!(captured.output.status.code(), Some(7));
        assert_eq!(captured.output.stderr, b"refused");
    }

    #[test]
    fn deadlines_cover_inherited_pipes_and_expired_commands_never_start() {
        let root = environment();
        let mut expired = command(root.path(), "sh");
        expired.args(["-c", "touch should-not-exist"]);
        let error = output(&mut expired, None, Some(Instant::now()))
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        assert!(!root.path().join("repo/should-not-exist").exists());

        let mut held = command(root.path(), "sh");
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
