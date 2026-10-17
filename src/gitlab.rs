use serde_json::Value;

use crate::anchors;
use crate::clock;
use crate::error::{Error, Result};
use crate::forge::{self, CommentData, Draft, Fetched, PrInfo, ThreadData};
use crate::git::Repo;
use crate::model::{Anchor, Forge, RemoteKind, RemoteThread, Review, ReviewState, Side};
use crate::service::{
    ApproveCap, Capabilities, Inline, Kind, Method, Pagination, Placement, Position, Posted,
    ReplyCap, Request, Service, Subject,
};
use crate::transport::{Cli, CliTransport, Transport};
use serde_json::json;

pub const KIND: Kind = Kind {
    forge: Forge::Gitlab,
    key_prefix: "gl",
    url_marker: "/-/merge_requests/",
    well_known: |host| host == "gitlab.com",
    configured: |host| {
        let named = |value: &str| bare_host(value) == host;
        std::env::var("GITLAB_HOST").is_ok_and(|value| named(&value))
            || Cli::glab_configured()
                .text(&["config", "get", "host"])
                .is_some_and(|value| named(&value))
    },
    // `glab config get host` names only the global default, not every host glab is logged in to.
    probe: |host| {
        Cli::glab(host)
            .text(&["auth", "status", "--hostname", host])
            .is_some()
    },
    connect: |host| Box::new(Gitlab::new(Box::new(CliTransport::glab(host)))),
};

const CAPS: Capabilities = Capabilities {
    pagination: Pagination::CliPaginate,
    inline: Inline::DraftThenPublish,
    ranges: true,
    reply: ReplyCap::All,
    resolve: true,
    unresolve: true,
    general_kind: RemoteKind::Discussion,
    general_resolvable: true,
    approve: ApproveCap::Call,
    edits_attributed: false,
    notes_become_discussions: true,
};

/// The lowercase host of a `GITLAB_HOST`-style value, which may be a URL.
fn bare_host(value: &str) -> String {
    let value = value.split_once("://").map_or(value, |(_, rest)| rest);
    value
        .split('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

pub struct Gitlab {
    transport: Box<dyn Transport>,
}

impl Gitlab {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self { transport }
    }
}

fn base(subject: Subject) -> String {
    format!(
        "projects/{}/merge_requests/{}",
        forge::encode(subject.repo),
        subject.number
    )
}

impl Service for Gitlab {
    fn forge(&self) -> Forge {
        Forge::Gitlab
    }

    fn name(&self) -> &'static str {
        "GitLab"
    }

    fn caps(&self) -> &Capabilities {
        &CAPS
    }

    fn transport(&self) -> &dyn Transport {
        self.transport.as_ref()
    }

    fn login(&self) -> Result<String> {
        login(self.transport())
    }

    fn find_review(&self, origin: &str, branch: &str) -> Result<Option<(String, u64)>> {
        find_mr(self.transport(), origin, branch)
    }

    fn fetch(&self, subject: Subject) -> Result<Fetched> {
        fetch(self.transport(), subject.repo, subject.number)
    }

    fn imported_id(&self, _kind: RemoteKind, first: &str) -> String {
        format!("gl-n{first}")
    }

    fn place(
        &self,
        repo: &Repo,
        review: &Review,
        anchor: &Anchor,
        commit: &str,
        line: u32,
    ) -> Option<Placement> {
        place(repo, review, anchor, commit, line)
    }

    fn draft(&self, subject: Subject, body: &str, position: &Position) -> Request {
        Request::rest(
            Method::Post,
            format!("{}/draft_notes", base(subject)),
            Some(json!({ "note": body, "position": position.data })),
        )
    }

    fn publish_draft(&self, subject: Subject, draft: &str) -> Request {
        Request::rest(
            Method::Put,
            format!("{}/draft_notes/{draft}/publish", base(subject)),
            None,
        )
    }

    fn thread(&self, subject: Subject, text: &str) -> Request {
        Request::rest(
            Method::Post,
            format!("{}/discussions", base(subject)),
            Some(json!({ "body": text })),
        )
    }

    // A note, not a discussion: discussions are resolvable and can block merging.
    fn note(&self, subject: Subject, text: &str) -> Request {
        Request::rest(
            Method::Post,
            format!("{}/notes", base(subject)),
            Some(json!({ "body": text })),
        )
    }

    fn reply(&self, subject: Subject, remote: &RemoteThread, text: &str) -> Request {
        Request::rest(
            Method::Post,
            format!("{}/discussions/{}/notes", base(subject), remote.id),
            Some(json!({ "body": text })),
        )
    }

    fn resolve(&self, subject: Subject, remote: &RemoteThread, resolved: bool) -> Request {
        Request::rest(
            Method::Put,
            format!("{}/discussions/{}", base(subject), remote.id),
            Some(json!({ "resolved": resolved })),
        )
    }

    fn approve(&self, subject: Subject, head: &str, _comment: Option<&str>) -> Request {
        Request::rest(
            Method::Post,
            format!("{}/approve", base(subject)),
            Some(json!({ "sha": head })),
        )
    }

    fn comment_created(
        &self,
        review_url: Option<&str>,
        response: &Value,
    ) -> Option<(String, Option<String>)> {
        let id = forge::id(response, "/notes/0/id").or_else(|| forge::id(response, "/id"))?;
        let url = review_url.map(|url| format!("{url}#note_{id}"));
        Some((id, url))
    }

    fn draft_created(&self, response: &Value) -> Option<String> {
        forge::id(response, "/id")
    }

    fn review_posted(&self, _subject: Subject, _response: &Value) -> Result<(String, Vec<Posted>)> {
        Err(Error("GitLab takes no review batches".into()))
    }
}

