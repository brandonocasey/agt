//! `agents`: what spawning sub-agents costs a session. Per harness it reports the spawns (prompt
//! tokens out, result tokens back), the files a parent and its sub-agents both read, and the
//! prompt lines repeated across spawns.
//!
//! Claude Code spawns with the `Agent` (older: `Task`) tool, whose result is the tool result.
//! Codex spawns with `spawn_agent`, whose prompt is encrypted in the rollout (its size is
//! estimated from the ciphertext, and it has no lines to compare) and whose reply arrives in a
//! `<subagent_notification>` message (older rollouts) or an `agent_message` from the child (newer
//! ones, encrypted), which the record adapter drops; both are read here from the file.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::core::harness::Harness;
use crate::core::transcripts::Transcript;
use crate::core::transcripts::json::{get, head, key, text};
use crate::sessions::fmt::{self, tok};
use crate::sessions::source::Loaded;
use crate::sessions::tools::{bare_command, est_text_tokens};
use crate::sessions::{Scope, Which};

/// A prompt line is worth comparing from this many characters.
const MIN_LINE: usize = 40;
/// A line is repeated when this many spawn prompts hold it.
const MIN_PROMPTS: i64 = 3;
/// Characters of a repeated line that are printed.
const SHOWN: usize = 120;

/// Whether `name` is the tool that spawns a sub-agent.
fn is_spawn(name: &str) -> bool {
    matches!(name, "Agent" | "Task" | "spawn_agent")
}

/// A prompt line as compared: trimmed, with runs of whitespace collapsed.
fn normalize(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The distinct normalized lines of a prompt that are long enough to compare.
fn long_lines(prompt: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    prompt
        .lines()
        .map(normalize)
        .filter(|line| line.chars().count() >= MIN_LINE && seen.insert(line.clone()))
        .collect()
}

/// Tokens of a Codex prompt, which the rollout stores as a Fernet token: base64 over a 57 byte
/// envelope and the padded text.
fn encrypted_tokens(message: &str) -> f64 {
    (message.len() as f64 * 0.75 - 57.0).max(0.0) / 4.0
}

/// The file a shell command reads alone: one of `cat`, `head`, `tail`, `sed` (printing) or
/// `rg`/`grep` given a single absolute or `~/` path. Pipes, redirects and substitutions make the
/// command too hard to attribute, so they give `None`.
fn shell_read(command: &str, home: &Path) -> Option<String> {
    let command = bare_command(command);
    if command.contains(['|', ';', '&', '>', '<', '`', '$', '(', ')', '\n', '*']) {
        return None;
    }
    let words: Vec<&str> = command
        .split_whitespace()
        .map(|word| word.trim_matches(['\'', '"']))
        .collect();
    let (first, args) = words.split_first()?;
    let tool = first.rsplit('/').next().unwrap_or(first);
    let takes_value: &[&str] = match tool {
        "head" | "tail" => &["-n", "-c"],
        "sed" => &["-e", "-f"],
        "rg" | "grep" => &["-g", "-t", "-T", "-A", "-B", "-C", "-m", "-j", "-e", "-f"],
        "cat" => &[],
        _ => return None,
    };
    let mut plain = Vec::new();
    let mut scripted = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if *arg == "--in-place" || (tool == "sed" && arg.starts_with("-i")) {
            return None;
        }
        if takes_value.contains(arg) {
            scripted |= matches!(*arg, "-e" | "-f");
            args.next();
        } else if arg.starts_with('-') && arg.len() > 1 {
            continue;
        } else {
            plain.push(*arg);
        }
    }
    // A script or a pattern comes before the file, unless an option already gave it.
    let wants_pattern = matches!(tool, "sed" | "rg" | "grep") && !scripted;
    let path = match (plain.as_slice(), wants_pattern) {
        ([_, path], true) | ([path], false) => *path,
        _ => return None,
    };
    if matches!(tool, "rg" | "grep")
        && !path
            .rsplit('/')
            .next()
            .is_some_and(|name| name.contains('.'))
    {
        return None;
    }
    match path.strip_prefix("~/") {
        Some(rest) => Some(format!("{}/{rest}", home.display())),
        None if path.starts_with('/') => Some(path.to_owned()),
        None => None,
    }
}

/// The file a tool call reads, for the tools whose input names it.
fn read_path(name: &str, input: &Value, home: &Path) -> Option<String> {
    match name {
        "Read" => Some(text(input, "file_path").to_owned()).filter(|path| !path.is_empty()),
        "Bash" => shell_read(text(input, "command"), home),
        _ => None,
    }
}

/// Adds what `agents` needs from a `tool_use` block to its summary: the call name, the file it
/// reads, and for a spawn its agent type, task, prompt size and repeatable lines.
pub fn compact_call(block: &Value, home: &Path, summary: &mut Value) {
    let name = text(block, "name");
    let input = get(block, "input");
    summary["name"] = name.into();
    if let Some(path) = read_path(name, input, home) {
        summary["read_path"] = path.into();
    }
    if !is_spawn(name) {
        return;
    }
    let codex = name == "spawn_agent";
    let (kind, prompt) = if codex {
        (text(input, "agent_type"), text(input, "message"))
    } else {
        (text(input, "subagent_type"), text(input, "prompt"))
    };
    let kind = if kind.is_empty() {
        if codex { "default" } else { "general-purpose" }
    } else {
        kind
    };
    let encrypted = prompt.starts_with("gAAAA");
    let (tokens, lines) = if encrypted {
        (encrypted_tokens(prompt), Vec::new())
    } else {
        (est_text_tokens(prompt), long_lines(prompt))
    };
    summary["spawn"] = json!({
        "agent": kind,
        "task": text(input, "task_name"),
        "prompt_tokens": tokens,
        "lines": lines,
        "encrypted": encrypted,
    });
}

