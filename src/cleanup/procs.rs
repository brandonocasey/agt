//! Processes that agent sessions started and left behind. A tool that a Claude Code session
//! runs inherits `CLAUDE_PID`; once that session is gone and the process is a day old, it is
//! an orphan. Only the user's own processes are read. On macOS the environment of Apple's own
//! binaries (such as `/bin/sleep`) is hidden, so those are never found.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::core::error::{Error, Result};

pub const MIN_AGE_SECS: i64 = 86_400;
const GRACE: Duration = Duration::from_secs(10);
const PID_OVERRIDE: &str = "AGT_CLEANUP_PID";

#[derive(Clone)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    /// Unix seconds.
    pub start: i64,
    /// The start time in the OS's own finest unit, to tell a reused pid apart.
    pub start_key: u64,
    pub comm: String,
    pub cmd: String,
    pub claude_pid: Option<i32>,
}

pub struct Orphan {
    pub proc: Proc,
    pub age: i64,
}

#[cfg(target_os = "linux")]
mod os {
    use std::fs;

    use super::Proc;

    fn boot_time() -> Option<i64> {
        let stat = fs::read_to_string("/proc/stat").ok()?;
        stat.lines()
            .find_map(|line| line.strip_prefix("btime "))
            .and_then(|value| value.trim().parse().ok())
    }

    pub fn pids() -> Vec<i32> {
        let uid = unsafe { libc::getuid() };
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let pid = entry.file_name().to_str()?.parse().ok()?;
                use std::os::unix::fs::MetadataExt;
                (entry.metadata().ok()?.uid() == uid).then_some(pid)
            })
            .collect()
    }

    pub fn read(pid: i32) -> Option<Proc> {
        use std::os::unix::fs::MetadataExt;
        let dir = format!("/proc/{pid}");
        if fs::metadata(&dir).ok()?.uid() != unsafe { libc::getuid() } {
            return None;
        }
        let stat = fs::read_to_string(format!("{dir}/stat")).ok()?;
        // The command name may hold spaces and parentheses; fields resume after the last `)`.
        let fields: Vec<&str> = stat.get(stat.rfind(')')? + 2..)?.split(' ').collect();
        // An exited process waiting for its parent to reap it is gone, not running.
        if fields.first() == Some(&"Z") {
            return None;
        }
        let ppid = fields.get(1)?.parse().ok()?;
        let ticks: u64 = fields.get(19)?.parse().ok()?;
        let per_sec = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) })
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or(100);
        let start = boot_time()? + i64::try_from(ticks / per_sec).ok()?;
        let environ = fs::read(format!("{dir}/environ")).unwrap_or_default();
        let claude_pid = environ
            .split(|byte| *byte == 0)
            .find_map(|var| var.strip_prefix(b"CLAUDE_PID="))
            .and_then(|value| std::str::from_utf8(value).ok()?.parse().ok());
        let cmd = fs::read(format!("{dir}/cmdline")).unwrap_or_default();
        Some(Proc {
            pid,
            ppid,
            start,
            start_key: ticks,
            comm: fs::read_to_string(format!("{dir}/comm"))
                .unwrap_or_default()
                .trim_end()
                .to_owned(),
            cmd: super::command_line(cmd.split(|byte| *byte == 0)),
            claude_pid,
        })
    }
}

#[cfg(target_os = "macos")]
mod os {
    use std::ffi::CStr;

    use super::Proc;

    pub fn pids() -> Vec<i32> {
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        let Ok(count) = usize::try_from(count) else {
            return Vec::new();
        };
        // Room for processes started between the two calls.
        let mut pids = vec![0i32; count + 64];
        let bytes = i32::try_from(pids.len() * size_of::<i32>()).unwrap_or(i32::MAX);
        let found = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        pids.truncate(usize::try_from(found).unwrap_or(0));
        pids.retain(|pid| *pid > 0);
        pids
    }

