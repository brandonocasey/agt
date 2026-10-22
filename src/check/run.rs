//! Running the command with its output teed to stderr and a log file.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::core::error::{Result, ctx, fail};
use crate::core::{clock, local};

/// How long after the command exits its remaining output is still copied.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

pub(super) struct Outcome {
    pub code: i32,
    pub ran_at: String,
    pub duration_ms: u64,
    pub log: PathBuf,
}

/// Runs `argv` with inherited stdin, copying stdout and stderr to our stderr (stdout stays for
/// the JSON summary) and to a new `check-<name>-` log in the agt cache `output` directory.
pub(super) fn execute(name: &str, argv: &[String]) -> Result<Outcome> {
    let dir = local::agents_cache()?.join("output");
    std::fs::create_dir_all(&dir).map_err(ctx(format!("create {}", dir.display())))?;
    let label: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(60)
        .collect();
    let log = dir.join(format!(
        "check-{label}-{}-{}.log",
        std::process::id(),
        clock::unix_nanos()
    ));
    let sink = File::create(&log).map_err(ctx(format!("create {}", log.display())))?;
    let sink = Arc::new(Mutex::new(sink));
    let ran_at = clock::now()?;
    let started = Instant::now();
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(ctx(format!("run {}", argv[0])))?;
    let (done, finished) = mpsc::channel();
    let streams = [
        child.stdout.take().map(|out| tee(out, &sink, &done)),
        child.stderr.take().map(|err| tee(err, &sink, &done)),
    ];
    drop(done);
    let status = child.wait().map_err(ctx(format!("wait for {}", argv[0])))?;
    // A descendant that outlives the command keeps the pipes open; its output after the grace
    // period is dropped rather than waited for.
    let deadline = Instant::now() + DRAIN_GRACE;
    for _ in streams.iter().flatten() {
        let left = deadline.saturating_duration_since(Instant::now());
        match finished.recv_timeout(left) {
            Ok(true) => {}
            Ok(false) => return fail("copying the command output failed"),
            Err(_) => break,
        }
    }
    let code = status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1);
    Ok(Outcome {
        code,
        ran_at,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        log,
    })
}

/// Copies `from` until it closes, then reports on `done` whether it ended without a read error.
fn tee(
    mut from: impl Read + Send + 'static,
    sink: &Arc<Mutex<File>>,
    done: &mpsc::Sender<bool>,
) -> std::thread::JoinHandle<()> {
    let sink = Arc::clone(sink);
    let done = done.clone();
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        let ok = loop {
            match from.read(&mut buffer) {
                Ok(0) => break true,
                Ok(read) => {
                    let chunk = &buffer[..read];
                    let _ = std::io::stderr().lock().write_all(chunk);
                    if let Ok(mut file) = sink.lock() {
                        let _ = file.write_all(chunk);
                    }
                }
                Err(_) => break false,
            }
        };
        let _ = done.send(ok);
    })
}
