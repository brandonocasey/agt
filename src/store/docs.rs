//! Typed JSON documents for the namespaces that are not review targets (issues). A document is
//! a file `<name>.json` under a directory of the namespace, carries a `version`, and is
//! changed under the same per-file lock and atomic replace as a target.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::prune::{Removal, remove_with_lock};
use super::{
    Store, create_dir, ctx, fail, lock, lost_field, persist_json, read_document, try_lock,
    validate_key,
};
use crate::core::error::Result;

impl Store {
    /// The directories directly under the namespace root.
    pub fn child_dirs(&self) -> Result<Vec<PathBuf>> {
        let entries = self.read_entries(&self.root)?;
        let mut dirs = Vec::new();
        for entry in entries {
            let kind = entry
                .file_type()
                .map_err(ctx(format!("stat {}", entry.path().display())))?;
            if kind.is_dir() {
                dirs.push(self.root.join(entry.file_name()));
            }
        }
        dirs.sort();
        Ok(dirs)
    }

    pub fn load_doc<T: DeserializeOwned>(&self, dir: &Path, name: &str) -> Result<Option<T>> {
        validate_key(name)?;
        let path = dir.join(format!("{name}.json"));
        read_document(&self.read_path(&path))?
            .map(|document| self.parse_doc(&path, document))
            .transpose()
    }

    /// Every document of `dir` except the ones named in `skip`, in name order.
    pub fn docs<T: DeserializeOwned>(&self, dir: &Path, skip: &[&str]) -> Result<Vec<T>> {
        let mut docs = Vec::new();
        for path in self.doc_paths(dir, skip)? {
            if let Some(document) = read_document(&path)? {
                docs.push(self.parse_doc(&path, document)?);
            }
        }
        Ok(docs)
    }

    /// Like [`Store::docs`], but leaves out documents with fields this build does not know,
    /// because acting on them would lose those fields. Each is named in the second list.
    pub fn docs_intact<T: Serialize + DeserializeOwned>(
        &self,
        dir: &Path,
        skip: &[&str],
    ) -> Result<(Vec<T>, Vec<String>)> {
        let (mut docs, mut unknown) = (Vec::new(), Vec::new());
        for path in self.doc_paths(dir, skip)? {
            let Some(document) = read_document(&path)? else {
                continue;
            };
            let doc: T = self.parse_doc(&path, document.clone())?;
            match lost_field_of(&path, &document, &doc)? {
                Some(field) => unknown.push(format!(
                    "{} has field {field} that this agt does not know; kept",
                    path.display()
                )),
                None => docs.push(doc),
            }
        }
        Ok((docs, unknown))
    }

    /// Runs `change` on the document under its lock and writes the result atomically. A missing
    /// document starts from `init`.
    pub fn update_doc<T: Serialize + DeserializeOwned, R>(
        &self,
        dir: &Path,
        name: &str,
        init: impl FnOnce() -> T,
        change: impl FnOnce(&mut T) -> Result<R>,
    ) -> Result<R> {
        self.update_doc_with(dir, name, || Ok(init()), change)
    }

    pub fn update_existing_doc<T: Serialize + DeserializeOwned, R>(
        &self,
        dir: &Path,
        name: &str,
        change: impl FnOnce(&mut T) -> Result<R>,
    ) -> Result<R> {
        self.update_doc_with(
            dir,
            name,
            || fail(format!("{name} moved or was removed; retry the command")),
            change,
        )
    }

    fn update_doc_with<T: Serialize + DeserializeOwned, R>(
        &self,
        dir: &Path,
        name: &str,
        init: impl FnOnce() -> Result<T>,
        change: impl FnOnce(&mut T) -> Result<R>,
    ) -> Result<R> {
        validate_key(name)?;
        create_dir(dir)?;
        let _lock = lock(&dir.join(format!(".{name}.lock")))?;
        let path = dir.join(format!("{name}.json"));
        let mut doc = match read_document(&path)? {
            Some(document) => {
                let doc: T = self.parse_doc(&path, document.clone())?;
                // Writing a file with fields this build does not know would drop them.
                if let Some(field) = lost_field_of(&path, &document, &doc)? {
                    return fail(format!(
                        "{} has field {field} that this agt does not know; it was probably written by a newer agt, so upgrade agt before changing it",
                        path.display()
                    ));
                }
                doc
            }
            None => init()?,
        };
        let result = change(&mut doc)?;
        self.backup_before_migration(&path)?;
        persist_json(dir, &path, &doc, || Ok(()))?;
        Ok(result)
    }

    /// Deletes the document when `remove` approves it, re-reading it under its lock first. Gives
    /// up with [`Removal::Busy`] when another process holds the lock.
    pub fn try_remove_doc_if<T: Serialize + DeserializeOwned>(
        &self,
        dir: &Path,
        name: &str,
        remove: impl FnOnce(&T) -> bool,
    ) -> Result<Removal> {
        validate_key(name)?;
        let lock_path = dir.join(format!(".{name}.lock"));
        let Some(_lock) = try_lock(&lock_path)? else {
            return Ok(Removal::Busy);
        };
        let path = dir.join(format!("{name}.json"));
        let Some(document) = read_document(&path)? else {
            return Ok(Removal::Kept);
        };
        let doc: T = self.parse_doc(&path, document.clone())?;
        // A document with fields this build does not know is not this build's to delete.
        if lost_field_of(&path, &document, &doc)?.is_some() || !remove(&doc) {
            return Ok(Removal::Kept);
        }
        remove_with_lock(&path, &lock_path).map(Removal::Removed)
    }

