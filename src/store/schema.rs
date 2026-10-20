//! Target documents: schema versions and their migrations, which files are targets, and reads
//! and writes that refuse to lose data this build does not understand.

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::json::{lost_field, persist_json};
use crate::core::error::{Result, ctx, fail};
use crate::core::local::read_json;
use crate::store::model::{Target, VERSION};

const _: () = assert!(REVIEW_MIGRATIONS.len() + 1 == VERSION as usize);
pub(super) const TARGET_KINDS: [&str; 3] = ["branch", "range", "commit"];

/// The current schema version of a namespace and the steps that bring older documents to it.
#[derive(Clone, Copy)]
pub(super) struct Schema {
    pub(super) version: u32,
    pub(super) migrations: &'static [fn(&mut Value)],
}

/// Whether the store file holds a target. Other documents may share a repository directory; a
/// file whose `kind` is not a target kind belongs to them and is left alone.
pub(super) fn is_target_document(document: &Value) -> bool {
    match document.get("kind").and_then(Value::as_str) {
        Some(kind) => TARGET_KINDS.contains(&kind),
        None => true,
    }
}

pub(super) fn is_target_file(path: &Path) -> Result<bool> {
    Ok(read_json::<Value>(path)?.is_some_and(|document| is_target_document(&document)))
}

/// Reads the target a caller asked for by key. A file of another document kind is an error so
/// that a write never replaces it.
pub(super) fn read_target(path: &Path, schema: Schema) -> Result<Option<Target>> {
    match read_json::<Value>(path)? {
        Some(document) => parse_target(path, document, schema).map(|(target, _)| Some(target)),
        None => Ok(None),
    }
}

/// Like [`read_target`], but the inner `Err` names the first field this build does not know, for
/// a caller that must keep such a file as it is.
pub(super) fn read_target_intact(
    path: &Path,
    schema: Schema,
) -> Result<Option<std::result::Result<Target, String>>> {
    let Some(document) = read_json::<Value>(path)? else {
        return Ok(None);
    };
    if !is_target_document(&document) {
        return Ok(None);
    }
    let (target, document) = parse_target(path, document, schema)?;
    Ok(Some(match lost_field_of(path, &document, &target)? {
        Some(field) => Err(field),
        None => Ok(target),
    }))
}

/// Like [`read_target`], for a caller that will write the target back. Refuses a file holding
/// fields this build does not know, because writing it would silently drop them.
pub(super) fn read_target_for_write(path: &Path, schema: Schema) -> Result<Option<Target>> {
    let Some(document) = read_json::<Value>(path)? else {
        return Ok(None);
    };
    let (target, document) = parse_target(path, document, schema)?;
    refuse_lost_field(path, &document, &target)?;
    Ok(Some(target))
}

/// The first field of `document` that `value` would not write back.
pub(super) fn lost_field_of<T: Serialize>(
    path: &Path,
    document: &Value,
    value: &T,
) -> Result<Option<String>> {
    let written = serde_json::to_value(value).map_err(ctx(format!("encode {}", path.display())))?;
    Ok(lost_field(document, &written, ""))
}

/// Fails when writing `value` back would drop a field of `document` that this build lacks.
pub(super) fn refuse_lost_field<T: Serialize>(
    path: &Path,
    document: &Value,
    value: &T,
) -> Result<()> {
    match lost_field_of(path, document, value)? {
        Some(field) => fail(format!(
            "{} has field {field} that this agt does not know; it was probably written by a newer agt, so upgrade agt before changing it",
            path.display()
        )),
        None => Ok(()),
    }
}

/// Parses a stored target, migrating older schema versions first. Returns the migrated
/// document alongside the target.
pub(super) fn parse_target(
    path: &Path,
    document: Value,
    schema: Schema,
) -> Result<(Target, Value)> {
    if !is_target_document(&document) {
        return fail(format!("{} is not a target document", path.display()));
    }
    parse_versioned(path, document, schema)
}

/// Parses a stored document of any namespace: its `version` must be one this build knows, and
/// older ones are migrated first. Returns the migrated document alongside the parsed value.
pub(super) fn parse_versioned<T: DeserializeOwned>(
    path: &Path,
    mut document: Value,
    schema: Schema,
) -> Result<(T, Value)> {
    let version = document.get("version").and_then(Value::as_u64);
    let version = match version.map(u32::try_from) {
        Some(Ok(version)) => version,
        _ => return fail(format!("{} has no valid schema version", path.display())),
    };
    if version > schema.version {
        return fail(format!(
            "{} uses schema version {version}, newer than the {} this agt (v{}) supports; upgrade agt to a release that supports schema version {version}",
            path.display(),
            schema.version,
            env!("CARGO_PKG_VERSION"),
        ));
    }
    if version == 0 {
        return fail(format!("{} has unsupported version 0", path.display()));
    }
    migrate_with(&mut document, version, schema.migrations);
    let value = serde_json::from_value(document.clone())
        .map_err(ctx(format!("parse {}", path.display())))?;
    Ok((value, document))
}

/// One step per schema version: `MIGRATIONS[n - 1]` rewrites a version `n` document into
/// version `n + 1`'s shape. Bumping [`VERSION`] means adding a step here and a frozen-JSON test.
pub(super) const REVIEW_MIGRATIONS: &[fn(&mut Value)] = &[];

pub(super) fn migrate_with(document: &mut Value, from: u32, steps: &[fn(&mut Value)]) {
    let from = from as usize;
    for step in steps.iter().skip(from - 1) {
        step(document);
    }
    let to = steps.len() as u32 + 1;
    if let Some(version) = document.get_mut("version") {
        *version = Value::from(to);
    }
}

pub(super) fn write_target(
    path: &Path,
    target: &Target,
    expected: Option<u64>,
    schema: Schema,
) -> Result<()> {
    persist_json(path, target, || {
        let on_disk = read_target(path, schema)?.map(|current| current.revision);
        if on_disk != expected {
            return fail(format!(
                "target {} changed during update; retry",
                target.key
            ));
        }
        Ok(())
    })
}
