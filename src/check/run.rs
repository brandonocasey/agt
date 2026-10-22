//! Running the command with its output teed to stderr and a log file.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::core::error::{Result, ctx, fail};
use crate::core::{clock, local};

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
    let streams = [
        child.stdout.take().map(|out| tee(out, &sink)),
        child.stderr.take().map(|err| tee(err, &sink)),
    ];
    let status = child.wait().map_err(ctx(format!("wait for {}", argv[0])))?;
    for stream in streams.into_iter().flatten() {
        if stream.join().is_err() {
            return fail("copying the command output failed");
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

fn tee(
    mut from: impl Read + Send + 'static,
    sink: &Arc<Mutex<File>>,
) -> std::thread::JoinHandle<()> {
    let sink = Arc::clone(sink);
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        while let Ok(read) = from.read(&mut buffer) {
            if read == 0 {
                break;
            }
            let chunk = &buffer[..read];
            let _ = std::io::stderr().lock().write_all(chunk);
            if let Ok(mut file) = sink.lock() {
                let _ = file.write_all(chunk);
            }
        }
    })
}