    fn info(pid: i32) -> Option<libc::proc_bsdinfo> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = i32::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
        let read = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        (read == size).then_some(info)
    }

    /// argv and environment from `KERN_PROCARGS2`: argc, the exec path, padding, argv, env.
    fn args(pid: i32) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let mut max: libc::c_int = 0;
        let mut size = size_of::<libc::c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        let ok = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&raw mut max).cast(),
                &raw mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        let Ok(max) = usize::try_from(max) else {
            return (Vec::new(), Vec::new());
        };
        if ok != 0 || max == 0 {
            return (Vec::new(), Vec::new());
        }
        let mut buffer = vec![0u8; max];
        let mut size = max;
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let ok = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buffer.as_mut_ptr().cast(),
                &raw mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if ok != 0 || size < 4 {
            return (Vec::new(), Vec::new());
        }
        buffer.truncate(size);
        let argc = usize::try_from(i32::from_ne_bytes([
            buffer[0], buffer[1], buffer[2], buffer[3],
        ]))
        .unwrap_or(0);
        let mut parts = buffer[4..]
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(<[u8]>::to_vec);
        let _exec_path = parts.next();
        let argv: Vec<Vec<u8>> = parts.by_ref().take(argc).collect();
        (argv, parts.collect())
    }

    pub fn read(pid: i32) -> Option<Proc> {
        let info = info(pid)?;
        // An exited process waiting for its parent to reap it is gone, not running.
        if info.pbi_status == libc::SZOMB {
            return None;
        }
        if info.pbi_uid != unsafe { libc::getuid() } {
            return None;
        }
        let (argv, env) = args(pid);
        let claude_pid = env
            .iter()
            .find_map(|var| var.strip_prefix(b"CLAUDE_PID="))
            .and_then(|value| std::str::from_utf8(value).ok()?.parse().ok());
        let comm = unsafe { CStr::from_ptr(info.pbi_comm.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        Some(Proc {
            pid,
            ppid: i32::try_from(info.pbi_ppid).ok()?,
            start: i64::try_from(info.pbi_start_tvsec).ok()?,
            start_key: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
            comm,
            cmd: super::command_line(argv.iter().map(Vec::as_slice)),
            claude_pid,
        })
    }
}

fn command_line<'a>(parts: impl Iterator<Item = &'a [u8]>) -> String {
    let joined = parts
        .filter(|part| !part.is_empty())
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>()
        .join(" ");
    joined.chars().take(80).collect()
}

pub fn read(pid: i32) -> Option<Proc> {
    os::read(pid)
}

/// The directories where Claude Code records live sessions as `<pid>.json`: `sessions` in
/// `~/.claude`, in every other `~/.claude*` config directory, and in `$CLAUDE_CONFIG_DIR`.
fn session_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(config) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        dirs.push(PathBuf::from(config).join("sessions"));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty())
        && let Ok(entries) = std::fs::read_dir(&home)
    {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(".claude") {
                dirs.push(entry.path().join("sessions"));
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

#[derive(PartialEq, Eq)]
enum Session {
    Alive,
    Gone,
    /// The pid still runs but no record says whose it is, or a record agt cannot read:
    /// leave its tools alone.
    Unknown,
}

/// `ps`'s `lstart` text, such as `Fri Oct 16 16:41:54 2026`, in local time, as Unix seconds.
fn parse_lstart(text: &str) -> Option<i64> {
    let text = std::ffi::CString::new(text.trim()).ok()?;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let end =
        unsafe { libc::strptime(text.as_ptr(), c"%a %b %d %H:%M:%S %Y".as_ptr(), &raw mut tm) };
    if end.is_null() || unsafe { *end } != 0 {
        return None;
    }
    tm.tm_isdst = -1;
    let secs = unsafe { libc::mktime(&raw mut tm) };
    (secs != -1).then_some(secs)
}

/// Whether a session record describes the running process, by the `procStart` Claude Code
/// wrote: clock ticks since boot on Linux, `ps` `lstart` text on macOS. `None` when the
/// record cannot tell, such as one with no `procStart` or a time in a DST shift.
fn record_matches(record: &Value, proc: &Proc) -> Option<bool> {
    let value = record.get("procStart")?;
    let text = value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|n| n.to_string()))?;
    if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) {
        if !cfg!(target_os = "linux") {
            return None;
        }
        return Some(text.parse::<u64>().ok()? == proc.start_key);
    }
    let diff = (parse_lstart(&text)? - proc.start).abs();
    match diff {
        0 | 1 => Some(true),
        // A local time inside a daylight saving shift can read an hour off.
        3599..=3601 => None,
        _ => Some(false),
    }
}