    /// Moves the document `from` to `to`, replacing nothing: it fails when `to` exists.
    pub fn rename_doc(&self, dir: &Path, from: &str, to: &str) -> Result<()> {
        validate_key(from)?;
        validate_key(to)?;
        let from_lock = dir.join(format!(".{from}.lock"));
        let to_lock = dir.join(format!(".{to}.lock"));
        let (first, second) = if from < to {
            (&from_lock, &to_lock)
        } else {
            (&to_lock, &from_lock)
        };
        let _first = lock(first)?;
        let _second = lock(second)?;
        let target = dir.join(format!("{to}.json"));
        if target.exists() {
            return fail(format!("{} already exists", target.display()));
        }
        let source = dir.join(format!("{from}.json"));
        fs::rename(&source, &target).map_err(ctx(format!("move {}", source.display())))?;
        fs::remove_file(&from_lock).map_err(ctx(format!("remove {}", from_lock.display())))?;
        Ok(())
    }

    fn parse_doc<T: DeserializeOwned>(&self, path: &Path, document: Value) -> Result<T> {
        let version = document.get("version").and_then(Value::as_u64);
        let Some(version) = version.and_then(|version| u32::try_from(version).ok()) else {
            return fail(format!("{} has no valid schema version", path.display()));
        };
        if version > self.schema.version {
            return fail(format!(
                "{} uses schema version {version}, newer than the {} this agt (v{}) supports; upgrade agt to a release that supports schema version {version}",
                path.display(),
                self.schema.version,
                env!("CARGO_PKG_VERSION"),
            ));
        }
        serde_json::from_value(document).map_err(ctx(format!("parse {}", path.display())))
    }
}

impl Store {
    fn doc_paths(&self, dir: &Path, skip: &[&str]) -> Result<Vec<PathBuf>> {
        let entries = self.read_entries(dir)?;
        let mut paths = Vec::new();
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            if name.starts_with('.') || skip.contains(&stem) || !entry.path().is_file() {
                continue;
            }
            paths.push(entry.path());
        }
        paths.sort();
        Ok(paths)
    }
}

/// The first field of `document` that `doc` would not write back.
fn lost_field_of<T: Serialize>(
    path: &Path,
    document: &serde_json::Value,
    doc: &T,
) -> Result<Option<String>> {
    let written = serde_json::to_value(doc).map_err(ctx(format!("encode {}", path.display())))?;
    Ok(lost_field(document, &written, ""))
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::super::Namespace;
    use super::*;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Note {
        version: u32,
        text: String,
    }

    fn note(text: &str) -> Note {
        Note {
            version: 1,
            text: text.into(),
        }
    }

    #[test]
    fn documents_are_created_changed_listed_and_moved() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Issues);
        let dir = store.repo_dir("gh-github.com-o-r").unwrap();
        store
            .update_doc(&dir, "1", || note("a"), |_| Ok(()))
            .unwrap();
        store
            .update_doc(
                &dir,
                "2",
                || note("b"),
                |doc| {
                    doc.text.push('!');
                    Ok(())
                },
            )
            .unwrap();
        store
            .update_doc(&dir, "tracker", || note("t"), |_| Ok(()))
            .unwrap();
        let listed: Vec<Note> = store.docs(&dir, &["tracker"]).unwrap();
        assert_eq!(listed, [note("a"), note("b!")]);
        store.rename_doc(&dir, "2", "3").unwrap();
        assert_eq!(store.load_doc::<Note>(&dir, "2").unwrap(), None);
        assert_eq!(store.load_doc(&dir, "3").unwrap(), Some(note("b!")));
        assert!(store.rename_doc(&dir, "1", "3").is_err());
        assert_eq!(store.child_dirs().unwrap(), [dir]);
    }

    #[test]
    fn a_newer_document_is_refused_for_reads_and_writes() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Issues);
        let dir = store.repo_dir("t").unwrap();
        create_dir(&dir).unwrap();
        fs::write(dir.join("1.json"), r#"{"version":2,"text":"x"}"#).unwrap();
        let error = store.load_doc::<Note>(&dir, "1").err().unwrap().0;
        assert!(error.contains("upgrade agt"), "{error}");
        assert!(
            store
                .update_doc(&dir, "1", || note("a"), |_| Ok(()))
                .is_err()
        );
    }

    #[test]
    fn writing_refuses_to_drop_unknown_fields() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Issues);
        let dir = store.repo_dir("t").unwrap();
        create_dir(&dir).unwrap();
        fs::write(dir.join("1.json"), r#"{"version":1,"text":"x","extra":1}"#).unwrap();
        let error = store
            .update_doc(&dir, "1", || note("a"), |_| Ok(()))
            .err()
            .unwrap()
            .0;
        assert!(error.contains("field extra"), "{error}");
    }

    #[test]
    fn a_document_with_unknown_fields_is_not_removed_or_listed_as_intact() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Issues);
        let dir = store.repo_dir("t").unwrap();
        create_dir(&dir).unwrap();
        fs::write(dir.join("1.json"), r#"{"version":1,"text":"x","extra":1}"#).unwrap();
        fs::write(dir.join("2.json"), r#"{"version":1,"text":"y"}"#).unwrap();
        let (docs, unknown) = store.docs_intact::<Note>(&dir, &[]).unwrap();
        assert_eq!(docs, [note("y")]);
        assert_eq!(unknown.len(), 1);
        assert!(unknown[0].contains("field extra"), "{unknown:?}");
        let kept = store
            .try_remove_doc_if::<Note>(&dir, "1", |_| true)
            .unwrap();
        assert_eq!(kept, Removal::Kept);
        assert!(dir.join("1.json").exists());
        let gone = store
            .try_remove_doc_if::<Note>(&dir, "2", |_| true)
            .unwrap();
        assert!(matches!(gone, Removal::Removed(_)));
        assert!(!dir.join("2.json").exists());
    }
}
