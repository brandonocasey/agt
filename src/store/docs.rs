//! Typed JSON documents for the namespaces that are not review targets (issues). A document is
//! a file `<name>.json` under a directory of the namespace, carries a `version`, and is
//! changed under the same per-file lock and atomic replace as a target.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{
    Store, create_dir, ctx, fail, lock, lost_field, persist_json, read_document, validate_key,
};
use crate::core::error::Result;

#[allow(
    dead_code,
    reason = "used by `agt issue`, which lands in the next change"
)]
impl Store {
    /// The directories directly under the namespace root.
    pub fn child_dirs(&self) -> Result<Vec<PathBuf>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return fail(format!("read {}: {error}", self.root.display())),
        };
        let mut dirs = Vec::new();
        for entry in entries {
            let entry = entry.map_err(ctx(format!("read {}", self.root.display())))?;
            let kind = entry
                .file_type()
                .map_err(ctx(format!("stat {}", entry.path().display())))?;
            if kind.is_dir() {
                dirs.push(entry.path());
            }
        }
        dirs.sort();
        Ok(dirs)
    }

    pub fn load_doc<T: DeserializeOwned>(&self, dir: &Path, name: &str) -> Result<Option<T>> {
        validate_key(name)?;
        let path = dir.join(format!("{name}.json"));
        read_document(&path)?
            .map(|document| self.parse_doc(&path, document))
            .transpose()
    }

    /// Every document of `dir` except the ones named in `skip`, in name order.
    pub fn docs<T: DeserializeOwned>(&self, dir: &Path, skip: &[&str]) -> Result<Vec<T>> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return fail(format!("read {}: {error}", dir.display())),
        };
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(ctx(format!("read {}", dir.display())))?;
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
        let mut docs = Vec::new();
        for path in paths {
            if let Some(document) = read_document(&path)? {
                docs.push(self.parse_doc(&path, document)?);
            }
        }
        Ok(docs)
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
        validate_key(name)?;
        create_dir(dir)?;
        let _lock = lock(&dir.join(format!(".{name}.lock")))?;
        let path = dir.join(format!("{name}.json"));
        let mut doc = match read_document(&path)? {
            Some(document) => {
                let doc: T = self.parse_doc(&path, document.clone())?;
                // Writing a file with fields this build does not know would drop them.
                let written = serde_json::to_value(&doc)
                    .map_err(ctx(format!("encode {}", path.display())))?;
                if let Some(field) = lost_field(&document, &written, "") {
                    return fail(format!(
                        "{} has field {field} that this agt does not know; it was probably written by a newer agt, so upgrade agt before changing it",
                        path.display()
                    ));
                }
                doc
            }
            None => init(),
        };
        let result = change(&mut doc)?;
        self.backup_before_migration(&path)?;
        persist_json(dir, &path, &doc, || Ok(()))?;
        Ok(result)
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
}
