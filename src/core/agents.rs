//! Running processes and the live Claude Code sessions recorded for them. `agt cleanup`, `agt ps` and
//! `agt worktree` read them here.

use std::path::{Path, PathBuf};

use crate::core::clock;
use crate::core::harness;
use crate::core::local;
use serde_json::Value;

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

    pub fn cwds() -> std::io::Result<Vec<(i32, std::path::PathBuf)>> {
        cwds_in(std::path::Path::new("/proc"))
    }

    fn cwds_in(root: &std::path::Path) -> std::io::Result<Vec<(i32, std::path::PathBuf)>> {
        let mut all = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
            else {
                continue;
            };
            if let Ok(cwd) = fs::read_link(entry.path().join("cwd")) {
                all.push((pid, cwd));
            }
        }
        Ok(all)
    }

    pub fn cwd(pid: i32) -> Option<std::path::PathBuf> {
        fs::read_link(format!("/proc/{pid}/cwd")).ok()
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn unreadable_processes_are_skipped_but_enumeration_errors_propagate() {
            let root = tempfile::tempdir().unwrap();
            let processes = root.path().join("proc");
            fs::create_dir(&processes).unwrap();
            for pid in [1, 2, 3] {
                fs::create_dir(processes.join(pid.to_string())).unwrap();
            }
            let cwd = root.path().join("worktree");
            fs::create_dir(&cwd).unwrap();
            std::os::unix::fs::symlink(&cwd, processes.join("1/cwd")).unwrap();
            // A vanished cwd and an unreadable cwd MUST NOT hide the readable process.
            fs::write(processes.join("3/cwd"), "not a readable process link").unwrap();
            assert_eq!(cwds_in(&processes).unwrap(), vec![(1, cwd)]);
            assert_eq!(
                cwds_in(&root.path().join("missing")).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );
        }
    }
}

#[cfg(target_os = "macos")]
mod os {
    use std::ffi::CStr;

    use super::Proc;

    pub fn pids() -> Vec<i32> {
        pids_checked().unwrap_or_default()
    }

    fn pids_checked() -> std::io::Result<Vec<i32>> {
        // proc_listallpids returns a PID count, while its buffer size is in bytes.
        // https://github.com/apple-oss-distributions/xnu/blob/main/libsyscall/wrappers/libproc/libproc.c
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut capacity = count as usize + 64;
        loop {
            let mut pids = vec![0i32; capacity];
            let bytes = capacity
                .checked_mul(size_of::<i32>())
                .and_then(|size| i32::try_from(size).ok())
                .ok_or_else(|| std::io::Error::other("process table is too large"))?;
            let found = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
            if found <= 0 {
                return Err(std::io::Error::last_os_error());
            }
            if (found as usize) < capacity {
                pids.truncate(found as usize);
                pids.retain(|pid| *pid > 0);
                return Ok(pids);
            }
            capacity = capacity
                .checked_mul(2)
                .ok_or_else(|| std::io::Error::other("process table is too large"))?;
        }
    }

    pub fn cwds() -> std::io::Result<Vec<(i32, std::path::PathBuf)>> {
        let pids = pids_checked()?;
        let mut all = Vec::new();
        for pid in pids.into_iter().filter(|pid| *pid > 0) {
            let Ok(info) = info_checked(pid) else {
                continue;
            };
            if info.pbi_status == libc::SZOMB {
                continue;
            }
            if let Ok(cwd) = cwd_checked(pid) {
                all.push((pid, cwd));
            }
        }
        Ok(all)
    }