/// Whether Claude process `pid` is still the session that started the tool. Its record
/// (`<pid>.json` in any config directory) goes away with the session, and its `procStart`
/// tells a reused pid apart. A session is gone only when its pid no longer runs, or every
/// record clearly names another process.
fn session(pid: i32) -> Session {
    let records: Vec<Value> = session_dirs()
        .into_iter()
        .map(|dir| dir.join(format!("{pid}.json")))
        .filter(|path| path.is_file())
        .map(|path| {
            std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or(Value::Null)
        })
        .collect();
    let Some(proc) = read(pid) else {
        // Not ours to read (another user's) or not running at all.
        return if exists(pid) {
            Session::Unknown
        } else {
            Session::Gone
        };
    };
    if records.is_empty() {
        return Session::Unknown;
    }
    let mut unsure = false;
    for record in &records {
        match record_matches(record, &proc) {
            Some(true) => return Session::Alive,
            Some(false) => {}
            None => unsure = true,
        }
    }
    if unsure {
        Session::Unknown
    } else {
        Session::Gone
    }
}

fn exists(pid: i32) -> bool {
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// This process and its ancestors, which a sweep never signals.
fn lineage() -> Vec<i32> {
    let mut pids = vec![std::process::id().cast_signed()];
    let mut current = read(pids[0]).map(|proc| proc.ppid);
    while let Some(pid) = current.filter(|pid| *pid > 1 && !pids.contains(pid)) {
        pids.push(pid);
        current = read(pid).map(|proc| proc.ppid);
    }
    pids
}

/// Harness processes themselves, which carry `CLAUDE_PID` too; versioned Claude installs run
/// under their version number, such as `2.1.302`.
fn is_harness(comm: &str) -> bool {
    matches!(comm, "claude" | "codex" | "node-claude")
        || (comm.split('.').count() == 3
            && comm
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())))
}

/// Orphans at least `MIN_AGE_SECS` old. The test-only `AGT_CLEANUP_PID` limits the sweep to
/// one pid, so tests never see the machine's own processes; `0` looks at none.
pub fn orphans(now: i64) -> Result<Vec<Orphan>> {
    let skip = lineage();
    let pids = match std::env::var(PID_OVERRIDE) {
        Ok(value) => {
            let pid: i32 = value
                .parse()
                .map_err(|_| Error(format!("{PID_OVERRIDE} must be a pid, got {value}")))?;
            if pid > 0 { vec![pid] } else { Vec::new() }
        }
        Err(_) => os::pids(),
    };
    let mut found = Vec::new();
    for pid in pids {
        if skip.contains(&pid) {
            continue;
        }
        let Some(proc) = read(pid) else { continue };
        let Some(claude) = proc.claude_pid.filter(|claude| *claude != pid) else {
            continue;
        };
        if is_harness(&proc.comm) || session(claude) != Session::Gone {
            continue;
        }
        let age = now - proc.start;
        if age >= MIN_AGE_SECS {
            found.push(Orphan { proc, age });
        }
    }
    found.sort_by_key(|orphan| orphan.proc.pid);
    Ok(found)
}

fn same(proc: &Proc) -> bool {
    read(proc.pid).is_some_and(|now| now.start_key == proc.start_key)
}

/// Sends SIGTERM to each process that is still the one planned, waits up to ten seconds, then
/// sends SIGKILL to those still running. Returns, per process, whether it was signalled.
pub fn stop(procs: &[Proc]) -> Vec<(Proc, std::result::Result<&'static str, String>)> {
    let mut results = Vec::new();
    let mut waiting = Vec::new();
    for proc in procs {
        if !same(proc) {
            results.push((
                proc.clone(),
                Err("already gone or its pid was reused".into()),
            ));
        } else if unsafe { libc::kill(proc.pid, libc::SIGTERM) } == 0 {
            waiting.push(proc.clone());
        } else {
            let error = std::io::Error::last_os_error();
            results.push((proc.clone(), Err(format!("SIGTERM: {error}"))));
        }
    }
    let deadline = Instant::now() + GRACE;
    while waiting.iter().any(same) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    for proc in waiting {
        let signal = if same(&proc) && unsafe { libc::kill(proc.pid, libc::SIGKILL) } == 0 {
            "SIGKILL"
        } else {
            "SIGTERM"
        };
        results.push((proc, Ok(signal)));
    }
    results
}

pub fn reason(orphan: &Orphan) -> String {
    format!(
        "Claude session {} is gone; running {} day(s): {}",
        orphan.proc.claude_pid.unwrap_or_default(),
        orphan.age / 86_400,
        orphan.proc.cmd
    )
}