/// One end of a `line_range`: its line code is the SHA-1 of the path and the old and new line
/// counters, as GitLab builds it.
fn line_point(line: &anchors::DiffLine) -> Value {
    let hash = sha1_smol::Sha1::from(line.new_path.as_bytes()).digest();
    let kind = if line.old_line.is_none() {
        "new"
    } else {
        "old"
    };
    json!({
        "line_code": format!("{hash}_{}_{}", line.old_pos, line.new_pos),
        "type": kind,
        "old_line": line.old_line,
        "new_line": line.new_line,
    })
}

/// GitLab positions are against the latest MR diff, so a line is first mapped to its head.
fn place(
    repo: &Repo,
    review: &Review,
    anchor: &Anchor,
    commit: &str,
    line: u32,
) -> Option<Placement> {
    let head = &review.head_sha;
    let base = review.base_sha.as_deref()?;
    let start = review.start_sha.as_deref().unwrap_or(base);
    let first = anchor
        .start_line
        .filter(|first| anchor.side == Side::New && *first < line);
    let (path, line, first) = if anchor.side == Side::Old || commit == head {
        (anchor.path.clone(), line, first)
    } else {
        let files = anchors::parse(&repo.diff_u0(commit, head).ok()?);
        let location = anchors::locate(&files, anchor);
        // Changed since review, as by a fix: the latest MR diff no longer has these lines.
        if location.outdated {
            return Some(Placement::OtherFile);
        }
        // The range's first line maps to the head the same way, or the range is dropped.
        let first = match first {
            Some(first) => {
                let start = Anchor {
                    line: Some(first),
                    start_line: None,
                    ..anchor.clone()
                };
                let mapped = anchors::locate(&files, &start);
                if mapped.outdated {
                    return Some(Placement::OtherFile);
                }
                Some(mapped.line?)
            }
            None => None,
        };
        (location.path, location.line?, first)
    };
    let diff = repo.diff_context(base, head).ok()?;
    let found = anchors::diff_line(&diff, &path, anchor.side, line)?;
    let range = match first {
        Some(first) => {
            let start = anchors::diff_line(&diff, &path, Side::New, first)?;
            if start.hunk != found.hunk {
                return None;
            }
            Some(json!({ "start": line_point(&start), "end": line_point(&found) }))
        }
        None => None,
    };
    let mut position = json!({
        "position_type": "text",
        "base_sha": base,
        "start_sha": start,
        "head_sha": head,
        "old_path": found.old_path,
        "new_path": found.new_path,
    });
    if let Some(new) = found.new_line {
        position["new_line"] = json!(new);
    }
    if let Some(old) = found.old_line {
        position["old_line"] = json!(old);
    }
    if let Some(range) = range {
        position["line_range"] = range;
    }
    Some(Placement::InDiff(Position {
        commit: head.clone(),
        data: position,
    }))
}

fn login(glab: &dyn Transport) -> Result<String> {
    forge::required(
        &glab.json(&Request::get("user"))?,
        "/username",
        "glab api user",
    )
}

/// Finds the MR from `branch` of `origin`, in `origin` or, for a fork, in the project it was
/// forked from. MRs from other forks with the same branch name are skipped.
fn find_mr(glab: &dyn Transport, origin: &str, branch: &str) -> Result<Option<(String, u64)>> {
    let branch = forge::encode(branch);
    let project = glab.json(&Request::get(format!("projects/{}", forge::encode(origin))))?;
    let own_id = project["id"].as_u64();
    let search = |target: &str| -> Result<Option<u64>> {
        let path = format!(
            "projects/{}/merge_requests?source_branch={branch}&state=all&per_page=20",
            forge::encode(target)
        );
        let mrs: Vec<Value> = glab
            .json(&Request::get(path))?
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|mr| mr["source_project_id"].as_u64() == own_id)
            .collect();
        Ok(mrs
            .iter()
            .find(|mr| mr["state"] == "opened")
            .or(mrs.first())
            .and_then(|mr| mr["iid"].as_u64()))
    };
    if let Some(iid) = search(origin)? {
        return Ok(Some((origin.to_owned(), iid)));
    }
    let Some(parent) = forge::text(&project, "/forked_from_project/path_with_namespace") else {
        return Ok(None);
    };
    Ok(search(&parent)?.map(|iid| (parent, iid)))
}

