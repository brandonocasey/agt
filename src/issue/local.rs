//! The commands that change only the local copy. Nothing here talks to a host.

use std::io::Read;

use serde_json::Value;

use super::cli::LinkArgs;
use super::trackers::{self, Tracker, doc_name};
use super::view::summary;
use crate::core::clock;
use crate::core::error::{Result, ctx, fail};
use crate::forge::{self, service};
use crate::store::Store;
use crate::store::issues::{Issue, LinkSource, Links, Note, PrLink};

const MAX_TEXT: usize = 65_536;

/// `text`, or stdin when it is `-`; never empty unless `allow_empty`.
fn read_text(text: &str, what: &str, allow_empty: bool) -> Result<String> {
    let text = if text == "-" {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .map_err(ctx(format!("read {what} from stdin")))?;
        input
    } else {
        text.to_owned()
    };
    if !allow_empty && text.trim().is_empty() {
        return fail(format!("{what} must not be empty"));
    }
    if text.len() > MAX_TEXT {
        return fail(format!("{what} exceeds {MAX_TEXT} bytes"));
    }
    Ok(text)
}

/// Changes the issue `key` names under its lock and reports the result.
fn change(
    store: &Store,
    filter: Option<&str>,
    key: &str,
    edit: impl FnOnce(&Tracker, &mut Issue) -> Result<()>,
) -> Result<Value> {
    let (tracker, issue) = trackers::find(store, filter, key)?;
    let now = clock::now()?;
    let name = doc_name(&issue);
    let updated = store.update_doc(
        &tracker.dir,
        &name,
        || issue.clone(),
        |issue| {
            edit(&tracker, issue)?;
            issue.fields.normalize();
            issue.updated_at.clone_from(&now);
            Ok(issue.clone())
        },
    )?;
    Ok(summary(&updated))
}

pub fn note(store: &Store, filter: Option<&str>, key: &str, text: &str) -> Result<Value> {
    let text = read_text(text, "note", false)?;
    let now = clock::now()?;
    change(store, filter, key, |_, issue| {
        let last = issue
            .notes
            .iter()
            .filter_map(|note| note.id.strip_prefix('n')?.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        issue.notes.push(Note {
            id: format!("n{}", last + 1),
            text,
            created_at: now,
        });
        Ok(())
    })
}

/// The PR/MR `arg` names as a link, in the repository of `tracker` unless a URL says otherwise.
fn pr_link(tracker: &Tracker, arg: &str, source: LinkSource) -> Result<PrLink> {
    let pr = forge::parse_pr(arg)?;
    let doc = &tracker.doc;
    let kind = service::kind(doc.forge);
    let repo = match &pr.location {
        Some((host, _)) if !host.eq_ignore_ascii_case(&doc.host) => {
            return fail(format!(
                "{arg} is on {host}, not on the tracker's host {}",
                doc.host
            ));
        }
        Some((_, repo)) => repo.clone(),
        None => doc.repo.clone(),
    };
    Ok(PrLink {
        forge: doc.forge,
        url: Some(format!(
            "https://{}/{repo}{}{}",
            doc.host, kind.url_marker, pr.number
        )),
        repo,
        number: pr.number,
        branch: None,
        state: None,
        source,
    })
}

fn add_pr(issue: &mut Issue, link: PrLink) -> bool {
    let known = issue
        .links
        .prs
        .iter()
        .any(|known| known.repo == link.repo && known.number == link.number);
    if !known {
        issue.links.prs.push(link);
    }
    !known
}

pub fn link(store: &Store, filter: Option<&str>, args: &LinkArgs) -> Result<Value> {
    change(store, filter, &args.key, |tracker, issue| {
        let links: &mut Links = &mut issue.links;
        let push = |list: &mut Vec<String>, value: &str| {
            if !list.iter().any(|known| known == value) {
                list.push(value.to_owned());
            }
        };
        match (
            &args.pr,
            &args.issue,
            &args.review,
            &args.task,
            &args.branch,
        ) {
            (Some(pr), ..) => {
                add_pr(issue, pr_link(tracker, pr, LinkSource::Local)?);
            }
            (_, Some(other), ..) => push(&mut links.issues, other),
            (_, _, Some(review), ..) => push(&mut links.reviews, review),
            (_, _, _, Some(task), _) => push(&mut links.tasks, task),
            (_, _, _, _, Some(branch)) => push(&mut links.branches, branch),
            _ => return fail("link needs one of --pr, --issue, --review, --task, --branch"),
        }
        Ok(())
    })
}
