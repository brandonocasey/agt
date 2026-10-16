use serde_json::Value;

use crate::clock;
use crate::error::{Error, Result, fail};
use crate::forge::{self, Client, CommentData, Fetched, PrInfo, ThreadData};
use crate::model::{RemoteKind, RemoteThread, ReviewState, Side};

const COMMENT_FIELDS: &str = "databaseId url body createdAt author{login} editor{login} originalCommit{oid} pullRequestReview{databaseId}";

fn pull_query(name: &str, selection: &str) -> String {
    format!(
        "query {name}($owner:String!,$name:String!,$number:Int!,$cursor:String){{repository(owner:$owner,name:$name){{pullRequest(number:$number){{{selection}}}}}}}"
    )
}

pub fn login(gh: &Client) -> Result<String> {
    forge::required(
        &gh.json(&["api", "--method", "GET", "user"])?,
        "/login",
        "gh api user",
    )
}

/// Finds the PR whose head is `<origin owner>:<branch>`, in `origin` or, for a fork, its parent.
pub fn find_pr(gh: &Client, origin: &str, branch: &str) -> Result<Option<(String, u64)>> {
    let owner = origin.split('/').next().unwrap_or_default();
    let head = forge::encode(&format!("{owner}:{branch}"));
    let search = |repo: &str| -> Result<Option<u64>> {
        let path = format!("repos/{repo}/pulls?head={head}&state=all&per_page=20");
        let pulls = gh.json(&["api", "--method", "GET", &path])?;
        let pulls = pulls.as_array().cloned().unwrap_or_default();
        let open = pulls
            .iter()
            .find(|pull| pull["state"] == "open")
            .or(pulls.first());
        Ok(open.and_then(|pull| pull["number"].as_u64()))
    };
    if let Some(number) = search(origin)? {
        return Ok(Some((origin.to_owned(), number)));
    }
    let repo = gh.json(&["api", "--method", "GET", &format!("repos/{origin}")])?;
    let Some(parent) = forge::text(&repo, "/parent/full_name") else {
        return Ok(None);
    };
    Ok(search(&parent)?.map(|number| (parent, number)))
}

struct Pager<'a> {
    gh: &'a Client,
    owner: String,
    name: String,
    number: String,
}

impl Pager<'_> {
    /// Fetches every node of one connection; returns the first response and all nodes.
    fn pages(&self, query: &str, connection: &str) -> Result<(Value, Vec<Value>)> {
        let mut nodes = Vec::new();
        let mut first = None;
        let mut cursor: Option<String> = None;
        loop {
            let owner = format!("owner={}", self.owner);
            let name = format!("name={}", self.name);
            let number = format!("number={}", self.number);
            let query_arg = format!("query={query}");
            let mut args = vec![
                "api", "graphql", "-f", &owner, "-f", &name, "-F", &number, "-f", &query_arg,
            ];
            let cursor_arg = cursor.as_ref().map(|cursor| format!("cursor={cursor}"));
            if let Some(cursor_arg) = &cursor_arg {
                args.extend(["-f", cursor_arg]);
            }
            let response = self.gh.json(&args)?;
            let pointer = format!("/data/repository/pullRequest/{connection}");
            let page = response
                .pointer(&pointer)
                .ok_or_else(|| Error(format!("GitHub returned no {connection}")))?;
            nodes.extend(page["nodes"].as_array().cloned().unwrap_or_default());
            cursor = next_cursor(page);
            if first.is_none() {
                first = Some(response.clone());
            }
            if cursor.is_none() {
                break;
            }
        }
        Ok((first.unwrap_or_default(), nodes))
    }

    fn thread_comments(&self, thread: &str, cursor: String) -> Result<Vec<Value>> {
        let query = format!(
            "query ThreadComments($id:ID!,$cursor:String){{node(id:$id){{... on PullRequestReviewThread{{comments(first:100,after:$cursor){{pageInfo{{hasNextPage endCursor}} nodes{{{COMMENT_FIELDS}}}}}}}}}}}"
        );
        let mut nodes = Vec::new();
        let mut cursor = Some(cursor);
        while let Some(after) = cursor {
            let id = format!("id={thread}");
            let query_arg = format!("query={query}");
            let cursor_arg = format!("cursor={after}");
            let response = self.gh.json(&[
                "api",
                "graphql",
                "-f",
                &id,
                "-f",
                &query_arg,
                "-f",
                &cursor_arg,
            ])?;
            let page = response
                .pointer("/data/node/comments")
                .ok_or_else(|| Error(format!("GitHub returned no comments for {thread}")))?;
            nodes.extend(page["nodes"].as_array().cloned().unwrap_or_default());
            cursor = next_cursor(page);
        }
        Ok(nodes)
    }
}

fn next_cursor(page: &Value) -> Option<String> {
    if page.pointer("/pageInfo/hasNextPage") == Some(&Value::Bool(true)) {
        forge::text(page, "/pageInfo/endCursor")
    } else {
        None
    }
}

fn comment(node: &Value) -> Result<CommentData> {
    let created = forge::required(node, "/createdAt", "GitHub comment")?;
    Ok(CommentData {
        id: forge::id(node, "/databaseId")
            .ok_or_else(|| Error("GitHub comment has no databaseId".into()))?,
        url: forge::text(node, "/url"),
        body: forge::text(node, "/body").unwrap_or_default(),
        created_at: clock::normalize(&created)?,
        login: forge::text(node, "/author/login"),
        editor: forge::text(node, "/editor/login"),
        review_id: forge::id(node, "/pullRequestReview/databaseId"),
    })
}

