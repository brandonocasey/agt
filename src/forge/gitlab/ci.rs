//! MRs, pipelines, jobs and logs of a project, for `agt ci`.

use serde_json::Value;

use super::{Gitlab, project_path};
use crate::forge;
use crate::forge::service::{Method, Pagination, Request};
use crate::forge::transport::TransportError;

/// Hex digits in a full commit id.
const FULL_SHA_LEN: usize = 40;

type Result<T> = std::result::Result<T, TransportError>;

/// The API path of a project by its id.
pub fn base_of_id(id: i64) -> String {
    format!("projects/{id}")
}

impl Gitlab {
    fn read(&self, path: String) -> Result<Value> {
        let transport: &dyn forge::transport::Transport = self.transport.as_ref();
        transport.json(&Request::get(path))
    }

    fn list(&self, path: String) -> Result<Vec<Value>> {
        let transport: &dyn forge::transport::Transport = self.transport.as_ref();
        transport.items(&Request::get(path), &Pagination::CliPaginate)
    }

    /// The project at `path` with namespace, or the current repository's for `None`.
    pub fn project(&self, path: Option<&str>) -> Result<Value> {
        self.read(path.map_or_else(|| "projects/:id".to_owned(), project_path))
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

    /// The full id of commit `sha` and the pipelines that ran it, newest first. A full `sha` is
    /// not looked up, since the pipeline list takes only full ones.
    pub fn commit_pipelines(&self, base: &str, sha: &str) -> Result<(String, Vec<Value>)> {
        let full = if sha.len() == FULL_SHA_LEN {
            sha.to_owned()
        } else {
            let commit = self.read(format!("{base}/repository/commits/{sha}"))?;
            commit["id"]
                .as_str()
                .ok_or_else(|| TransportError::new(format!("{sha} is not a commit of {base}")))?
                .to_owned()
        };
        let pipelines = self.list(format!("{base}/pipelines?sha={full}&per_page=100"))?;
        Ok((full, pipelines))
    }

    /// Retries the job `job`; the new job it starts.
    pub fn retry_job(&self, base: &str, job: u64) -> Result<Value> {
        let transport: &dyn forge::transport::Transport = self.transport.as_ref();
        transport.json(&Request::rest(
            Method::Post,
            format!("{base}/jobs/{job}/retry"),
            None,
        ))
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
