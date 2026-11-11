//! GitLab merge requests through the REST API; a project is addressed by its path.

use serde_json::{Value, json};

use super::provider::{At, Capabilities, Filter, NewPr, Outcome, Provider};
use super::remote::{self, Remote};
use super::upload::{Image, Uploaded, Uploader};
use super::{Changes, How, Native, Pr, State};
use crate::core::error::{Error, Result, fail};
use crate::forge::encode;
use crate::forge::gitlab::{self, project_path};
use crate::forge::service::{Method, Request};
use crate::forge::transport::Transport;
use crate::store::model::ReviewState;

fn base(repo: &str) -> String {
    format!("{}/merge_requests", project_path(repo))
}

fn mr(node: &Value) -> Pr {
    let text = |key: &str| node[key].as_str().unwrap_or_default().to_owned();
    let detailed = text("detailed_merge_status");
    let conflicts = node["has_conflicts"].as_bool().unwrap_or(false);
    let squash = node["squash_on_merge"]
        .as_bool()
        .or_else(|| node["squash"].as_bool())
        .unwrap_or(false);
    let mergeable = match detailed.as_str() {
        _ if conflicts => "conflicting",
        "conflict" => "conflicting",
        "mergeable" => "mergeable",
        "" | "checking" | "unchecked" | "preparing" => "unknown",
        _ => "blocked",
    };
    let shared = gitlab::pull_node(node);
    Pr {
        id: node["iid"].to_string(),
        number: Some(shared.number.unwrap_or_default()),
        url: Some(shared.url),
        title: shared.title,
        body: text("description"),
        state: shared.state.unwrap_or(ReviewState::Open),
        draft: node["draft"].as_bool().unwrap_or(false),
        head_branch: shared.head_branch,
        base_branch: shared.base,
        head_sha: shared.head_sha,
        author: node["author"]["username"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        labels: node["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|label| label.as_str().map(str::to_owned))
            .collect(),
        auto_merge_known: true,
        auto_merge: node["merge_when_pipeline_succeeds"]
            .as_bool()
            .unwrap_or(false)
            .then(|| if squash { "squash" } else { "merge" }.to_owned()),
        queue: None,
        mergeable: mergeable.to_owned(),
        merge_state: detailed,
        conflicts,
        same_repo: shared.same_repo.unwrap_or(true),
        native: Native::Gitlab { squash },
    }
}

/// The merge train state of an open MR with auto-merge on; `None` when it is not on a train
/// (the host answers 404).
fn queue(t: &dyn Transport, repo: &str, found: &mut Pr) -> Result<()> {
    if found.state != ReviewState::Open || found.auto_merge.is_none() {
        return Ok(());
    }
    let path = format!(
        "{}/merge_trains/merge_requests/{}",
        project_path(repo),
        found.forge_number()
    );
    match t.send(&Request::get(path)) {
        Ok(reply) => {
            found.queue = Some(reply.body["status"].as_str().unwrap_or("queued").to_owned());
        }
        Err(error) if error.status == Some(404) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub fn view(t: &dyn Transport, repo: &str, number: u64) -> Result<Pr> {
    let mut found = mr(&t.json(&Request::get(format!("{}/{number}", base(repo))))?);
    queue(t, repo, &mut found)?;
    Ok(found)
}

pub fn list(
    t: &dyn Transport,
    repo: &str,
    branch: Option<&str>,
    state: State,
    mine: bool,
) -> Result<Vec<Pr>> {
    let state = match state {
        State::Open => "opened",
        State::Merged => "merged",
        State::All => "all",
    };
    let mut path = format!(
        "{}?state={state}&order_by=created_at&sort=desc&per_page=100",
        base(repo)
    );
    if let Some(branch) = branch {
        path.push_str(&format!("&source_branch={}", encode(branch)));
    }
    if mine {
        path.push_str("&scope=created_by_me");
    }
    let reply = t.json(&Request::get(path))?;
    let mut found: Vec<Pr> = reply
        .as_array()
        .ok_or_else(|| Error(format!("GitLab returned no merge requests for {repo}")))?
        .iter()
        .map(mr)
        .collect();
    for one in &mut found {
        queue(t, repo, one)?;
    }
    Ok(found)
}

pub fn create(
    t: &dyn Transport,
    repo: &str,
    head: &str,
    base_branch: &str,
    title: &str,
    body: &str,
    draft: bool,
) -> Result<u64> {
    let title = if draft {
        format!("Draft: {title}")
    } else {
        title.to_owned()
    };
    let body = json!({
        "source_branch": head,
        "target_branch": base_branch,
        "title": title,
        "description": body,
    });
    let created = t.json(&Request::rest(Method::Post, base(repo), Some(body)))?;
    created["iid"]
        .as_u64()
        .ok_or_else(|| Error("GitLab did not return the new merge request's number".into()))
}

/// Uploads to the project; GitLab answers with Markdown that resolves inside the project.
pub fn upload(t: &dyn Transport, host: &str, repo: &str, image: &Image) -> Result<Uploaded> {
    let reply = t.json(&Request::upload(
        format!("{}/uploads", project_path(repo)),
        image.path.clone(),
    ))?;
    let text = |key: &str| {
        reply[key]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let (Some(markdown), Some(path)) = (text("markdown"), text("full_path")) else {
        return fail("GitLab did not return the uploaded image's Markdown and path");
    };
    Ok(Uploaded {
        url: format!("https://{host}{path}"),
        markdown,
    })
}

/// The title without a draft marker.
fn plain(title: &str) -> &str {
    let lower = title.to_lowercase();
    for marker in [
        "draft: ", "draft:", "[draft] ", "[draft]", "(draft) ", "(draft)",
    ] {
        if lower.starts_with(marker) {
            return title[marker.len()..].trim_start();
        }
    }
    title
}

pub fn update(t: &dyn Transport, repo: &str, current: &Pr, changes: &Changes) -> Result<()> {
    let mut body = serde_json::Map::new();
    let title = changes.title.as_deref().unwrap_or(&current.title);
    match changes.draft {
        Some(false) => body.insert("title".into(), json!(plain(title))),
        Some(true) => body.insert("title".into(), json!(format!("Draft: {}", plain(title)))),
        None => changes
            .title
            .as_ref()
            .and_then(|title| body.insert("title".into(), json!(title))),
    };
    if let Some(text) = &changes.body {
        body.insert("description".into(), json!(text));
    }
    if !changes.add_labels.is_empty() {
        body.insert("add_labels".into(), json!(changes.add_labels.join(",")));
    }
    if !changes.remove_labels.is_empty() {
        body.insert(
            "remove_labels".into(),
            json!(changes.remove_labels.join(",")),
        );
    }
    t.send(&Request::rest(
        Method::Put,
        format!("{}/{}", base(repo), current.forge_number()),
        Some(Value::Object(body)),
    ))
    .map(|_| ())
    .map_err(Error::from)
}

/// Merges, or with `auto` merges when the pipeline succeeds; GitLab refuses when the head is no
/// longer `sha`.
pub fn merge(
    t: &dyn Transport,
    repo: &str,
    current: &Pr,
    sha: &str,
    how: Option<How>,
    auto: bool,
) -> Result<()> {
    let mut body = json!({"sha": sha, "should_remove_source_branch": false});
    match how {
        Some(How::Squash) => body["squash"] = json!(true),
        // GitLab squashes when the MR has squash on and only reads `squash: true` from the request.
        Some(How::Merge) if matches!(current.native, Native::Gitlab { squash: true }) => {
            return fail(format!(
                "!{} has squash on, which GitLab applies on merge; use --method squash or turn squash off on the merge request",
                current.forge_number()
            ));
        }
        Some(How::Rebase) => {
            return fail("GitLab cannot rebase on merge; use --method merge or squash");
        }
        Some(How::Merge) | None => {}
    }
    if auto {
        body["merge_when_pipeline_succeeds"] = json!(true);
    }
    t.send(&Request::rest(
        Method::Put,
        format!("{}/{}/merge", base(repo), current.forge_number()),
        Some(body),
    ))
    .map(|_| ())
    .map_err(Error::from)
}

pub fn disable_auto(t: &dyn Transport, repo: &str, number: u64) -> Result<()> {
    t.send(&Request::rest(
        Method::Post,
        format!(
            "{}/{number}/cancel_merge_when_pipeline_succeeds",
            base(repo)
        ),
        None,
    ))
    .map(|_| ())
    .map_err(Error::from)
}

pub struct Gitlab(pub Remote);

impl Provider for Gitlab {
    fn at(&self, text: &str) -> Result<At> {
        self.0.at(text)
    }

    fn key(&self, at: &At) -> String {
        self.0.key(at)
    }

    fn place(&self) -> String {
        self.0.origin.repo.clone()
    }

    fn allow_write(&self, what: &str) -> Result<()> {
        self.0.allow_write(what)
    }

    fn explain(&self, error: Error) -> Error {
        remote::explain(error)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            resumes_auto_merge: false,
            no_text: None,
        }
    }

    fn view(&self, at: &At) -> Result<Pr> {
        let (t, repo) = self.0.api();
        view(t, repo, remote::number(at)?)
    }

    fn list(&self, filter: &Filter) -> Result<Vec<Pr>> {
        let (t, repo) = self.0.api();
        list(t, repo, filter.branch, filter.state, filter.mine)
    }

    fn create(&self, new: &NewPr) -> Result<At> {
        let (t, repo) = self.0.api();
        let number = create(t, repo, new.head, new.base, new.title, new.body, new.draft)?;
        Ok(At::Number(number))
    }

    fn update(&self, current: &Pr, changes: &Changes) -> Result<()> {
        let (t, repo) = self.0.api();
        update(t, repo, current, changes)
    }

    fn uploader(&self) -> Result<Uploader<'_>> {
        let (t, repo) = self.0.api();
        let host = self.0.origin.host.as_str();
        Ok(Box::new(move |image| upload(t, host, repo, image)))
    }

    fn merge(&self, pr: &Pr, sha: &str, how: Option<How>) -> Result<Outcome> {
        let (t, repo) = self.0.api();
        merge(t, repo, pr, sha, how, false)?;
        Ok(Outcome::Done)
    }

    fn enable_auto(&self, pr: &Pr, sha: &str, how: Option<How>) -> Result<Outcome> {
        let (t, repo) = self.0.api();
        merge(t, repo, pr, sha, how, true)?;
        Ok(Outcome::Done)
    }

    fn disable_auto(&self, pr: &Pr) -> Result<bool> {
        let (t, repo) = self.0.api();
        disable_auto(t, repo, pr.forge_number()).map(|()| true)
    }

    fn auto_method(&self, pr: &Pr, _recorded: Option<&str>) -> Option<String> {
        pr.auto_merge.clone()
    }
}