    fn info_checked(pid: i32) -> std::io::Result<libc::proc_bsdinfo> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = size_of::<libc::proc_bsdinfo>() as i32;
        let read = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        checked_size(read, size)?;
        Ok(info)
    }

    fn checked_size(read: i32, size: i32) -> std::io::Result<()> {
        if read <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        if read != size {
            return Err(std::io::Error::other("incomplete process information"));
        }
        Ok(())
    }

    fn info(pid: i32) -> Option<libc::proc_bsdinfo> {
        info_checked(pid).ok()
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

    pub fn cwd(pid: i32) -> Option<std::path::PathBuf> {
        cwd_checked(pid).ok()
    }

    fn cwd_checked(pid: i32) -> std::io::Result<std::path::PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = size_of::<libc::proc_vnodepathinfo>() as i32;
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        checked_size(read, size)?;
        let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
        let path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes()));
        if path.as_os_str().is_empty() {
            return Err(std::io::Error::other("process has no current directory"));
        }
        Ok(path)
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

/// Enumeration errors MUST reach callers that use current directories to permit deletion.
/// Individual processes can disappear or deny access; only readable directories are returned.
pub fn cwds_checked() -> std::io::Result<Vec<(i32, PathBuf)>> {
    os::cwds()
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

pub fn pids() -> Vec<i32> {
    os::pids()
}
/// The directories where Claude Code records live sessions as `<pid>.json`: `sessions` in
/// each Claude Code profile directory.
pub fn session_dirs() -> Vec<PathBuf> {
    // Without a home only `$CLAUDE_CONFIG_DIR` can name a profile; an empty path lists nothing.
    let home = local::home().unwrap_or_default();
    harness::claude_profiles(&home)
        .into_iter()
        .map(|profile| profile.join("sessions"))
        .collect()
}

/// `ps`'s `lstart` text, such as `Fri Oct 16 16:41:54 2026`, as Unix seconds read in local time
/// and read in UTC: `ps` prints local time, Claude Code writes UTC on macOS.
fn parse_lstart(text: &str) -> Option<(i64, i64)> {
    let text = std::ffi::CString::new(text.trim()).ok()?;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let end =
        unsafe { libc::strptime(text.as_ptr(), c"%a %b %d %H:%M:%S %Y".as_ptr(), &raw mut tm) };
    if end.is_null() || unsafe { *end } != 0 {
        return None;
    }
    let mut utc = tm;
    let local = clock::mktime_local(&mut tm);
    let utc = unsafe { libc::timegm(&raw mut utc) };
    (local != -1 && utc != -1).then_some((local, utc))
}

/// Whether a session record describes the running process, by the `procStart` Claude Code
/// wrote: clock ticks since boot on Linux, `ps` `lstart` text on macOS. `None` when the
/// record cannot tell, such as one with no `procStart` or a time in a DST shift.
pub fn record_matches(record: &Value, proc: &Proc) -> Option<bool> {
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
    let (local, utc) = parse_lstart(&text)?;
    if (utc - proc.start).abs() <= 1 {
        return Some(true);
    }
    match (local - proc.start).abs() {
        0 | 1 => Some(true),
        // A local time inside a daylight saving shift can read an hour off.
        3599..=3601 => None,
        _ => Some(false),
    }
}

pub fn exists(pid: i32) -> bool {
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// A running Claude Code session and its record.
pub struct Live {
    pub pid: i32,
    pub cwd: PathBuf,
    pub record: Value,
}

/// Every live Claude Code session. A record counts when its pid runs and its `procStart`
/// matches the process start time.
pub fn live() -> Vec<Live> {
    let mut found = Vec::new();
    for dir in session_dirs() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(pid) = path
                .file_stem()
                .and_then(|stem| stem.to_str()?.parse::<i32>().ok())
                .filter(|_| path.extension().is_some_and(|ext| ext == "json"))
            else {
                continue;
            };
            let Some(record) = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            else {
                continue;
            };
            let Some(cwd) = record.get("cwd").and_then(Value::as_str).map(PathBuf::from) else {
                continue;
            };
            if !read(pid).is_some_and(|proc| record_matches(&record, &proc) == Some(true)) {
                continue;
            }
            found.push(Live { pid, cwd, record });
        }
    }
    found
}

/// This process and its ancestors, nearest first.
pub fn lineage() -> Vec<i32> {
    let mut pids = vec![std::process::id().cast_signed()];
    let mut current = read(pids[0]).map(|proc| proc.ppid);
    while let Some(pid) = current.filter(|pid| *pid > 1 && !pids.contains(pid)) {
        pids.push(pid);
        current = read(pid).map(|proc| proc.ppid);
    }
    pids
}

