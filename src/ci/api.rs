//! A GitLab project reached through `src/forge`.

use indexmap::IndexMap;
use serde_json::Value;

use crate::core::error::Result;
use crate::core::transcripts::json::text;
use crate::forge::gitlab::ci::base_of_id;
use crate::forge::gitlab::{self, Gitlab};
use crate::forge::transport::CliTransport;

pub const RUNNING: [&str; 7] = [
    "created",
    "waiting_for_resource",
    "preparing",
    "pending",
    "running",
    "scheduled",
    "canceling",
];

/// A JSON id as text.
pub fn id_text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

/// The attempts of each job by name, oldest attempt first, in the order of the oldest attempt.
pub type Jobs = IndexMap<String, Vec<Value>>;

/// A GitLab project of the current repository or one a host and path name.
pub struct Project {
    host: Option<String>,
    pub base: String,
    pub path: String,
    pub default_branch: String,
}

impl Project {
    pub fn connect(path: Option<&str>, host: Option<&str>) -> Result<Self> {
        let mut project = Self {
            host: host.map(str::to_owned),
            base: String::new(),
            path: String::new(),
            default_branch: String::new(),
        };
        let info = project.service().project(path)?;
        project.base = base_of_id(info["id"].as_i64().unwrap_or_default());
        project.path = text(&info, "path_with_namespace").to_owned();
        project.default_branch = text(&info, "default_branch").to_owned();
        Ok(project)
    }

    /// A service of its own, since a transport is not shared between threads.
    fn service(&self) -> Gitlab {
        match &self.host {
            Some(host) => gitlab::connect(host),
            None => Gitlab::new(Box::new(CliTransport::glab_configured())),
        }
    }

    pub fn merge_request(&self, iid: u64) -> Result<Value> {
        Ok(self.service().merge_request(&self.base, iid)?)
    }

    pub fn runs_commit(&self, pipeline: &Value, sha: &str) -> Result<bool> {
        Ok(self.service().pipeline_runs(&self.base, pipeline, sha)?)
    }

    pub fn finished_pipelines(&self, per_page: usize) -> Result<Value> {
        Ok(self
            .service()
            .finished_pipelines(&self.base, &self.default_branch, per_page)?)
    }

    pub fn jobs(&self, pipeline: &Value) -> Result<Jobs> {
        let id = pipeline.as_u64().unwrap_or_default();
        let mut jobs = self.service().pipeline_jobs(&self.base, id, true)?;
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
        Ok(self
            .service()
            .job_trace(&self.base, job_id.as_u64().unwrap_or_default())?)
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
