use std::io::Read;
use std::path::Path;

use crate::error::{Result, ctx, fail};
use crate::model::{Anchor, Side};

use super::MAX_TEXT;

pub(super) fn limited(text: String, name: &str) -> Result<String> {
    if text.trim().is_empty() {
        return fail(format!("{name} must not be empty"));
    }
    if text.len() > MAX_TEXT {
        return fail(format!("{name} exceeds {MAX_TEXT} bytes"));
    }
    Ok(text)
}

pub(super) fn read_stdin(what: &str) -> Result<String> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(ctx(format!("read {what} from stdin")))?;
    Ok(text)
}

pub(super) fn read_body(body: &str) -> Result<String> {
    if body == "-" {
        return limited(read_stdin("body")?, "body");
    }
    limited(body.to_owned(), "body")
}

pub(super) fn validate_path(path: &str) -> Result<()> {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.contains('\0')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..");
    if bad {
        return fail(format!(
            "path must be relative to the repository root: {path}"
        ));
    }
    Ok(())
}

pub(super) struct AnchorInput<'a> {
    pub(super) path: Option<&'a str>,
    pub(super) line: Option<u32>,
    pub(super) old_line: Option<u32>,
    pub(super) side: Option<Side>,
    pub(super) start_line: Option<u32>,
}

pub(super) fn build_anchor(
    input: &AnchorInput,
    commit: Option<String>,
    tree: String,
) -> Result<Option<Anchor>> {
    let Some(path) = input.path else {
        if input.line.is_some()
            || input.old_line.is_some()
            || input.side.is_some()
            || input.start_line.is_some()
        {
            return fail("line, old_line, side and start_line need a path");
        }
        return Ok(None);
    };
    validate_path(path)?;
    if [input.line, input.old_line, input.start_line].contains(&Some(0)) {
        return fail("line numbers start at 1");
    }
    let side = input
        .side
        .unwrap_or(if input.line.is_none() && input.old_line.is_some() {
            Side::Old
        } else {
            Side::New
        });
    match side {
        Side::Old if input.old_line.is_none() => return fail("side old needs old_line"),
        Side::Old if input.start_line.is_some() => {
            return fail("start_line is for new-side ranges; side old takes one old_line");
        }
        Side::New if input.line.is_none() && input.start_line.is_some() => {
            return fail("start_line needs line");
        }
        _ => {}
    }
    if let (Some(start), Some(line)) = (input.start_line, input.line)
        && start > line
    {
        return fail("start_line must not be after line");
    }
    Ok(Some(Anchor {
        path: path.to_owned(),
        line: input.line,
        old_line: input.old_line,
        side,
        start_line: input.start_line,
        commit,
        tree,
    }))
}

pub(super) fn read_source(source: &Path, what: &str) -> Result<String> {
    if source == Path::new("-") {
        read_stdin(what)
    } else {
        std::fs::read_to_string(source).map_err(ctx(format!("read {}", source.display())))
    }
}
