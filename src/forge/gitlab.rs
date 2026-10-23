pub mod ci;
mod issues;

use serde_json::Value;

use crate::core::clock;
use crate::core::error::{Error, Result};
use crate::forge::anchors;
use crate::forge::service::{
    ApproveCap, Capabilities, DraftPoster, Inline, Kind, Method, Pagination, Placement, Position,
    Posted, ReplyCap, Request, ReviewService, ReviewSubject, Service,
};
use crate::forge::transport::{Cli, CliTransport, Transport};
use crate::forge::{self, CommentData, Draft, Fetched, PrInfo, PullNode, ThreadData};
use crate::git::Repo;
use crate::store::model::{Anchor, Forge, RemoteKind, RemoteThread, Review, ReviewState, Side};
use serde_json::json;

pub const KIND: Kind = Kind {
    forge: Forge::Gitlab,
    name: "GitLab",
    head_ref_prefix: "refs/merge-requests/",
    key_prefix: "gl",
    tool: "glab",
    url_marker: "/-/merge_requests/",
    pipeline_marker: Some("/-/pipelines/"),
    commit_marker: "/-/commit/",
    well_known: |host| host == "gitlab.com",
    configured: |host| {
        env_host().is_some_and(|name| name == host) || cli_host().is_some_and(|name| name == host)
    },
    // `glab config get host` names only the global default, not every host glab is logged in to.
    probe: |host| {
        Cli::glab(host)
            .text(&["auth", "status", "--hostname", host])
            .is_some()
    },
    connect: |host| Box::new(connect(host)),
    issue_caps: &issues::ISSUE_CAPS,
    connect_issue: |host| Box::new(connect(host)),
};

pub fn connect(host: &str) -> Gitlab {
    Gitlab::new(Box::new(CliTransport::glab(host)))
}

/// The shared fields of a REST merge request node.
pub fn pull_node(node: &Value) -> PullNode {
    let text = |pointer: &str| forge::text(node, pointer).unwrap_or_default();
    PullNode {
        number: node["iid"].as_u64(),
        url: text("/web_url"),
        title: text("/title"),
        state: node["state"].as_str().and_then(forge::gitlab_state),
        merged: node["merged_at"].as_str().is_some(),
        closed_at: forge::text(node, "/closed_at"),
        base: text("/target_branch"),
        head_branch: text("/source_branch"),
        head_sha: text("/sha"),
        same_repo: match (
            node["source_project_id"].as_u64(),
            node["target_project_id"].as_u64(),
        ) {
            (Some(source), Some(target)) => Some(source == target),
            _ => None,
        },
    }
}