fn state(text: &str) -> ReviewState {
    match text {
        "MERGED" => ReviewState::Merged,
        "CLOSED" => ReviewState::Closed,
        _ => ReviewState::Open,
    }
}

fn review_thread(pager: &Pager, node: &Value) -> Result<ThreadData> {
    let id = forge::required(node, "/id", "GitHub review thread")?;
    let mut nodes = node
        .pointer("/comments/nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(cursor) = next_cursor(&node["comments"]) {
        nodes.extend(pager.thread_comments(&id, cursor)?);
    }
    let comments = nodes.iter().map(comment).collect::<Result<Vec<_>>>()?;
    let side = match node["diffSide"].as_str() {
        Some("LEFT") => Some(Side::Old),
        Some("RIGHT") => Some(Side::New),
        _ => None,
    };
    let line = forge::number(node, "/line");
    let file_level = node["subjectType"] == "FILE";
    let commit = nodes
        .first()
        .and_then(|first| forge::text(first, "/originalCommit/oid"));
    Ok(ThreadData {
        info: RemoteThread {
            kind: RemoteKind::ReviewThread,
            id,
            url: comments.first().and_then(|first| first.url.clone()),
            resolvable: true,
            outdated: node["isOutdated"].as_bool(),
            path: forge::text(node, "/path"),
            old_path: None,
            line: if side == Some(Side::Old) || file_level {
                None
            } else {
                line
            },
            old_line: if side == Some(Side::Old) { line } else { None },
            original_line: forge::number(node, "/originalLine"),
            start_line: forge::number(node, "/startLine"),
            original_start_line: forge::number(node, "/originalStartLine"),
            side,
            commit,
            review_state: None,
        },
        resolved: node["isResolved"].as_bool().unwrap_or(false),
        comments,
        position_head: None,
    })
}

fn general(kind: RemoteKind, comment: CommentData, review_state: Option<String>) -> ThreadData {
    ThreadData {
        info: RemoteThread {
            kind,
            id: comment.id.clone(),
            url: comment.url.clone(),
            resolvable: false,
            outdated: None,
            path: None,
            old_path: None,
            line: None,
            old_line: None,
            original_line: None,
            start_line: None,
            original_start_line: None,
            side: None,
            commit: None,
            review_state,
        },
        resolved: false,
        comments: vec![comment],
        position_head: None,
    }
}

pub fn fetch(gh: &Client, repo: &str, number: u64) -> Result<Fetched> {
    let Some((owner, name)) = repo.split_once('/') else {
        return fail(format!("GitHub repository must be owner/name: {repo}"));
    };
    let pager = Pager {
        gh,
        owner: owner.to_owned(),
        name: name.to_owned(),
        number: number.to_string(),
    };
    let threads_query = pull_query(
        "PullRequest",
        &format!(
            "number url state closedAt headRefName headRefOid baseRefOid baseRepository{{nameWithOwner}} reviewThreads(first:50,after:$cursor){{pageInfo{{hasNextPage endCursor}} nodes{{id isResolved isOutdated path line originalLine startLine originalStartLine diffSide subjectType comments(first:50){{pageInfo{{hasNextPage endCursor}} nodes{{{COMMENT_FIELDS}}}}}}}}}"
        ),
    );
    let (first, thread_nodes) = pager.pages(&threads_query, "reviewThreads")?;
    let pull = first
        .pointer("/data/repository/pullRequest")
        .ok_or_else(|| Error(format!("GitHub has no PR {repo}#{number}")))?;
    let pr = PrInfo {
        repo: forge::text(pull, "/baseRepository/nameWithOwner").unwrap_or_else(|| repo.to_owned()),
        url: forge::required(pull, "/url", "GitHub PR")?,
        state: state(pull["state"].as_str().unwrap_or_default()),
        closed_at: forge::text(pull, "/closedAt")
            .map(|time| clock::normalize(&time))
            .transpose()?,
        head_branch: forge::required(pull, "/headRefName", "GitHub PR")?,
        head_sha: forge::required(pull, "/headRefOid", "GitHub PR")?,
        base_sha: forge::text(pull, "/baseRefOid"),
        start_sha: None,
    };
    let mut threads = thread_nodes
        .iter()
        .map(|node| review_thread(&pager, node))
        .collect::<Result<Vec<_>>>()?;

    let reviews_query = pull_query(
        "PullReviews",
        "reviews(first:100,after:$cursor){pageInfo{hasNextPage endCursor} nodes{databaseId url body state submittedAt author{login} editor{login}}}",
    );
    for node in pager.pages(&reviews_query, "reviews")?.1 {
        let body = forge::text(&node, "/body").unwrap_or_default();
        let Some(submitted) = forge::text(&node, "/submittedAt") else {
            continue;
        };
        if body.trim().is_empty() {
            continue;
        }
        let review = CommentData {
            id: forge::id(&node, "/databaseId")
                .ok_or_else(|| Error("GitHub review has no databaseId".into()))?,
            url: forge::text(&node, "/url"),
            body,
            created_at: clock::normalize(&submitted)?,
            login: forge::text(&node, "/author/login"),
            editor: forge::text(&node, "/editor/login"),
            review_id: None,
        };
        threads.push(general(
            RemoteKind::Review,
            review,
            forge::text(&node, "/state"),
        ));
    }

    let comments_query = pull_query(
        "PullComments",
        "comments(first:100,after:$cursor){pageInfo{hasNextPage endCursor} nodes{databaseId url body createdAt author{login} editor{login}}}",
    );
    for node in pager.pages(&comments_query, "comments")?.1 {
        threads.push(general(RemoteKind::General, comment(&node)?, None));
    }
    Ok(Fetched {
        pr,
        threads,
        drafts: Vec::new(),
    })
}