/// The processes below the calling Claude Code session: every descendant of an ancestor of
/// agt that has a live session record, or of `CLAUDE_PID` when set. Its task servers and
/// shells live there; agt's own ancestors are not in the list.
pub fn session_tree() -> Vec<i32> {
    let lineage = lineage();
    let live_pids: Vec<i32> = live().into_iter().map(|session| session.pid).collect();
    let mut found: Vec<i32> = lineage
        .iter()
        .copied()
        .filter(|pid| live_pids.contains(pid))
        .collect();
    let claude: Option<i32> = std::env::var("CLAUDE_PID")
        .ok()
        .and_then(|value| value.parse().ok());
    found.extend(claude);
    if found.is_empty() {
        return found;
    }
    let table: Vec<(i32, i32)> = pids()
        .into_iter()
        .filter_map(read)
        .map(|proc| (proc.pid, proc.ppid))
        .collect();
    let mut next = 0;
    while next < found.len() {
        let parent = found[next];
        next += 1;
        for (pid, ppid) in &table {
            if *ppid == parent && !found.contains(pid) {
                found.push(*pid);
            }
        }
    }
    found
}

/// The pids of the calling session: this process, its ancestors and `CLAUDE_PID` when set.
pub fn own_pids() -> Vec<i32> {
    let mut pids = lineage();
    let claude: Option<i32> = std::env::var("CLAUDE_PID")
        .ok()
        .and_then(|value| value.parse().ok());
    pids.extend(claude);
    pids
}

/// The first live Claude Code session, other than the calling one, whose `cwd` is `root` or
/// inside it.
pub fn live_in(root: &Path) -> Option<Live> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    let own = own_pids();
    live()
        .into_iter()
        .filter(|session| !own.contains(&session.pid))
        .find(|session| inside(&session.cwd, &root))
}

/// Whether `path`, links resolved, is `root` or inside it; `root` is already canonical.
pub fn inside(path: &Path, root: &Path) -> bool {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_owned())
        .starts_with(root)
}

/// A running `codex` process and its working directory, when the platform lets it be read.
pub struct CodexProc {
    pub proc: Proc,
    pub cwd: Option<PathBuf>,
}

pub fn codex_procs() -> Vec<CodexProc> {
    pids()
        .into_iter()
        .filter_map(read)
        .filter(|proc| proc.comm == "codex")
        .map(|proc| CodexProc {
            cwd: os::cwd(proc.pid),
            proc,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_started(start: i64) -> Proc {
        Proc {
            pid: 1,
            ppid: 0,
            start,
            start_key: 0,
            comm: String::new(),
            cmd: String::new(),
            claude_pid: None,
        }
    }

    /// `lstart` text for `secs`, in UTC or local time.
    fn lstart(secs: i64, utc: bool) -> String {
        let mut buffer = [0 as libc::c_char; 64];
        let mut tm = clock::local_tm(secs);
        unsafe {
            if utc {
                libc::gmtime_r(&secs, &raw mut tm);
            }
            libc::strftime(
                buffer.as_mut_ptr(),
                buffer.len(),
                c"%a %b %d %H:%M:%S %Y".as_ptr(),
                &raw const tm,
            );
            std::ffi::CStr::from_ptr(buffer.as_ptr())
                .to_string_lossy()
                .into_owned()
        }
    }

    #[test]
    fn a_proc_start_in_utc_or_local_time_matches_the_process() {
        let start = 1_792_282_349;
        let proc = proc_started(start);
        for utc in [true, false] {
            let record = serde_json::json!({ "procStart": lstart(start, utc) });
            assert_eq!(record_matches(&record, &proc), Some(true), "utc: {utc}");
        }
        let other = serde_json::json!({ "procStart": lstart(start - 7200 - 600, true) });
        assert_eq!(record_matches(&other, &proc), Some(false));
    }
}