/// The full agent path (`/root/<task>`) a Codex `spawn_agent` result names, if `content` is one.
pub fn task_path(content: &Value) -> Option<String> {
    let body = content
        .as_str()
        .or_else(|| content.as_array()?.first().map(|part| text(part, "text")))?;
    let reply: Value = serde_json::from_str(body).ok()?;
    Some(text(&reply, "task_name").to_owned()).filter(|path| !path.is_empty())
}

/// A background sub-agent's reply to a Claude Code parent, which Claude Code queues as an
/// attachment or a user message holding a `<task-notification>` that names the spawning call.
pub fn claude_notice(record: &Value) -> Option<Value> {
    let message = get(get(record, "message"), "content");
    let body = match text(record, "type") {
        "attachment" if text(get(record, "attachment"), "type") == "queued_command" => {
            text(get(record, "attachment"), "prompt")
        }
        "user" => message.as_str().or_else(|| {
            message
                .as_array()?
                .iter()
                .map(|part| text(part, "text"))
                .find(|part| part.starts_with("<task-notification>"))
        })?,
        _ => return None,
    };
    if !body.starts_with("<task-notification>") {
        return None;
    }
    let call = body
        .split_once("<tool-use-id>")?
        .1
        .split_once("</tool-use-id>")?
        .0;
    Some(json!({
        "type": "notice",
        "notice": {"tool_use_id": call, "tokens": est_text_tokens(body)},
    }))
}

/// A sub-agent's reply to a Codex parent, naming the child by thread id.
pub struct Notice {
    pub agent: String,
    pub tokens: f64,
}

/// What a Codex rollout says that the record adapter drops: the replies its sub-agents sent it,
/// and for a sub-agent's own rollout, its thread id and agent path, which join a reply to the
/// spawn call (`task_name`) that started it.
#[derive(Default)]
pub struct Links {
    notices: Vec<Notice>,
    child: Option<(String, String)>,
    /// For the rollout of an agent that a parent spawned: its agent path, empty when unrecorded.
    spawned: Option<String>,
}

/// A newer rollout's `agent_message` from a descendant to its ancestor, whose body is encrypted.
fn message_of(record: &Value) -> Option<Notice> {
    let payload = get(record, "payload");
    let (author, recipient) = (text(payload, "author"), text(payload, "recipient"));
    if text(payload, "type") != "agent_message"
        || !author
            .strip_prefix(recipient)
            .is_some_and(|rest| rest.starts_with('/'))
    {
        return None;
    }
    let tokens = get(payload, "content")
        .as_array()?
        .iter()
        .map(|part| match text(part, "type") {
            "encrypted_content" => encrypted_tokens(text(part, "encrypted_content")),
            _ => est_text_tokens(text(part, "text")),
        })
        .sum();
    Some(Notice {
        agent: author.to_owned(),
        tokens,
    })
}

fn stamped_since(record: &Value, since: &str) -> bool {
    since.is_empty()
        || get(record, "timestamp")
            .as_str()
            .is_none_or(|stamp| stamp >= since)
}

fn notice_of(record: &Value) -> Vec<Notice> {
    let mut found = Vec::new();
    for part in get(get(record, "payload"), "content")
        .as_array()
        .into_iter()
        .flatten()
    {
        let message = text(part, "text");
        let Some(body) = message
            .strip_prefix("<subagent_notification>")
            .and_then(|body| body.trim().strip_suffix("</subagent_notification>"))
        else {
            continue;
        };
        let Ok(body) = serde_json::from_str::<Value>(body.trim()) else {
            continue;
        };
        found.push(Notice {
            agent: text(&body, "agent_path").to_owned(),
            tokens: est_text_tokens(message),
        });
    }
    found
}

pub fn links(transcript: &Transcript, since: &str) -> Links {
    let mut links = Links::default();
    if transcript.harness != Harness::Codex {
        return links;
    }
    let Ok(file) = File::open(&transcript.path) else {
        return links;
    };
    for (at, line) in BufReader::new(file)
        .split(b'\n')
        .map_while(Result::ok)
        .enumerate()
    {
        let line = String::from_utf8_lossy(&line);
        if at == 0 {
            if let Ok(first) = serde_json::from_str::<Value>(&line) {
                let payload = get(&first, "payload");
                let path = text(
                    get(get(get(payload, "source"), "subagent"), "thread_spawn"),
                    "agent_path",
                );
                let spawn = get(get(get(payload, "source"), "subagent"), "thread_spawn");
                // A deeper child groups under its parent agent's thread, which holds none of that
                // agent's records, so counting it there would repeat the parent agent's own spawn
                // call, already counted in the root session.
                let direct = get(spawn, "depth").as_i64().is_none_or(|depth| depth <= 1);
                if spawn.is_object() && direct {
                    links.spawned = Some(path.to_owned());
                }
                if !path.is_empty() {
                    links.child = Some((text(payload, "id").to_owned(), path.to_owned()));
                }
            }
        } else if (line.contains("<subagent_notification>") || line.contains("\"agent_message\""))
            && let Ok(record) = serde_json::from_str::<Value>(&line)
            && stamped_since(&record, since)
        {
            links.notices.extend(notice_of(&record));
            links.notices.extend(message_of(&record));
        }
    }
    links
}

impl Stats {
    fn add_result(&mut self, tokens: f64) {
        self.results += 1;
        self.result += tokens;
        self.result_max = self.result_max.max(tokens);
    }
}

