//! `agt review sync`: pull, then push. Without `--pr` and `--target` it refreshes every review
//! already in the local store, on whichever tracker each belongs to, and never imports a PR/MR
//! the store does not have. `--pr` (or a `--target` with a review) names the one to sync, and
//! may start a new local review. Every synced review is then published when its tracker's
//! `push` is `manual` or `auto`; with `push = "off"` it stays local and the report says so.

use serde_json::{Value, json};

use super::{connection_of, sync, sync_with};
use crate::core::config::{Config, Kind};
use crate::core::error::{Result, fail};
use crate::forge::select::select_tracker;
use crate::review::commands::publish::{self, Options};
use crate::review::commands::{Ctx, Selection};

pub(in crate::review::commands) fn run(
    ctx: &Ctx,
    pr: Option<&str>,
    remote: Option<&str>,
) -> Result<Value> {
    let mut failed: Vec<String> = Vec::new();
    let mut pulled = Vec::new();
    let mut keys = Vec::new();
    if pr.is_some() || ctx.selection.target.is_some() {
        let value = sync(ctx, pr, remote)?;
        if let Some(key) = value["target"].as_str() {
            keys.push(key.to_owned());
        }
        pulled.push(value);
    } else {
        let config = Config::load_for_repo(&ctx.repo)?;
        let only = match remote {
            Some(_) => select_tracker(&ctx.repo, &config, Kind::PullRequests, remote, None)?,
            None => None,
        };
        let mut reviews: Vec<_> = ctx
            .store
            .targets(&ctx.dir)?
            .into_iter()
            .filter_map(|target| target.review.map(|review| (target.key, review)))
            .filter(|(_, review)| {
                only.as_ref().is_none_or(|selected| {
                    selected.host.eq_ignore_ascii_case(&review.host)
                        && selected.repo.eq_ignore_ascii_case(&review.repo)
                })
            })
            .collect();
        reviews.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, review) in reviews {
            let result = refresh(ctx, &key, &review);
            match result {
                Ok(value) => {
                    keys.push(value["target"].as_str().unwrap_or(&key).to_owned());
                    pulled.push(value);
                }
                Err(error) => {
                    eprintln!("agt: review sync {key}: {error}");
                    failed.push(format!("{key}: {error}"));
                }
            }
        }
    }
    let mut pushed = Vec::new();
    let mut kept = 0;
    for key in &keys {
        match push(ctx, key) {
            Ok(value) => {
                if value.get("kept_local").is_some() {
                    kept += 1;
                }
                pushed.push(value);
            }
            Err(error) => {
                eprintln!("agt: review publish {key}: {error}");
                failed.push(format!("{key}: {error}"));
            }
        }
    }
    if !failed.is_empty() {
        return fail(format!(
            "review sync failed for {} of its reviews: {}",
            failed.len(),
            failed.join("; ")
        ));
    }
    let mut report = if let [only] = pulled.as_slice() {
        only.clone()
    } else {
        json!({})
    };
    report["synced"] = json!(pulled);
    report["push"] = json!({
        "published": pushed,
        "kept_local": kept > 0 && kept == pushed.len(),
    });
    Ok(report)
}

/// Pulls the stored review of target `key`.
fn refresh(ctx: &Ctx, key: &str, review: &crate::store::model::Review) -> Result<Value> {
    let sub = sub_ctx(ctx, key, false)?;
    sync_with(&sub, &connection_of(review)?)
}

/// Publishes target `key` when its tracker sends; a review of someone else's PR/MR stays local.
fn push(ctx: &Ctx, key: &str) -> Result<Value> {
    let mut sub = sub_ctx(ctx, key, true)?;
    let review = sub
        .store
        .load(&sub.dir, key)?
        .and_then(|target| target.review);
    if let Some(review) = review
        && review.author.as_deref() != Some(review.login.as_str())
    {
        return Ok(json!({
            "target": key,
            "skipped": "not your PR/MR",
        }));
    }
    publish::publish(
        &mut sub,
        None,
        None,
        &Options {
            dry_run: false,
            not_own: false,
            only: Vec::new(),
            approval: None,
            run: None,
        },
    )
}

fn sub_ctx(ctx: &Ctx, key: &str, scratch_git: bool) -> Result<Ctx> {
    Ctx::new(
        Selection {
            target: Some(key.to_owned()),
            range: None,
            default_branch: ctx.selection.default_branch.clone(),
        },
        false,
        scratch_git,
        false,
    )
}
