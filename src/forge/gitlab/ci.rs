//! MRs, pipelines, jobs and logs of a project, for `agt ci`.

use serde_json::Value;

use super::Gitlab;
use crate::forge;
use crate::forge::service::{Pagination, Request};
use crate::forge::transport::TransportError;

type Result<T> = std::result::Result<T, TransportError>;

/// The API path of a project by its path with namespace.
pub fn base_of_path(path: &str) -> String {
    format!("projects/{}", forge::encode(path))
}

/// The API path of a project by its id.
pub fn base_of_id(id: i64) -> String {
    format!("projects/{id}")
}

impl Gitlab {
    fn read(&self, path: String) -> Result<Value> {
        match self.transport.send(&Request::get(path))?.body {
            Value::Null => Err(TransportError::new("the host sent an empty reply")),
            body => Ok(body),
        }
    }

    fn list(&self, path: String) -> Result<Vec<Value>> {
        let transport: &dyn forge::transport::Transport = self.transport.as_ref();
        Ok(transport.items(&Request::get(path), &Pagination::CliPaginate)?)
    }

    /// The project at `path` with namespace, or the current repository's for `None`.
    pub fn project(&self, path: Option<&str>) -> Result<Value> {
        self.read(path.map_or_else(|| "projects/:id".to_owned(), base_of_path))
    }

    pub fn merge_request(&self, base: &str, iid: u64) -> Result<Value> {
        self.read(format!("{base}/merge_requests/{iid}"))
    }

    pub fn pipeline(&self, base: &str, id: u64) -> Result<Value> {
        self.read(format!("{base}/pipelines/{id}"))
    }

    /// The jobs of a pipeline; with `retried`, the earlier attempts of retried jobs too.
    pub fn pipeline_jobs(&self, base: &str, id: u64, retried: bool) -> Result<Vec<Value>> {
        let retried = if retried { "include_retried=true&" } else { "" };
        self.list(format!("{base}/pipelines/{id}/jobs?{retried}per_page=100"))
    }

    /// The finished pipelines of `branch`, newest first.
    pub fn finished_pipelines(&self, base: &str, branch: &str, per_page: usize) -> Result<Value> {
        self.read(format!(
            "{base}/pipelines?ref={}&scope=finished&per_page={per_page}",
            forge::encode(branch)
        ))
    }

    pub fn job_trace(&self, base: &str, job: u64) -> Result<String> {
        let request = Request {
            raw: true,
            ..Request::get(format!("{base}/jobs/{job}/trace"))
        };
        match self.transport.send(&request)?.body {
            Value::String(text) => Ok(text),
            _ => Ok(String::new()),
        }
    }

    /// Whether the head pipeline of the MR `iid` tested `sha`; false without a pipeline.
    pub fn head_pipeline_runs(&self, base: &str, iid: u64, sha: &str) -> Result<bool> {
        let request = self.merge_request(base, iid)?;
        let pipeline = &request["head_pipeline"];
        if pipeline.is_null() {
            return Ok(false);
        }
        self.pipeline_runs(base, pipeline, sha)
    }

    /// Whether `pipeline` tested `sha`. A merged-results pipeline runs on a merge commit whose
    /// parent is `sha`.
    pub fn pipeline_runs(&self, base: &str, pipeline: &Value, sha: &str) -> Result<bool> {
        let Some(pipeline_sha) = pipeline["sha"].as_str().filter(|sha| !sha.is_empty()) else {
            return Ok(false);
        };
        if pipeline_sha == sha {
            return Ok(true);
        }
        let commit = self.read(format!("{base}/repository/commits/{pipeline_sha}"))?;
        Ok(commit["parent_ids"]
            .as_array()
            .is_some_and(|parents| parents.iter().any(|parent| parent.as_str() == Some(sha))))
    }
}