#[derive(Default)]
struct Stats {
    spawns: i64,
    /// Spawns whose prompt was read; spawns seen only through a child rollout have none.
    prompts: i64,
    prompt: f64,
    prompt_max: f64,
    results: i64,
    result: f64,
    result_max: f64,
}

/// Reads of one file: how many, their summed tokens and the largest.
#[derive(Default, Clone, Copy)]
struct Reads {
    count: i64,
    total: f64,
    max: f64,
}

#[derive(Default)]
struct Session {
    parent: HashMap<String, Reads>,
    agents: HashMap<String, Reads>,
    spawns: Vec<Spawn>,
    notices: Vec<Notice>,
    children: HashMap<String, String>,
    /// Agents whose own rollouts name this session as the parent: their type and agent path.
    spawned: Vec<(String, String)>,
}

#[derive(Default)]
struct Side {
    types: IndexMap<String, Stats>,
    lines: HashMap<String, i64>,
    encrypted: i64,
}

#[derive(Default)]
struct Spawn {
    agent: String,
    task: String,
    /// The full agent path from the spawn's result, when it names one.
    path: String,
    result: Option<f64>,
}

pub struct Agents {
    sides: IndexMap<&'static str, Side>,
    sessions: HashMap<(&'static str, String, String), Session>,
    seen: HashSet<(&'static str, String)>,
}

impl Agents {
    pub fn new() -> Self {
        Self {
            sides: IndexMap::new(),
            sessions: HashMap::new(),
            seen: HashSet::new(),
        }
    }

    pub fn feed(&mut self, transcript: &Loaded) {
        let id = transcript.harness.id();
        let parent = transcript.agent == "main";
        let side = self.sides.entry(id).or_default();
        let session = self
            .sessions
            .entry((id, transcript.project.clone(), transcript.session.clone()))
            .or_default();
        let mut spawns: Vec<Spawn> = Vec::new();
        let mut spawn_at: HashMap<String, usize> = HashMap::new();
        let mut waiting_read: HashMap<String, String> = HashMap::new();
        transcript.records(|record| {
            let notice = get(record, "notice");
            if notice.is_object()
                && let Some(&at) = spawn_at.get(&key(get(notice, "tool_use_id")))
            {
                let tokens = get(notice, "tokens").as_f64().unwrap_or_default();
                *spawns[at].result.get_or_insert(0.0) += tokens;
            }
            if text(record, "type") == "notice" {
                return;
            }
            let Some(blocks) = get(get(record, "message"), "content").as_array() else {
                return;
            };
            for block in blocks {
                let call = key(get(block, "id"));
                match text(block, "type") {
                    // Resumed sessions copy earlier records into a new file.
                    "tool_use" if self.seen.insert((id, call.clone())) => {
                        let spawn = get(block, "spawn");
                        if spawn.is_object() {
                            let prompt = get(spawn, "prompt_tokens").as_f64().unwrap_or_default();
                            let stats = side
                                .types
                                .entry(text(spawn, "agent").to_owned())
                                .or_default();
                            stats.spawns += 1;
                            stats.prompts += 1;
                            stats.prompt += prompt;
                            stats.prompt_max = stats.prompt_max.max(prompt);
                            let lines = get(spawn, "lines").as_array().into_iter().flatten();
                            for line in lines {
                                *side
                                    .lines
                                    .entry(line.as_str().unwrap_or_default().to_owned())
                                    .or_default() += 1;
                            }
                            side.encrypted += i64::from(get(spawn, "encrypted") == true);
                            spawn_at.insert(call.clone(), spawns.len());
                            spawns.push(Spawn {
                                agent: text(spawn, "agent").to_owned(),
                                task: text(spawn, "task").to_owned(),
                                path: String::new(),
                                result: None,
                            });
                        }
                        if let Some(path) = get(block, "read_path").as_str() {
                            waiting_read.insert(call, path.to_owned());
                        }
                    }
                    "tool_result" => {
                        let tokens = get(block, "result_tokens").as_f64().unwrap_or_default();
                        if let Some(&at) = spawn_at.get(&call_of(block)) {
                            if id == "claude" {
                                *spawns[at].result.get_or_insert(0.0) += tokens;
                            } else if let Some(path) = get(block, "task_path").as_str() {
                                spawns[at].path = path.to_owned();
                            }
                        }
                        if let Some(path) = waiting_read.remove(&call_of(block)) {
                            let reads = if parent {
                                &mut session.parent
                            } else {
                                &mut session.agents
                            };
                            let entry = reads.entry(path).or_default();
                            entry.count += 1;
                            entry.total += tokens;
                            entry.max = entry.max.max(tokens);
                        }
                    }
                    _ => {}
                }
            }
        });
        if id == "claude" {
            for spawn in spawns {
                if let Some(tokens) = spawn.result {
                    side.types
                        .entry(spawn.agent)
                        .or_default()
                        .add_result(tokens);
                }
            }
        } else {
            let links = transcript.links();
            session.spawns.extend(spawns);
            session.notices.extend(links.notices.iter().map(|n| Notice {
                agent: n.agent.clone(),
                tokens: n.tokens,
            }));
            if let Some(path) = &links.spawned {
                session
                    .spawned
                    .push((transcript.agent.clone(), path.clone()));
            }
            if let Some((child, path)) = &links.child {
                session.children.insert(child.clone(), path.clone());
            }
        }
    }

    /// Adds each Codex reply to the spawn whose task it answers: the reply names the child by
    /// thread id, and the child's own rollout names its agent path, which ends in the task.
    fn join_replies(&mut self) {
        for ((harness, ..), session) in &mut self.sessions {
            let side = self.sides.entry(harness).or_default();
            // Older rollouts spawn from an `exec` script, so only the children's own rollouts
            // show the spawns; their prompts are not read.
            if session.spawns.is_empty() {
                for (agent, path) in std::mem::take(&mut session.spawned) {
                    let stats = side.types.entry(agent.clone()).or_default();
                    stats.spawns += 1;
                    session.spawns.push(Spawn {
                        agent,
                        task: path.rsplit('/').next().unwrap_or_default().to_owned(),
                        path,
                        result: None,
                    });
                }
            }
            for notice in std::mem::take(&mut session.notices) {
                let path = session.children.get(&notice.agent).unwrap_or(&notice.agent);
                // The spawn's result names the full path. An older spawn named only its task, and
                // a failed spawn names none, so match the full path first: a failed attempt
                // must not take the reply of its successful retry.
                let exact = session
                    .spawns
                    .iter()
                    .position(|spawn| !path.is_empty() && spawn.path == *path);
                let index = exact.or_else(|| {
                    session.spawns.iter().position(|spawn| {
                        spawn.path.is_empty()
                            && !spawn.task.is_empty()
                            && path.ends_with(&format!("/{}", spawn.task))
                    })
                });
                if let Some(spawn) = index.map(|index| &mut session.spawns[index]) {
                    *spawn.result.get_or_insert(0.0) += notice.tokens;
                }
            }
            for spawn in session.spawns.drain(..) {
                if let Some(tokens) = spawn.result {
                    side.types
                        .entry(spawn.agent)
                        .or_default()
                        .add_result(tokens);
                }
            }
        }
    }

    pub fn render(mut self, scope: &Scope) -> String {
        self.join_replies();
        let mut out = String::new();
        for harness in Harness::ALL {
            let wanted = match scope.harness {
                Which::All => true,
                Which::Claude => harness == Harness::Claude,
                Which::Codex => harness == Harness::Codex,
            };
            if !wanted {
                continue;
            }
            let id = harness.id();
            let side = self.sides.get(id);
            let spawns: i64 = side.map_or(0, |side| side.types.values().map(|s| s.spawns).sum());
            if spawns == 0 {
                out.push_str(&format!(
                    "{id}: no sub-agent spawns since {}\n",
                    scope.since
                ));
                continue;
            }
            let side = side.expect("spawns come from a side");
            out.push_str(&format!("{id}: sub-agent spawns since {}\n", scope.since));
            spawn_table(&mut out, side, scope);
            if side.encrypted > 0 {
                out.push_str(&format!(
                    "{} spawn prompts are encrypted in the rollout: prompt tokens are estimated from their size and their lines are not compared\n",
                    side.encrypted
                ));
            }
            self.overlap(&mut out, id, scope);
            repeated(&mut out, side, scope);
        }
        out
    }

    fn overlap(&self, out: &mut String, id: &str, scope: &Scope) {
        let mut files: IndexMap<&str, (i64, i64, f64, i64)> = IndexMap::new();
        let mut sessions = HashSet::new();
        for ((harness, project, session), data) in &self.sessions {
            if *harness != id {
                continue;
            }
            for (path, parent) in &data.parent {
                let Some(agents) = data.agents.get(path) else {
                    continue;
                };
                sessions.insert((project, session));
                let entry = files.entry(path).or_default();
                entry.0 += parent.count;
                entry.1 += agents.count;
                // The biggest read is paid for once; every other read is a repeat.
                entry.2 += parent.total + agents.total - parent.max.max(agents.max);
                entry.3 += 1;
            }
        }
        if files.is_empty() {
            out.push_str("overlap: no file read by both a parent and its sub-agents\n");
            return;
        }
        let duplicate: f64 = files.values().map(|file| file.2).sum();
        out.push_str(&format!(
            "overlap: {} files read by both a parent and its sub-agents in {} sessions, about {} duplicate tokens\n",
            files.len(),
            sessions.len(),
            tok(duplicate)
        ));
        let mut rows: Vec<_> = files.into_iter().collect();
        // Sessions come from a hash map, so ties need a stable order.
        rows.sort_by(|a, b| b.1.2.total_cmp(&a.1.2).then(a.0.cmp(b.0)));
        let table: Vec<Vec<String>> = rows
            .iter()
            .take(scope.limit)
            .map(|(path, (parent, agents, tokens, _))| {
                vec![
                    home_relative(path, &scope.home),
                    parent.to_string(),
                    agents.to_string(),
                    tok(*tokens),
                ]
            })
            .collect();
        fmt::table(
            out,
            &["file", "parent reads", "agent reads", "dup tok"],
            &table,
        );
    }
}

fn call_of(block: &Value) -> String {
    key(get(block, "tool_use_id"))
}

fn home_relative(path: &str, home: &Path) -> String {
    match Path::new(path).strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.to_owned(),
    }
}

fn spawn_table(out: &mut String, side: &Side, scope: &Scope) {
    let mut rows: Vec<(&String, &Stats)> =
        side.types.iter().filter(|(_, s)| s.spawns > 0).collect();
    // Types are added in hash-map order, and spawns seen only through a child all have prompt 0.
    rows.sort_by(|a, b| {
        b.1.prompt
            .total_cmp(&a.1.prompt)
            .then(b.1.spawns.cmp(&a.1.spawns))
            .then(a.0.cmp(b.0))
    });
    let all = rows.iter().fold(Stats::default(), |mut all, (_, s)| {
        all.spawns += s.spawns;
        all.prompts += s.prompts;
        all.prompt += s.prompt;
        all.prompt_max = all.prompt_max.max(s.prompt_max);
        all.results += s.results;
        all.result += s.result;
        all.result_max = all.result_max.max(s.result_max);
        all
    });
    let line = |name: String, s: &Stats| {
        let average = |total: f64, count: i64| {
            tok(if count == 0 {
                0.0
            } else {
                total / count as f64
            })
        };
        vec![
            name,
            s.spawns.to_string(),
            tok(s.prompt),
            average(s.prompt, s.prompts),
            tok(s.prompt_max),
            tok(s.result),
            average(s.result, s.results),
            tok(s.result_max),
        ]
    };
    let mut table: Vec<Vec<String>> = rows
        .iter()
        .take(scope.limit)
        .map(|(name, s)| line(head(name, 40).to_owned(), s))
        .collect();
    if rows.len() > 1 {
        table.push(line("all".into(), &all));
    }
    fmt::table(
        out,
        &[
            "agent",
            "spawns",
            "prompt tok",
            "avg",
            "max",
            "result tok",
            "avg",
            "max",
        ],
        &table,
    );
}

fn repeated(out: &mut String, side: &Side, scope: &Scope) {
    let mut lines: Vec<(&String, i64, f64)> = side
        .lines
        .iter()
        .filter(|(_, prompts)| **prompts >= MIN_PROMPTS)
        .map(|(line, prompts)| (line, *prompts, est_text_tokens(line) * *prompts as f64))
        .collect();
    if lines.is_empty() {
        out.push_str(&format!(
            "repeated prompt lines: none of {MIN_LINE}+ characters in {MIN_PROMPTS}+ spawn prompts\n"
        ));
        return;
    }
    lines.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(b.0)));
    out.push_str(&format!(
        "repeated prompt lines: {} lines of {MIN_LINE}+ characters in {MIN_PROMPTS}+ spawn prompts\n",
        lines.len()
    ));
    let table: Vec<Vec<String>> = lines
        .iter()
        .take(scope.limit)
        .map(|(line, prompts, tokens)| {
            vec![
                prompts.to_string(),
                tok(*tokens),
                head(line, SHOWN).to_owned(),
            ]
        })
        .collect();
    fmt::table(out, &["prompts", "total tok", "line"], &table);
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;
    use crate::sessions::source::{Needs, load, scoped};

    const HOME: &str = "/home/al";

    fn write(path: &Path, records: &[Value]) {
        fs::create_dir_all(path.parent().expect("parent")).expect("create directory");
        let lines: Vec<String> = records.iter().map(Value::to_string).collect();
        fs::write(path, lines.join("\n") + "\n").expect("write transcript");
    }

    fn call(id: &str, name: &str, input: Value) -> Value {
        json!({"type": "assistant", "message": {"id": format!("m{id}"), "content": [
            {"type": "tool_use", "id": id, "name": name, "input": input}]}})
    }

    fn result(id: &str, size: usize) -> Value {
        json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": id, "content": "x".repeat(size)}]}})
    }

    fn render(root: &Path) -> String {
        let scope = scoped(root);
        let mut report = Agents::new();
        for transcript in load(
            &scope,
            Needs {
                agents: true,
                ..Needs::default()
            },
        ) {
            report.feed(&transcript);
        }
        report.render(&scope)
    }

    const LINE: &str = "Check   the  failing   test and report the root cause only";

    #[test]
    fn claude_spawns_overlap_and_repeated_lines() {
        let dir = tempfile::tempdir().expect("temporary root");
        let project = dir.path().join("-repo");
        let prompt = format!(
            "{LINE}\nshort\nunique line number {} that is long enough",
            1
        );
        let mut main = Vec::new();
        for (index, id) in ["t1", "t2", "t3"].into_iter().enumerate() {
            let prompt = prompt.replace("number 1", &format!("number {index}"));
            main.push(call(
                id,
                "Agent",
                json!({"subagent_type": "worker", "prompt": prompt}),
            ));
            main.push(result(id, 400));
        }
        main.push(call(
            "r1",
            "Read",
            json!({"file_path": "/home/al/src/a.rs"}),
        ));
        main.push(result("r1", 4000));
        main.push(call(
            "r2",
            "Bash",
            json!({"command": "cd /x && cat /home/al/src/b.rs"}),
        ));
        main.push(result("r2", 2000));
        main.push(call(
            "r3",
            "Read",
            json!({"file_path": "/home/al/src/only-parent.rs"}),
        ));
        main.push(result("r3", 800));
        write(&project.join("11111111-aaaa.jsonl"), &main);
        write(
            &project.join("11111111-aaaa/subagents/agent-a1.jsonl"),
            &[
                call("s1", "Read", json!({"file_path": "/home/al/src/a.rs"})),
                result("s1", 4000),
                call(
                    "s2",
                    "Bash",
                    json!({"command": "sed -n '1,20p' ~/src/b.rs"}),
                ),
                result("s2", 2000),
            ],
        );
        fs::write(
            project.join("11111111-aaaa/subagents/agent-a1.meta.json"),
            r#"{"agentType":"worker"}"#,
        )
        .expect("write meta");
        let out = render(dir.path());
        assert!(out.contains("claude: sub-agent spawns"), "{out}");
        let spawn_row = out
            .lines()
            .find(|line| line.starts_with("worker"))
            .expect("worker row");
        assert!(spawn_row.split_whitespace().nth(1) == Some("3"), "{out}");
        // 400 characters come back from each spawn: 100 tokens.
        assert!(spawn_row.contains("100"), "{out}");
        assert!(
            out.contains("overlap: 2 files read by both a parent and its sub-agents in 1 sessions"),
            "{out}"
        );
        assert!(out.contains("~/src/a.rs"), "{out}");
        assert!(!out.contains("only-parent"), "{out}");
        // Paid once: the 4000 and 2000 character reads each repeat once.
        assert!(out.contains("about 1.5k duplicate tokens"), "{out}");
        assert!(
            out.contains("repeated prompt lines: 1 lines of 40+ characters in 3+ spawn prompts"),
            "{out}"
        );
        assert!(
            out.contains("Check the failing test and report the root cause only"),
            "{out}"
        );
        assert!(!out.contains("unique line"), "{out}");
    }

    #[test]
    fn background_claude_results_come_back_as_notifications() {
        let dir = tempfile::tempdir().expect("temporary root");
        let notice = format!(
            "<task-notification>\n<tool-use-id>t1</tool-use-id>\n{}",
            "y".repeat(800)
        );
        write(
            &dir.path().join("-repo/22222222-aaaa.jsonl"),
            &[
                call(
                    "t1",
                    "Agent",
                    json!({"subagent_type": "worker", "prompt": "go"}),
                ),
                result("t1", 40),
                json!({"type": "attachment", "attachment": {"type": "queued_command", "prompt": notice}}),
            ],
        );
        let out = render(dir.path());
        let row = out
            .lines()
            .find(|line| line.starts_with("worker"))
            .expect("row");
        // The acknowledgement and the notification both reach the parent.
        assert!(row.contains("222"), "{out}");
    }

    #[test]
    fn notifications_count_when_hooks_are_selected_too() {
        let dir = tempfile::tempdir().expect("temporary root");
        let notice = format!(
            "<task-notification>\n<tool-use-id>t1</tool-use-id>\n{}",
            "y".repeat(800)
        );
        write(
            &dir.path().join("-repo/22222222-aaaa.jsonl"),
            &[
                call(
                    "t1",
                    "Agent",
                    json!({"subagent_type": "worker", "prompt": "go"}),
                ),
                result("t1", 40),
                json!({"type": "attachment", "attachment": {"type": "queued_command", "prompt": notice}}),
            ],
        );
        let scope = scoped(dir.path());
        let mut report = Agents::new();
        for transcript in load(
            &scope,
            Needs {
                agents: true,
                hooks: true,
                ..Needs::default()
            },
        ) {
            report.feed(&transcript);
        }
        let out = report.render(&scope);
        let row = out
            .lines()
            .find(|line| line.starts_with("worker"))
            .expect("row");
        assert!(row.contains("222"), "{out}");
    }

    #[test]
    fn older_codex_spawns_from_exec_scripts_count_from_the_children() {
        let dir = tempfile::tempdir().expect("temporary root");
        let parent = "019f0000-0000-7000-8000-00000000aaaa";
        let exec = json!({"type": "response_item", "payload": {"type": "custom_tool_call",
            "name": "exec", "call_id": "c1",
            "input": "const r = await tools.multi_agent_v1__spawn_agent({message: \"go\"});"}});
        write(
            &dir.path().join("sessions/2026/03/01/rollout-parent.jsonl"),
            &rollout(parent, json!("cli"), &[exec]),
        );
        for (child, role) in [
            ("019f0000-0000-7000-8000-00000000bbbb", "reviewer"),
            ("019f0000-0000-7000-8000-00000000cccc", "reviewer"),
        ] {
            write(
                &dir.path()
                    .join(format!("sessions/2026/03/01/rollout-{child}.jsonl")),
                &rollout(
                    child,
                    json!({"subagent": {"thread_spawn": {"parent_thread_id": parent,
                        "agent_path": null, "agent_role": role}}}),
                    &[],
                ),
            );
        }
        // A newer session's spawn has a readable prompt; the children above have none.
        let spawn = json!({"type": "response_item", "payload": {"type": "function_call",
            "name": "spawn_agent", "call_id": "c2",
            "arguments": json!({"agent_type": "reviewer", "message": "x".repeat(400)}).to_string()}});
        write(
            &dir.path().join("sessions/2026/03/01/rollout-newer.jsonl"),
            &rollout(
                "019f0000-0000-7000-8000-00000000dddd",
                json!("cli"),
                &[spawn],
            ),
        );
        let out = render(dir.path());
        let row = out
            .lines()
            .find(|line| line.starts_with("reviewer"))
            .expect("row");
        let cells: Vec<&str> = row.split_whitespace().collect();
        // Columns: agent, spawns, prompt tok, avg; the average skips spawns with no prompt.
        assert_eq!(cells[1], "3", "{out}");
        assert_eq!(cells[3], "100", "{out}");
    }

    #[test]
    fn a_nested_child_counts_once() {
        let dir = tempfile::tempdir().expect("temporary root");
        let root = "019f0000-0000-7000-8000-00000000aaaa";
        let child = "019f0000-0000-7000-8000-00000000bbbb";
        let grandchild = "019f0000-0000-7000-8000-00000000cccc";
        let spawn = |id: &str, kind: &str, task: &str, path: &str| {
            vec![
                json!({"type": "response_item", "payload": {"type": "function_call",
                    "name": "spawn_agent", "call_id": id,
                    "arguments": json!({"task_name": task, "agent_type": kind}).to_string()}}),
                json!({"type": "response_item", "payload": {"type": "function_call_output",
                    "call_id": id, "output": json!({"task_name": path}).to_string()}}),
            ]
        };
        let rollouts = [
            (root, json!("cli"), spawn("c1", "explorer", "a", "/root/a")),
            (
                child,
                json!({"subagent": {"thread_spawn": {"parent_thread_id": root, "depth": 1,
                    "agent_path": "/root/a", "agent_role": "explorer"}}}),
                spawn("c2", "worker", "b", "/root/a/b"),
            ),
            (
                grandchild,
                json!({"subagent": {"thread_spawn": {"parent_thread_id": child, "depth": 2,
                    "agent_path": "/root/a/b", "agent_role": "worker"}}}),
                Vec::new(),
            ),
        ];
        for (id, source, records) in rollouts {
            write(
                &dir.path()
                    .join(format!("sessions/2026/03/01/rollout-{id}.jsonl")),
                &rollout(id, source, &records),
            );
        }
        let out = render(dir.path());
        let spawns = |name: &str| {
            out.lines()
                .find(|line| line.starts_with(name))
                .and_then(|line| line.split_whitespace().nth(1))
                .map(str::to_owned)
        };
        assert_eq!(spawns("explorer").as_deref(), Some("1"), "{out}");
        assert_eq!(spawns("worker").as_deref(), Some("1"), "{out}");
    }

    #[test]
    fn a_failed_spawn_does_not_take_its_retry_reply() {
        let dir = tempfile::tempdir().expect("temporary root");
        let parent = "019f0000-0000-7000-8000-00000000aaaa";
        let call = |id: &str, kind: &str| {
            json!({"type": "response_item", "payload": {"type": "function_call",
                "name": "spawn_agent", "call_id": id,
                "arguments": json!({"task_name": "critic", "agent_type": kind}).to_string()}})
        };
        let output = |id: &str, body: String| {
            json!({"type": "response_item", "payload": {"type": "function_call_output",
                "call_id": id, "output": body}})
        };
        let records = [
            call("c1", "alpha"),
            output("c1", "agent thread limit reached".into()),
            call("c2", "beta"),
            output("c2", json!({"task_name": "/root/critic"}).to_string()),
            json!({"type": "response_item", "payload": {"type": "agent_message",
                "author": "/root/critic", "recipient": "/root",
                "content": [{"type": "input_text", "text": "x".repeat(400)}]}}),
        ];
        write(
            &dir.path().join("sessions/2026/03/01/rollout-parent.jsonl"),
            &rollout(parent, json!("cli"), &records),
        );
        let out = render(dir.path());
        let cells = |name: &str| -> Vec<String> {
            out.lines()
                .find(|line| line.starts_with(name))
                .map(|line| line.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default()
        };
        // Columns: agent, spawns, prompt tok, avg, max, result tok.
        assert_eq!(cells("beta")[5], "100", "{out}");
        assert_eq!(cells("alpha")[5], "0", "{out}");
    }

    #[test]
    fn tied_agent_rows_keep_a_stable_order() {
        let dir = tempfile::tempdir().expect("temporary root");
        // Exec-script sessions whose children all have prompt 0: only the tie-break orders them.
        let roles = [
            "worker", "explorer", "tester", "cheap", "consult", "reviewer",
        ];
        for (n, role) in roles.iter().enumerate() {
            let parent = format!("019f0000-0000-7000-8000-0000000010{n:02}");
            let child = format!("019f0000-0000-7000-8000-0000000020{n:02}");
            write(
                &dir.path()
                    .join(format!("sessions/2026/03/01/rollout-{parent}.jsonl")),
                &rollout(&parent, json!("cli"), &[]),
            );
            write(
                &dir.path()
                    .join(format!("sessions/2026/03/01/rollout-{child}.jsonl")),
                &rollout(
                    &child,
                    json!({"subagent": {"thread_spawn": {"parent_thread_id": parent,
                        "agent_path": null, "agent_role": role}}}),
                    &[],
                ),
            );
        }
        let out = render(dir.path());
        let order: Vec<&str> = out
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|name| roles.contains(name))
            .collect();
        let mut sorted = roles.to_vec();
        sorted.sort_unstable();
        assert_eq!(order, sorted, "{out}");
    }

    #[test]
    fn replies_match_the_spawn_by_full_agent_path() {
        let dir = tempfile::tempdir().expect("temporary root");
        let parent = "019f0000-0000-7000-8000-00000000aaaa";
        let spawn = |id: &str, kind: &str, task: &str, path: &str| {
            vec![
                json!({"type": "response_item", "payload": {"type": "function_call",
                    "name": "spawn_agent", "call_id": id,
                    "arguments": json!({"task_name": task, "agent_type": kind}).to_string()}}),
                json!({"type": "response_item", "payload": {"type": "function_call_output",
                    "call_id": id, "output": json!({"task_name": path}).to_string()}}),
            ]
        };
        // Two spawns share a task name at different depths; the reply is the second's.
        let mut records = spawn("c1", "alpha", "audit", "/root/audit");
        records.extend(spawn("c2", "beta", "audit", "/root/team/audit"));
        records.push(
            json!({"type": "response_item", "payload": {"type": "agent_message",
            "author": "/root/team/audit", "recipient": "/root",
            "content": [{"type": "input_text", "text": "x".repeat(400)}]}}),
        );
        write(
            &dir.path().join("sessions/2026/03/01/rollout-parent.jsonl"),
            &rollout(parent, json!("cli"), &records),
        );
        let out = render(dir.path());
        let cells = |name: &str| -> Vec<String> {
            out.lines()
                .find(|line| line.starts_with(name))
                .map(|line| line.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default()
        };
        // Columns: agent, spawns, prompt tok, avg, max, result tok.
        assert_eq!(cells("beta")[5], "100", "{out}");
        assert_eq!(cells("alpha")[5], "0", "{out}");
    }

    fn rollout(id: &str, source: Value, records: &[Value]) -> Vec<Value> {
        let mut all = vec![
            json!({"type": "session_meta", "timestamp": "2026-03-01T00:00:00Z",
            "payload": {"id": id, "cwd": "/home/al/repo", "source": source}}),
        ];
        all.extend(records.iter().cloned());
        all
    }

    fn shell(id: &str, command: &str, size: usize) -> Vec<Value> {
        vec![
            json!({"type": "response_item", "payload": {"type": "function_call",
                "name": "exec_command", "call_id": id,
                "arguments": json!({"cmd": command}).to_string()}}),
            json!({"type": "response_item", "payload": {"type": "function_call_output",
                "call_id": id, "output": "z".repeat(size)}}),
            json!({"type": "event_msg", "payload": {"type": "token_count", "info": null}}),
        ]
    }

    #[test]
    fn codex_child_rollouts_pair_with_the_parents_spawn() {
        let dir = tempfile::tempdir().expect("temporary root");
        let parent = "019f0000-0000-7000-8000-00000000aaaa";
        let child = "019f0000-0000-7000-8000-00000000bbbb";
        let mut records = vec![
            json!({"type": "response_item", "payload": {"type": "function_call",
                "name": "spawn_agent", "call_id": "c1",
                "arguments": json!({"task_name": "audit", "message": "g".repeat(0), "agent_type": "reviewer"}).to_string()}}),
            json!({"type": "response_item", "payload": {"type": "function_call_output",
                "call_id": "c1", "output": "{\"task_name\":\"/root/audit\"}"}}),
            json!({"type": "event_msg", "payload": {"type": "token_count", "info": null}}),
        ];
        records.extend(shell("c2", "cat /home/al/src/a.rs", 4000));
        records.push(
            json!({"type": "response_item", "payload": {"type": "agent_message",
            "author": "/root/audit", "recipient": "/root",
            "content": [{"type": "input_text", "text": "x".repeat(400)}]}}),
        );
        // The parent's own message to a child is not a reply.
        records.push(
            json!({"type": "response_item", "payload": {"type": "agent_message",
            "author": "/root", "recipient": "/root/audit",
            "content": [{"type": "input_text", "text": "x".repeat(4000)}]}}),
        );
        write(
            &dir.path().join("sessions/2026/03/01/rollout-parent.jsonl"),
            &rollout(parent, json!("cli"), &records),
        );
        write(
            &dir.path().join("sessions/2026/03/01/rollout-child.jsonl"),
            &rollout(
                child,
                json!({"subagent": {"thread_spawn": {"parent_thread_id": parent,
                    "agent_path": "/root/audit", "agent_role": "reviewer"}}}),
                &shell("d1", "cat /home/al/src/a.rs", 4000),
            ),
        );
        let out = render(dir.path());
        assert!(out.contains("codex: sub-agent spawns"), "{out}");
        let row = out
            .lines()
            .find(|line| line.starts_with("reviewer"))
            .expect("row");
        assert!(row.split_whitespace().nth(1) == Some("1"), "{out}");
        assert!(row.contains("100"), "{out}");
        assert!(
            out.contains("overlap: 1 files read by both a parent and its sub-agents in 1 sessions"),
            "{out}"
        );
        assert!(out.contains("~/src/a.rs"), "{out}");
        assert!(out.contains("claude: no sub-agent spawns"), "{out}");
    }

    #[test]
    fn older_codex_replies_name_the_child_by_thread_id() {
        let dir = tempfile::tempdir().expect("temporary root");
        let parent = "019f0000-0000-7000-8000-00000000aaaa";
        let child = "019f0000-0000-7000-8000-00000000bbbb";
        let note = format!(
            "<subagent_notification>\n{}\n</subagent_notification>",
            json!({"agent_path": child, "status": {"completed": "w".repeat(400)}})
        );
        let mut records = vec![
            json!({"type": "response_item", "payload": {"type": "function_call",
            "name": "spawn_agent", "call_id": "c1",
            "arguments": json!({"task_name": "audit", "message": "hi"}).to_string()}}),
        ];
        records
            .push(json!({"type": "event_msg", "payload": {"type": "token_count", "info": null}}));
        records.push(
            json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": note}]}}),
        );
        write(
            &dir.path().join("sessions/2026/03/01/rollout-parent.jsonl"),
            &rollout(parent, json!("cli"), &records),
        );
        write(
            &dir.path().join("sessions/2026/03/01/rollout-child.jsonl"),
            &rollout(
                child,
                json!({"subagent": {"thread_spawn": {"parent_thread_id": parent,
                    "agent_path": "/root/audit", "agent_role": null}}}),
                &[],
            ),
        );
        let out = render(dir.path());
        let row = out
            .lines()
            .find(|line| line.starts_with("default"))
            .expect("row");
        assert!(row.split_whitespace().nth(1) == Some("1"), "{out}");
        assert!(!row.contains(" 0  0  0"), "{out}");
    }

    #[test]
    fn single_file_shell_reads_name_their_path() {
        let home = Path::new(HOME);
        for (command, path) in [
            ("cat /a/b.rs", Some("/a/b.rs")),
            ("cat ~/b.rs", Some("/home/al/b.rs")),
            ("head -n 20 /a/b.rs", Some("/a/b.rs")),
            ("tail -50 '/a/b.rs'", Some("/a/b.rs")),
            ("sed -n '1,40p' /a/b.rs", Some("/a/b.rs")),
            ("cd /x && rg -n foo /a/b.rs", Some("/a/b.rs")),
            ("cat a.rs", None),
            ("cat /a/b.rs /a/c.rs", None),
            ("cat /a/b.rs | head", None),
            ("sed -i 's/a/b/' /a/b.rs", None),
            ("rg foo /a/src", None),
            ("ls /a/b.rs", None),
        ] {
            assert_eq!(shell_read(command, home).as_deref(), path, "{command}");
        }
    }

    #[test]
    fn prompt_lines_are_trimmed_collapsed_and_counted_once() {
        let lines = long_lines(&format!("  {LINE}  \n{LINE}\nshort line\n"));
        assert_eq!(
            lines,
            ["Check the failing test and report the root cause only"]
        );
    }
}