const CAPS: Capabilities = Capabilities {
    pagination: Pagination::CliPaginate,
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

/// The host a `GITLAB_HOST`-style value names, if it names one.
fn named_host(value: Option<String>) -> Option<String> {
    value
        .map(|value| bare_host(&value))
        .filter(|host| !host.is_empty())
}

/// The host `GITLAB_HOST` names.
fn env_host() -> Option<String> {
    named_host(std::env::var("GITLAB_HOST").ok())
}

/// The host `glab config get host` prints.
fn cli_host() -> Option<String> {
    named_host(Cli::glab_configured().text(&["config", "get", "host"]))
}

/// The GitLab host the user configured: `GITLAB_HOST`, else glab's own configuration.
pub fn configured_host() -> Option<String> {
    env_host().or_else(cli_host)
}

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

/// The API path of a project by its path with namespace.
pub fn project_path(repo: &str) -> String {
    format!("projects/{}", forge::encode(repo))
}

fn base(subject: ReviewSubject) -> String {
    format!(
        "{}/merge_requests/{}",
        project_path(subject.repo),
        subject.number
    )
}

impl Service for Gitlab {
    fn forge(&self) -> Forge {
        Forge::Gitlab
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
}

impl ReviewService for Gitlab {
    fn find_review(&self, origin: &str, branch: &str) -> Result<Option<(String, u64)>> {
        find_mr(self.transport(), origin, branch)
    }

    fn fetch(&self, subject: ReviewSubject) -> Result<Fetched> {
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

    fn inline(&self) -> Inline<'_> {
        Inline::DraftThenPublish(self)
    }

    fn thread(&self, subject: ReviewSubject, text: &str) -> Request {
        Request::rest(
            Method::Post,
            format!("{}/discussions", base(subject)),
            Some(json!({ "body": text })),
        )
    }

    // A discussion on the whole file; GitLab before the `file` position type refuses it.
    fn file_thread(
        &self,
        subject: ReviewSubject,
        review: &Review,
        old_path: &str,
        path: &str,
        text: &str,
    ) -> Option<Request> {
        let base_sha = review.base_sha.as_deref()?;
        Some(Request::rest(
            Method::Post,
            format!("{}/discussions", base(subject)),
            Some(json!({
                "body": text,
                "position": {
                    "position_type": "file",
                    "base_sha": base_sha,
                    "start_sha": review.start_sha.as_deref().unwrap_or(base_sha),
                    "head_sha": review.head_sha,
                    "old_path": old_path,
                    "new_path": path,
                },
            })),
        ))
    }

    // A note, not a discussion: discussions are resolvable and can block merging.
    fn note(&self, subject: ReviewSubject, text: &str) -> Request {
        Request::rest(
            Method::Post,
            format!("{}/notes", base(subject)),
            Some(json!({ "body": text })),
        )
    }

    fn edit_note(&self, subject: ReviewSubject, id: &str, text: &str) -> Request {
        Request::rest(
            Method::Put,
            format!("{}/notes/{}", base(subject), forge::encode(id)),
            Some(json!({ "body": text })),
        )
    }

    fn reply(&self, subject: ReviewSubject, remote: &RemoteThread, text: &str) -> Request {
        Request::rest(
            Method::Post,
            format!("{}/discussions/{}/notes", base(subject), remote.id),
            Some(json!({ "body": text })),
        )
    }

    fn resolve(&self, subject: ReviewSubject, remote: &RemoteThread, resolved: bool) -> Request {
        Request::rest(
            Method::Put,
            format!("{}/discussions/{}", base(subject), remote.id),
            Some(json!({ "resolved": resolved })),
        )
    }

    fn approve(&self, subject: ReviewSubject, head: &str, _comment: Option<&str>) -> Request {
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

    fn review_posted(
        &self,
        _subject: ReviewSubject,
        _response: &Value,
    ) -> Result<(String, Vec<Posted>)> {
        Err(Error("GitLab takes no review batches".into()))
    }
}

impl DraftPoster for Gitlab {
    fn draft(&self, subject: ReviewSubject, body: &str, position: &Position) -> Request {
        Request::rest(
            Method::Post,
            format!("{}/draft_notes", base(subject)),
            Some(json!({ "note": body, "position": position.data })),
        )
    }

    fn publish_draft(&self, subject: ReviewSubject, draft: &str) -> Request {
        Request::rest(
            Method::Put,
            format!("{}/draft_notes/{draft}/publish", base(subject)),
            None,
        )
    }

    fn delete_draft(&self, subject: ReviewSubject, draft: &str) -> Request {
        Request::rest(
            Method::Delete,
            format!("{}/draft_notes/{draft}", base(subject)),
            None,
        )
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
    let mut position = json!({
        "position_type": "text",
        "base_sha": base,
        "start_sha": start,
        "head_sha": head,
    });
    if let Some(found) = anchors::diff_line(&diff, &path, anchor.side, line) {
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
        position["old_path"] = json!(found.old_path);
        position["new_path"] = json!(found.new_path);
        if let Some(new) = found.new_line {
            position["new_line"] = json!(new);
        }
        if let Some(old) = found.old_line {
            position["old_line"] = json!(old);
        }
        if let Some(range) = range {
            position["line_range"] = range;
        }
    } else {
        // An unchanged line outside every hunk of a changed file: GitLab takes it with both
        // numbers, and the host rejects it where it does not expand the diff that far.
        if anchor.side == Side::Old {
            return None;
        }
        let files = anchors::parse(&repo.diff_u0(base, head).ok()?);
        let (old_path, old_line) = anchors::old_of_unchanged(&files, &path, line)?;
        position["old_path"] = json!(old_path);
        position["new_path"] = json!(path);
        position["old_line"] = json!(old_line);
        position["new_line"] = json!(line);
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
    let project = glab.json(&Request::get(project_path(origin)))?;
    let own_id = project["id"].as_u64();
    let search = |target: &str| -> Result<Option<u64>> {
        let path = format!(
            "{}/merge_requests?source_branch={branch}&state=all&per_page=20",
            project_path(target)
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
    forge::in_origin_or_parent(origin, search, || {
        Ok(forge::text(
            &project,
            "/forked_from_project/path_with_namespace",
        ))
    })
}

fn state(text: &str) -> ReviewState {
    forge::gitlab_state(text).unwrap_or(ReviewState::Open)
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

/// Every discussion under `base` (an MR, or an issue, which has the same discussion shape).
fn discussions(glab: &dyn Transport, base: &str, url: &str) -> Result<Vec<ThreadData>> {
    let items = glab.items(
        &Request::get(format!("{base}/discussions?per_page=100")),
        &CAPS.pagination,
    )?;
    let mut threads = Vec::new();
    for item in &items {
        threads.extend(discussion(item, url)?);
    }
    Ok(threads)
}

fn fetch(glab: &dyn Transport, project: &str, iid: u64) -> Result<Fetched> {
    let base = format!("{}/merge_requests/{iid}", project_path(project));
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
    let threads = discussions(glab, &base, &url)?;
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::forge::contract::{self, Replay};
    use crate::forge::transport::glab_args;

    #[test]
    fn a_url_style_host_is_reduced_to_the_host() {
        let named = |value: &str| named_host(Some(value.to_owned()));
        assert_eq!(
            named("https://GitLab.Example.com/"),
            Some("gitlab.example.com".to_owned())
        );
        assert_eq!(
            named("gitlab.example.com"),
            Some("gitlab.example.com".to_owned())
        );
        assert_eq!(named(""), None);
        assert_eq!(named_host(None), None);
    }

    const VARS: [(&str, &str); 7] = [
        ("RUN", "rv-test"),
        ("HEAD", "1111111111111111111111111111111111111111"),
        ("BASE", "2222222222222222222222222222222222222222"),
        ("OLD", "3333333333333333333333333333333333333333"),
        ("HOST", "gitlab.example.com"),
        ("REPO", "group/sub/proj"),
        ("AUTHOR", "me"),
    ];
    const MR: &str = "projects/group%2Fsub%2Fproj/merge_requests/3";

    fn gitlab() -> Gitlab {
        let list = |name: &str| format!("{MR}/{name}?per_page=100");
        let replay = Replay::new(glab_args)
            .answer(
                &["api", "-X", "GET", MR],
                contract::fixture("gitlab/mr.json", &VARS),
            )
            .answer(
                &["api", "-X", "GET", "--paginate", &list("discussions")],
                contract::fixture("gitlab/discussions.json", &VARS),
            )
            .answer(
                &["api", "-X", "GET", "--paginate", &list("draft_notes")],
                contract::fixture("gitlab/drafts.json", &VARS),
            );
        Gitlab::new(Box::new(replay))
    }

    fn subject() -> ReviewSubject<'static> {
        ReviewSubject {
            repo: "group/sub/proj",
            number: 3,
        }
    }

    #[test]
    fn capabilities_match_the_contract() {
        contract::golden(
            "gitlab/capabilities.json",
            &contract::capabilities(&gitlab()),
        );
    }

    /// System notes dropped, individual notes general, positions kept with their head, and a
    /// draft note, as the contract fixtures hold them.
    #[test]
    fn fetch_reads_the_contract_fixtures() {
        let fetched = gitlab().fetch(subject()).unwrap();
        contract::golden("gitlab/fetched.golden.json", &contract::dump(&fetched));
    }

    #[test]
    fn requests_render_to_the_canonical_argv() {
        let gitlab = gitlab();
        let thread = RemoteThread::new(RemoteKind::Discussion, "d-f1".to_owned(), true);
        let position = Position {
            commit: "c".to_owned(),
            data: json!({ "position_type": "text", "new_path": "a.txt", "new_line": 3 }),
        };
        let all = [
            ("draft", gitlab.draft(subject(), "text", &position)),
            ("publish_draft", gitlab.publish_draft(subject(), "77")),
            ("delete_draft", gitlab.delete_draft(subject(), "77")),
            ("thread", gitlab.thread(subject(), "text")),
            ("note", gitlab.note(subject(), "text")),
            ("reply", gitlab.reply(subject(), &thread, "text")),
            ("resolve", gitlab.resolve(subject(), &thread, true)),
            ("unresolve", gitlab.resolve(subject(), &thread, false)),
            ("approve", gitlab.approve(subject(), "c", Some("ok"))),
        ];
        contract::golden(
            "gitlab/requests.golden.json",
            &contract::requests(glab_args, &all),
        );
    }

    #[test]
    fn a_file_thread_on_a_renamed_file_names_both_paths() {
        let review = Review {
            forge: Forge::Gitlab,
            host: "gitlab.com".to_owned(),
            repo: "me/proj".to_owned(),
            number: 3,
            url: "https://gitlab.com/me/proj/-/merge_requests/3".to_owned(),
            state: ReviewState::Open,
            closed_at: None,
            head_branch: "feature".to_owned(),
            head_sha: "head".to_owned(),
            base_sha: Some("base".to_owned()),
            start_sha: None,
            author: None,
            login: "me".to_owned(),
            synced_at: String::new(),
            attached_at: None,
        };
        let request = gitlab()
            .file_thread(subject(), &review, "old.txt", "new.txt", "text")
            .unwrap();
        let position = &request.body.unwrap()["position"];
        assert_eq!(position["old_path"], "old.txt");
        assert_eq!(position["new_path"], "new.txt");
    }

    #[test]
    fn responses_to_writes_name_the_comment_they_created() {
        let gitlab = gitlab();
        let created = |response: Value| gitlab.comment_created(Some("https://h/mr/3"), &response);
        assert_eq!(
            created(json!({ "id": "d-f2", "notes": [{ "id": 5002 }] })),
            Some((
                "5002".to_owned(),
                Some("https://h/mr/3#note_5002".to_owned())
            ))
        );
        assert_eq!(
            created(json!({ "id": 5020 })),
            Some((
                "5020".to_owned(),
                Some("https://h/mr/3#note_5020".to_owned())
            ))
        );
        assert_eq!(created(json!({})), None);
        assert_eq!(
            gitlab.draft_created(&json!({ "id": 77 })),
            Some("77".to_owned())
        );
    }
}