fn state(text: &str) -> ReviewState {
    match text {
        "merged" => ReviewState::Merged,
        "closed" => ReviewState::Closed,
        _ => ReviewState::Open,
    }
}

fn note(note: &Value, mr_url: &str) -> Result<CommentData> {
    let id = forge::id(note, "/id").ok_or_else(|| Error("GitLab note has no id".into()))?;
    let created = forge::required(note, "/created_at", "GitLab note")?;
    Ok(CommentData {
        url: Some(format!("{mr_url}#note_{id}")),
        id,
        body: forge::text(note, "/body").unwrap_or_default(),
        created_at: clock::normalize(&created)?,
        login: forge::text(note, "/author/username"),
        editor: None,
        review_id: None,
    })
}

fn discussion(discussion: &Value, mr_url: &str) -> Result<Option<ThreadData>> {
    let notes: Vec<&Value> = discussion["notes"]
        .as_array()
        .map(|notes| notes.iter().filter(|note| note["system"] != true).collect())
        .unwrap_or_default();
    let Some(first) = notes.first() else {
        return Ok(None);
    };
    let comments = notes
        .iter()
        .map(|item| note(item, mr_url))
        .collect::<Result<Vec<_>>>()?;
    let position = &first["position"];
    let line = forge::number(position, "/new_line");
    let old_line = forge::number(position, "/old_line");
    let side = match (line, old_line) {
        (Some(_), _) => Some(Side::New),
        (None, Some(_)) => Some(Side::Old),
        (None, None) => None,
    };
    let individual = discussion["individual_note"] == true;
    let resolvable = first["resolvable"] == true;
    Ok(Some(ThreadData {
        info: RemoteThread {
            kind: if individual {
                RemoteKind::General
            } else {
                RemoteKind::Discussion
            },
            id: forge::required(discussion, "/id", "GitLab discussion")?,
            url: comments.first().and_then(|comment| comment.url.clone()),
            resolvable,
            outdated: None,
            path: forge::text(position, "/new_path"),
            old_path: forge::text(position, "/old_path"),
            line,
            old_line,
            original_line: line,
            start_line: forge::number(position, "/line_range/start/new_line"),
            original_start_line: None,
            side,
            commit: forge::text(position, "/head_sha"),
            review_state: None,
        },
        resolved: resolvable && first["resolved"] == true,
        comments,
        position_head: forge::text(position, "/head_sha"),
    }))
}

fn fetch(glab: &dyn Transport, project: &str, iid: u64) -> Result<Fetched> {
    let base = format!("projects/{}/merge_requests/{iid}", forge::encode(project));
    let mr = glab.json(&Request::get(base.clone()))?;
    let url = forge::required(&mr, "/web_url", "GitLab MR")?;
    let closed_at = forge::text(&mr, "/merged_at")
        .or_else(|| forge::text(&mr, "/closed_at"))
        .map(|time| clock::normalize(&time))
        .transpose()?;
    let pr = PrInfo {
        repo: project.to_owned(),
        state: state(mr["state"].as_str().unwrap_or_default()),
        closed_at,
        head_branch: forge::required(&mr, "/source_branch", "GitLab MR")?,
        head_sha: forge::text(&mr, "/diff_refs/head_sha")
            .or_else(|| forge::text(&mr, "/sha"))
            .ok_or_else(|| Error("GitLab MR has no head sha".into()))?,
        base_sha: forge::text(&mr, "/diff_refs/base_sha"),
        start_sha: forge::text(&mr, "/diff_refs/start_sha"),
        author: forge::text(&mr, "/author/username"),
        url: url.clone(),
    };
    let discussions = glab.items(
        &Request::get(format!("{base}/discussions?per_page=100")),
        &CAPS.pagination,
    )?;
    let mut threads = Vec::new();
    for item in &discussions {
        threads.extend(discussion(item, &url)?);
    }
    let drafts = glab
        .items(
            &Request::get(format!("{base}/draft_notes?per_page=100")),
            &CAPS.pagination,
        )?
        .iter()
        .filter_map(|draft| {
            Some(Draft {
                id: forge::id(draft, "/id")?,
                body: forge::text(draft, "/note")?,
            })
        })
        .collect();
    Ok(Fetched {
        pr,
        threads,
        drafts,
    })
}
