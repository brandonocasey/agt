//! A GitLab project reached through `glab api`.

use std::ffi::OsString;

use indexmap::IndexMap;
use serde_json::Value;

use crate::core::error::{Error, Result};
use crate::core::proc;

pub const RUNNING: [&str; 7] = [
    "created",
    "waiting_for_resource",
    "preparing",
    "pending",
    "running",
    "scheduled",
    "canceling",
];

pub fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Python's `str(value)` for the values the tool prints.
pub fn display(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(true)) => "True".to_owned(),
        Some(Value::Bool(false)) => "False".to_owned(),
        Some(other) => other.to_string(),
    }
}

pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(fields)) => !fields.is_empty(),
    }
}

/// `urllib.parse.quote(text, safe='')`.
pub fn quote(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"_.-~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The attempts of each job by name, oldest attempt first, in the order of the oldest attempt.
pub type Jobs = IndexMap<String, Vec<Value>>;

pub struct Gitlab {
    program: OsString,
    host: Option<String>,
    pub base: String,
    pub path: String,
    pub default_branch: String,
}

impl Gitlab {
    pub fn connect(project: Option<&str>, host: Option<&str>) -> Result<Self> {
        let mut gitlab = Self {
            program: std::env::var_os("AGT_GLAB_BIN").unwrap_or_else(|| "glab".into()),
            host: host.map(str::to_owned),
            base: String::new(),
            path: String::new(),
            default_branch: String::new(),
        };
        let reference = project.map_or_else(|| ":id".to_owned(), quote);
        let info = gitlab.get(&format!("projects/{reference}"))?;
        gitlab.base = format!("projects/{}", display(info.get("id")));
        gitlab.path = text(&info, "path_with_namespace").to_owned();
        gitlab.default_branch = text(&info, "default_branch").to_owned();
        Ok(gitlab)
    }

    fn call(&self, path: &str, paginate: bool) -> Result<String> {
        let mut args = vec!["api", path];
        if paginate {
            args.push("--paginate");
        }
        let env: Vec<(&str, &str)> = self
            .host
            .as_deref()
            .map(|host| ("GITLAB_HOST", host))
            .into_iter()
            .collect();
        let run = proc::run(
            &self.program,
            &args,
            &proc::Options {
                env: &env,
                ..proc::Options::default()
            },
        );
        if let Some(name) = run.spawn_error {
            return Err(Error(format!("glab api {path} failed: {name}")));
        }
        if run.code != Some(0) {
            let detail = [run.stderr.trim(), run.stdout.trim()]
                .into_iter()
                .find(|part| !part.is_empty())
                .unwrap_or("");
            return Err(Error(format!("glab api {path} failed: {detail}")));
        }
        Ok(run.stdout)
    }

    /// The parsed reply. `--paginate` prints one JSON array per page back to back; they come
    /// back as one array.
    fn fetch(&self, path: &str, paginate: bool) -> Result<Value> {
        let text = self.call(path, paginate)?;
        let mut values = Vec::new();
        for value in serde_json::Deserializer::from_str(&text).into_iter::<Value>() {
            values.push(value.map_err(|error| Error(format!("glab api {path} failed: {error}")))?);
        }
        if !paginate {
            return Ok(values.into_iter().next().unwrap_or(Value::Null));
        }
        let mut items = Vec::new();
        for page in values {
            match page {
                Value::Array(list) => items.extend(list),
                other => items.push(other),
            }
        }
        Ok(Value::Array(items))
    }

    pub fn get(&self, path: &str) -> Result<Value> {
        self.fetch(path, false)
    }

    pub fn jobs(&self, pipeline: &Value) -> Result<Jobs> {
        let path = format!(
            "{}/pipelines/{}/jobs?include_retried=true&per_page=100",
            self.base,
            display(Some(pipeline))
        );
        let Value::Array(mut jobs) = self.fetch(&path, true)? else {
            return Ok(Jobs::new());
        };
        jobs.sort_by_key(|job| job.get("id").and_then(Value::as_i64).unwrap_or(0));
        let mut by_name = Jobs::new();
        for job in jobs {
            by_name
                .entry(text(&job, "name").to_owned())
                .or_default()
                .push(job);
        }
        Ok(by_name)
    }

    pub fn trace(&self, job_id: &Value) -> Result<String> {
        self.call(
            &format!("{}/jobs/{}/trace", self.base, display(Some(job_id))),
            false,
        )
    }
}

/// Runs `work` on every item with at most `limit` threads at once; results keep the item order.
pub fn parallel<T: Sync, R: Send>(
    items: &[T],
    limit: usize,
    work: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new(items.iter().map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..limit.min(items.len()) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(item) = items.get(index) else { break };
                    let result = work(item);
                    if let Ok(mut slots) = results.lock() {
                        slots[index] = Some(result);
                    }
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_default()
        .into_iter()
        .flatten()
        .collect()
}
