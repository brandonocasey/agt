use serde_json::Value;

use crate::clock;
use crate::error::{Error, Result};
use crate::forge::{self, Client, CommentData, Draft, Fetched, PrInfo, ThreadData};
use crate::model::{RemoteKind, RemoteThread, ReviewState, Side};

pub fn login(glab: &Client) -> Result<String> {
    forge::required(
        &glab.json(&["api", "-X", "GET", "user"])?,
        "/username",
        "glab api user",
    )
}

/// Finds the MR from `branch` of `origin`, in `origin` or, for a fork, in the project it was
/// forked from. MRs from other forks with the same branch name are skipped.
pub fn find_mr(glab: &Client, origin: &str, branch: &str) -> Result<Option<(String, u64)>> {
    let branch = forge::encode(branch);
    let project = glab.json(&[
        "api",
        "-X",
        "GET",
        &format!("projects/{}", forge::encode(origin)),
    ])?;
    let own_id = project["id"].as_u64();
    let search = |target: &str| -> Result<Option<u64>> {
        let path = format!(
            "projects/{}/merge_requests?source_branch={branch}&state=all&per_page=20",
            forge::encode(target)
        );
        let mrs: Vec<Value> = glab
            .json(&["api", "-X", "GET", &path])?
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

pub fn fetch(glab: &Client, project: &str, iid: u64) -> Result<Fetched> {
    let base = format!("projects/{}/merge_requests/{iid}", forge::encode(project));
    let mr = glab.json(&["api", "-X", "GET", &base])?;
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
    let discussions = glab.items(&[
        "api",
        "-X",
        "GET",
        "--paginate",
        &format!("{base}/discussions?per_page=100"),
    ])?;
    let mut threads = Vec::new();
    for item in &discussions {
        threads.extend(discussion(item, &url)?);
    }
    let drafts = glab
        .items(&[
            "api",
            "-X",
            "GET",
            "--paginate",
            &format!("{base}/draft_notes?per_page=100"),
        ])?
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
