mod backup;
mod docs;
pub mod issues;
mod json;
pub mod model;
pub mod prune;
mod repository;
mod schema;
pub mod sidecars;

use std::fs::{self, DirBuilder, File};
use std::io::ErrorKind;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use self::json::read_document;
use self::schema::{
    REVIEW_MIGRATIONS, Schema, is_target_document, is_target_file, parse_target, read_target,
    read_target_for_write, read_target_intact, write_target,
};
use crate::core::error::{Result, ctx, fail};
use crate::core::local::{self, lock_file, try_lock_file};
use crate::store::model::{Target, VERSION};

const MAX_KEY: usize = 200;

/// Files left out of a read, each as its key and the warning that says why.
pub type LeftOut = Vec<(String, String)>;

/// A store area with its own directory and schema version, so a change to one cannot break
/// another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Namespace {
    Reviews,
    Issues,
    Tasks,
}

impl Namespace {
    pub const ALL: [Self; 3] = [Self::Reviews, Self::Issues, Self::Tasks];

    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Reviews => "reviews",
            Self::Issues => "issues",
            Self::Tasks => "tasks",
        }
    }

    fn schema(self) -> Schema {
        match self {
            Self::Reviews => Schema {
                version: VERSION,
                migrations: REVIEW_MIGRATIONS,
            },
            Self::Issues | Self::Tasks => Schema {
                version: 1,
                migrations: &[],
            },
        }
    }
}

pub struct Store {
    root: PathBuf,
    local: bool,
    fallback: Vec<(PathBuf, PathBuf)>,
    agents: PathBuf,
    backups: PathBuf,
    namespace: Namespace,
    schema: Schema,
    backed_up: AtomicBool,
}

impl Store {
    pub fn open(namespace: Namespace) -> Result<Self> {
        Ok(Self::at(&local::state_home()?, namespace))
    }

    /// The namespace directory, for domains whose documents are not review targets.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The shared backups directory, `agents/backups`.
    pub fn backups_dir(&self) -> &Path {
        &self.backups
    }

    /// Opens `namespace` under the state directory `base` (the parent of `agents`).
    fn at(base: &Path, namespace: Namespace) -> Self {
        let agents = base.join("agents");
        Self {
            root: agents.join(namespace.dir_name()),
            local: false,
            fallback: Vec::new(),
            agents: agents.clone(),
            backups: agents.join("backups"),
            namespace,
            schema: namespace.schema(),
            backed_up: AtomicBool::new(false),
        }
    }

    fn write(&self, path: &Path, target: &Target, expected: Option<u64>) -> Result<()> {
        self.backup_before_migration(path)?;
        write_target(path, target, expected, self.schema)
    }

    pub fn repo_dir(&self, repo_key: &str) -> Result<PathBuf> {
        if self.local && self.namespace == Namespace::Reviews {
            return Ok(self.root.clone());
        }
        let mut dir = self.root.clone();
        for part in repo_key.split('/') {
            let part: String = part
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            if part.is_empty() || part.chars().all(|c| c == '.') {
                return fail(format!("invalid repository key: {repo_key}"));
            }
            dir.push(part);
        }
        Ok(dir)
    }

    pub fn relative(&self, dir: &Path) -> String {
        dir.strip_prefix(&self.root)
            .unwrap_or(dir)
            .to_string_lossy()
            .into_owned()
    }

    pub fn load(&self, dir: &Path, key: &str) -> Result<Option<Target>> {
        validate_key(key)?;
        read_target(
            &self.read_path(&dir.join(format!("{key}.json"))),
            self.schema,
        )
    }

    pub fn targets(&self, dir: &Path) -> Result<Vec<Target>> {
        let entries = self.read_entries(dir)?;
        let mut targets = Vec::new();
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            let is_dir = entry
                .file_type()
                .map_err(ctx(format!("stat {}", entry.path().display())))?
                .is_dir();
            if is_dir {
                continue;
            }
            let path = entry.path();
            let Some(document) = read_document(&path)? else {
                continue;
            };
            if is_target_document(&document) {
                targets.push(parse_target(&path, document, self.schema)?.0);
            }
        }
        targets.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(targets)
    }

    /// The targets in `dir` that can be rewritten without losing anything, plus a warning for
    /// each file left out: one that cannot be read or parsed, or that holds fields this agt does
    /// not know. One bad file does not hide the others.
    pub fn targets_intact(&self, dir: &Path) -> Result<(Vec<Target>, Vec<String>)> {
        let (targets, left_out) = self.targets_intact_by_key(dir)?;
        Ok((
            targets,
            left_out.into_iter().map(|(_, warning)| warning).collect(),
        ))
    }

    /// Like [`Store::targets_intact`], with each file left out as its key and warning.
    pub fn targets_intact_by_key(&self, dir: &Path) -> Result<(Vec<Target>, LeftOut)> {
        let entries = self.read_entries(dir)?;
        let (mut targets, mut warnings) = (Vec::new(), Vec::new());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if name.starts_with('.') || !name.ends_with(".json") || is_dir {
                continue;
            }
            let path = entry.path();
            let key = name.trim_end_matches(".json").to_owned();
            match read_target_intact(&path, self.schema) {
                Ok(Some(Ok(target))) => targets.push(target),
                Ok(Some(Err(field))) => warnings.push((
                    key,
                    format!(
                        "{} has field {field} that this agt does not know; kept",
                        path.display()
                    ),
                )),
                Ok(None) => {}
                Err(error) => warnings.push((key, format!("{error}; kept"))),
            }
        }
        targets.sort_by(|a, b| a.key.cmp(&b.key));
        Ok((targets, warnings))
    }

    /// Every directory under the store root that holds at least one target file. A directory or file that cannot be read is skipped with a
    /// warning instead of ending the search.
    pub fn repo_dirs_lenient(&self) -> (Vec<PathBuf>, Vec<String>) {
        let mut warnings = Vec::new();
        let found = self.find_repo_dirs(Some(&mut warnings)).unwrap_or_default();
        (found, warnings)
    }

    /// With `warnings`, a problem is recorded and the search goes on; without, it ends the search.
    fn find_repo_dirs(&self, mut warnings: Option<&mut Vec<String>>) -> Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        let mut pending = vec![self.root.clone()];
        let note = |warnings: &mut Option<&mut Vec<String>>, message: String| match warnings
            .as_deref_mut()
        {
            Some(warnings) => {
                warnings.push(message);
                Ok(())
            }
            None => fail(message),
        };
        while let Some(dir) = pending.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    note(&mut warnings, format!("read {}: {error}", dir.display()))?;
                    continue;
                }
            };
            let mut has_target = false;
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        note(&mut warnings, format!("read {}: {error}", dir.display()))?;
                        continue;
                    }
                };
                let path = entry.path();
                let kind = match entry.file_type() {
                    Ok(kind) => kind,
                    Err(error) => {
                        note(&mut warnings, format!("stat {}: {error}", path.display()))?;
                        continue;
                    }
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                if kind.is_dir() {
                    pending.push(path);
                } else if kind.is_file() && !name.starts_with('.') && name.ends_with(".json") {
                    match is_target_file(&path) {
                        Ok(is_target) => has_target |= is_target,
                        // Lenient: the caller reads the directory and warns about this file once.
                        Err(_) if warnings.is_some() => has_target = true,
                        Err(error) => return Err(error),
                    }
                }
            }
            if has_target {
                found.push(dir);
            }
        }
        found.sort();
        Ok(found)
    }

    /// Uses a separate lock so publishing can still take the target's store-update lock.
    pub fn publish_lock(&self, dir: &Path, key: &str) -> Result<File> {
        validate_key(key)?;
        create_dir(dir)?;
        lock_file(&dir.join(format!(".{key}.publish.lock")))
    }

    /// Like [`Store::publish_lock`], but gives up with `None` when another process holds it.
    pub fn try_publish_lock(&self, dir: &Path, key: &str) -> Result<Option<File>> {
        validate_key(key)?;
        create_dir(dir)?;
        try_lock_file(&dir.join(format!(".{key}.publish.lock")))
    }

    /// Runs `change` on the target under its lock and writes the result atomically. A missing
    /// target starts from `init`.
    pub fn update<R>(
        &self,
        dir: &Path,
        key: &str,
        now: &str,
        init: impl FnOnce() -> Target,
        change: impl FnOnce(&mut Target) -> Result<R>,
    ) -> Result<R> {
        validate_key(key)?;
        create_dir(dir)?;
        let _lock = lock_file(&dir.join(format!(".{key}.lock")))?;
        let path = dir.join(format!("{key}.json"));
        let current = read_target_for_write(&path, self.schema)?;
        let expected = current.as_ref().map(|target| target.revision);
        let mut target = current.unwrap_or_else(init);
        let result = change(&mut target)?;
        target.revision = expected.unwrap_or(0) + 1;
        target.updated_at = now.to_owned();
        self.write(&path, &target, expected)?;
        Ok(result)
    }

    /// Like [`Store::update`] for the target `earlier` was read as, but fails when it is gone, so
    /// a change planned from an earlier read never recreates a target deleted since.
    pub fn update_existing<R>(
        &self,
        dir: &Path,
        earlier: &Target,
        now: &str,
        change: impl FnOnce(&mut Target) -> Result<R>,
    ) -> Result<R> {
        let key = earlier.key.as_str();
        let missing = std::cell::Cell::new(false);
        let init = || {
            missing.set(true);
            earlier.clone()
        };
        self.update(dir, key, now, init, |target| {
            if missing.get() {
                return fail(format!("target {key} vanished"));
            }
            change(target)
        })
    }

    /// Like [`Store::update`], but stores the result under `to` and deletes `from`. Both locks are
    /// held, taken in key order so two renames cannot deadlock. When both files exist, `from`'s
    /// runs, threads and records that `to` lacks are merged in.
    pub fn update_renamed<R>(
        &self,
        dir: &Path,
        from: &str,
        to: &str,
        now: &str,
        init: impl FnOnce() -> Target,
        change: impl FnOnce(&mut Target) -> Result<R>,
    ) -> Result<R> {
        if from == to {
            return self.update(dir, to, now, init, change);
        }
        validate_key(from)?;
        validate_key(to)?;
        create_dir(dir)?;
        let from_lock = dir.join(format!(".{from}.lock"));
        let to_lock = dir.join(format!(".{to}.lock"));
        let (first, second) = if from < to {
            (&from_lock, &to_lock)
        } else {
            (&to_lock, &from_lock)
        };
        let _first = lock_file(first)?;
        let _second = lock_file(second)?;
        let from_path = dir.join(format!("{from}.json"));
        let to_path = dir.join(format!("{to}.json"));
        let old = read_target_for_write(&from_path, self.schema)?;
        let current = read_target_for_write(&to_path, self.schema)?;
        let expected = current.as_ref().map(|target| target.revision);
        let had_old = old.is_some();
        let base_revision = expected.or(old.as_ref().map(|target| target.revision));
        let mut target = match (current, old) {
            (Some(mut current), Some(old)) => {
                current.absorb(old);
                current
            }
            (Some(current), None) => current,
            (None, Some(old)) => old,
            (None, None) => init(),
        };
        target.key = to.to_owned();
        let result = change(&mut target)?;
        target.revision = base_revision.unwrap_or(0) + 1;
        target.updated_at = now.to_owned();
        self.write(&to_path, &target, expected)?;
        if had_old {
            fs::remove_file(&from_path).map_err(ctx(format!("remove {}", from_path.display())))?;
            fs::remove_file(&from_lock).map_err(ctx(format!("remove {}", from_lock.display())))?;
        }
        Ok(result)
    }
}

fn validate_key(key: &str) -> Result<()> {
    let valid = !key.is_empty()
        && key.len() <= MAX_KEY
        && !key.starts_with('.')
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '%'));
    if valid {
        Ok(())
    } else {
        fail(format!("invalid target key: {key}"))
    }
}

fn create_dir(dir: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(ctx(format!("create {}", dir.display())))
}

#[cfg(test)]
mod tests {
    use super::schema::migrate_with;
    use super::*;
    use serde_json::{Value, json};

    fn comment(id: &str, body: &str, remote: Option<&str>) -> Value {
        json!({
            "id": id, "author": { "kind": "human", "login": "me" }, "body": body,
            "created_at": "2026-10-16T12:00:00Z",
            "post": if remote.is_some() { "posted" } else { "pending" },
            "remote": remote.map(|id| json!({
                "id": id, "url": null, "login": "alice", "is_me": false, "editor": null,
                "review_id": null,
            })),
            "draft_id": null, "attempt": null, "deleted_remote": false, "deleted_seen_at": null,
        })
    }

    fn thread(id: &str, remote: Option<&str>, comments: Vec<Value>) -> Value {
        json!({
            "id": id, "type": "comment", "severity": null, "anchor": null, "resolved": false,
            "resolution": null, "triage": null, "held": false, "finding": null,
            "comments": comments,
            "remote": remote.map(|id| json!({
                "kind": "general", "id": id, "url": null, "resolvable": false, "outdated": null,
                "path": null, "old_path": null, "line": null, "old_line": null,
                "original_line": null, "start_line": null, "original_start_line": null,
                "side": null, "commit": null, "review_state": null,
            })),
            "resolve_pending": null, "deleted_remote": false, "deleted_seen_at": null,
            "created_at": "2026-10-16T12:00:00Z", "updated_at": "2026-10-16T12:00:00Z",
        })
    }

    fn target(threads: Vec<Value>) -> Target {
        serde_json::from_value(json!({
            "version": VERSION, "revision": 1, "key": "k", "repo": "r", "kind": "branch",
            "branch": "feat", "branch_birth": null, "base": null, "head": null,
            "created_at": "2026-10-16T12:00:00Z", "updated_at": "2026-10-16T12:00:00Z",
            "review": null, "runs": [], "records": [], "threads": threads,
        }))
        .unwrap()
    }

    fn bodies(target: &Target) -> Vec<(String, Vec<String>)> {
        target
            .threads
            .iter()
            .map(|thread| {
                (
                    thread.id.clone(),
                    thread
                        .comments
                        .iter()
                        .map(|comment| comment.body.clone())
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn update_existing_refuses_a_deleted_target() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path(), Namespace::Reviews);
        let kept = target(Vec::new());
        let result = store.update_existing(dir.path(), &kept, "2026-10-16T12:00:00Z", |_| Ok(()));
        assert!(result.unwrap_err().to_string().contains("vanished"));
        assert!(!dir.path().join("k.json").exists());
        store
            .update(
                dir.path(),
                "k",
                "2026-10-16T12:00:00Z",
                || kept.clone(),
                |_| Ok(()),
            )
            .unwrap();
        store
            .update_existing(dir.path(), &kept, "2026-10-16T12:00:01Z", |target| {
                target.branch = Some("other".into());
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store
                .load(dir.path(), "k")
                .unwrap()
                .unwrap()
                .branch
                .as_deref(),
            Some("other")
        );
    }

    #[test]
    fn merge_renumbers_colliding_human_threads() {
        let mut into = target(vec![thread(
            "h1",
            None,
            vec![comment("h1/c1", "Into note", None)],
        )]);
        let from = target(vec![thread(
            "h1",
            None,
            vec![comment("h1/c1", "From note", None)],
        )]);
        into.absorb(from);
        assert_eq!(
            bodies(&into),
            [
                ("h1".to_owned(), vec!["Into note".to_owned()]),
                ("h2".to_owned(), vec!["From note".to_owned()]),
            ]
        );
        assert_eq!(into.threads[1].comments[0].id, "h2/c1");
    }

    #[test]
    fn merge_keeps_pending_replies_on_the_same_remote_thread() {
        let imported = || comment("remote-900", "Alice text", Some("900"));
        let mut into = target(vec![thread(
            "gh-c900",
            Some("900"),
            vec![imported(), comment("gh-c900/c1", "Into reply", None)],
        )]);
        let from = target(vec![thread(
            "gh-c900",
            Some("900"),
            vec![imported(), comment("gh-c900/c1", "From reply", None)],
        )]);
        into.absorb(from);
        assert_eq!(
            bodies(&into),
            [(
                "gh-c900".to_owned(),
                vec![
                    "Alice text".to_owned(),
                    "Into reply".to_owned(),
                    "From reply".to_owned()
                ]
            )]
        );
        assert_eq!(into.threads[0].comments[2].id, "gh-c900/c2");
    }

    fn stored(version: u64) -> Value {
        json!({
            "version": version, "revision": 1, "key": "k", "repo": "r", "kind": "branch",
            "branch": null, "branch_birth": null, "base": null, "head": null,
            "created_at": "t", "updated_at": "t", "review": null,
            "runs": [], "records": [], "threads": [],
        })
    }

    fn file(name: &str, document: &Value) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        fs::write(&path, document.to_string()).unwrap();
        (dir, path)
    }

    #[test]
    fn current_version_loads() {
        let (_dir, path) = file("k.json", &stored(VERSION.into()));
        assert!(
            read_target(&path, Namespace::Reviews.schema())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn newer_version_is_refused_with_an_upgrade_hint() {
        let (_dir, path) = file("k.json", &stored(u64::from(VERSION) + 1));
        let error = read_target(&path, Namespace::Reviews.schema())
            .err()
            .unwrap()
            .0;
        assert!(error.contains("newer"), "{error}");
        assert!(error.contains("upgrade agt"), "{error}");
        assert!(
            error.contains(&format!("version {}", VERSION + 1)),
            "{error}"
        );
    }

    #[test]
    fn version_zero_is_refused() {
        let (_dir, path) = file("k.json", &stored(0));
        assert!(read_target(&path, Namespace::Reviews.schema()).is_err());
    }

    #[test]
    fn migrations_run_from_the_stored_version() {
        fn rename(document: &mut Value) {
            document["renamed"] = json!(true);
        }
        fn twice(document: &mut Value) {
            document["twice"] = json!(true);
        }
        let steps: &[fn(&mut Value)] = &[rename, twice];
        let mut document = json!({"version": 1});
        migrate_with(&mut document, 1, steps);
        assert_eq!(
            document,
            json!({"version": 3, "renamed": true, "twice": true})
        );
        let mut document = json!({"version": 2});
        migrate_with(&mut document, 2, steps);
        assert_eq!(document, json!({"version": 3, "twice": true}));
    }

    #[test]
    fn writing_refuses_to_drop_unknown_fields() {
        let mut document = stored(VERSION.into());
        document["threads"] = json!([]);
        document["from_the_future"] = json!(1);
        let (_dir, path) = file("k.json", &document);
        assert!(
            read_target(&path, Namespace::Reviews.schema())
                .unwrap()
                .is_some()
        );
        let error = read_target_for_write(&path, Namespace::Reviews.schema())
            .err()
            .unwrap()
            .0;
        assert!(error.contains("from_the_future"), "{error}");
        let mut document = stored(VERSION.into());
        document["runs"] = json!([]);
        let (_dir, path) = file("k.json", &document);
        assert!(
            read_target_for_write(&path, Namespace::Reviews.schema())
                .unwrap()
                .is_some()
        );
    }

    const NOW: &str = "2026-10-16T12:00:00Z";

    fn write_target_named(store: &Store, dir: &Path) {
        store
            .update(dir, "k", NOW, || target(Vec::new()), |_| Ok(()))
            .unwrap();
    }

    #[test]
    fn namespaces_use_separate_directories_and_do_not_see_each_other() {
        let base = tempfile::tempdir().unwrap();
        let reviews = Store::at(base.path(), Namespace::Reviews);
        let issues = Store::at(base.path(), Namespace::Issues);
        let tasks = Store::at(base.path(), Namespace::Tasks);
        let dir = reviews.repo_dir("github.com/o/r").unwrap();
        write_target_named(&reviews, &dir);
        assert!(dir.starts_with(base.path().join("agents").join("reviews")));
        assert_eq!(reviews.repo_dirs_lenient().0, vec![dir.clone()]);
        assert!(issues.repo_dirs_lenient().0.is_empty());
        assert!(tasks.repo_dirs_lenient().0.is_empty());
        let issue_dir = issues.repo_dir("github.com/o/r").unwrap();
        assert!(issue_dir.starts_with(base.path().join("agents").join("issues")));
        assert!(issues.load(&issue_dir, "k").unwrap().is_none());
        assert!(
            tasks
                .repo_dir("x")
                .unwrap()
                .starts_with(base.path().join("agents").join("tasks"))
        );
    }

    #[test]
    fn each_namespace_refuses_a_newer_schema_version() {
        for namespace in [Namespace::Reviews, Namespace::Issues, Namespace::Tasks] {
            let base = tempfile::tempdir().unwrap();
            let store = Store::at(base.path(), namespace);
            let dir = store.repo_dir("o/r").unwrap();
            let newer = stored(u64::from(namespace.schema().version) + 1);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("k.json"), newer.to_string()).unwrap();
            let error = store.load(&dir, "k").err().unwrap().0;
            assert!(error.contains("upgrade agt"), "{namespace:?}: {error}");
        }
    }

    #[test]
    fn schema_versions_are_independent_per_namespace() {
        let base = tempfile::tempdir().unwrap();
        let mut reviews = Store::at(base.path(), Namespace::Reviews);
        let issues = Store::at(base.path(), Namespace::Issues);
        reviews.schema.version = 2;
        let dir = issues.repo_dir("o/r").unwrap();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("k.json"), stored(2).to_string()).unwrap();
        assert!(issues.load(&dir, "k").is_err());
        let dir = reviews.repo_dir("o/r").unwrap();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("k.json"), stored(2).to_string()).unwrap();
        reviews.schema.migrations = &[|_| {}];
        assert!(reviews.load(&dir, "k").unwrap().is_some());
    }

    #[test]
    fn migrating_write_backs_up_the_namespace_first_and_only_once() {
        let base = tempfile::tempdir().unwrap();
        let mut store = Store::at(base.path(), Namespace::Reviews);
        let dir = store.repo_dir("o/r").unwrap();
        write_target_named(&store, &dir);
        let backups = base.path().join("agents").join("backups");
        assert!(!backups.exists(), "a current-version write needs no backup");
        let before = fs::read_to_string(dir.join("k.json")).unwrap();

        store.schema = Schema {
            version: 2,
            migrations: &[|_| {}],
        };
        write_target_named(&store, &dir);
        let made: Vec<_> = fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(made.len(), 1);
        let name = made[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("reviews-"), "{name}");
        let copy = made[0].join("o").join("r").join("k.json");
        assert_eq!(fs::read_to_string(copy).unwrap(), before);
        let after = fs::read_to_string(dir.join("k.json")).unwrap();
        assert_ne!(
            after, before,
            "the live file was rewritten after the backup"
        );

        write_target_named(&store, &dir);
        assert_eq!(fs::read_dir(&backups).unwrap().count(), 1);
    }

    #[test]
    fn a_later_process_finds_the_backup_of_that_version_and_skips_it() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = store.repo_dir("o/r").unwrap();
        write_target_named(&store, &dir);
        fs::write(
            dir.join("old.json"),
            fs::read_to_string(dir.join("k.json")).unwrap(),
        )
        .unwrap();
        let backups = base.path().join("agents").join("backups");
        let v2 = Schema {
            version: 2,
            migrations: &[|_| {}],
        };
        // Two processes, each with its own store, migrate different files from version 1.
        for key in ["k", "old"] {
            let mut next = Store::at(base.path(), Namespace::Reviews);
            next.schema = v2;
            next.update(&dir, key, NOW, || target(Vec::new()), |_| Ok(()))
                .unwrap();
        }
        let made: Vec<String> = fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(made.len(), 1, "{made:?}");
        assert!(made[0].starts_with("reviews-v1-"), "{made:?}");
    }

    #[test]
    fn a_partial_backup_is_not_taken_for_a_finished_one() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = store.repo_dir("o/r").unwrap();
        write_target_named(&store, &dir);
        let backups = base.path().join("agents").join("backups");
        // What a copy that failed part way leaves behind.
        fs::create_dir_all(backups.join(".reviews-v1-2026-01-01T00-00-00Z.partial")).unwrap();
        let mut next = Store::at(base.path(), Namespace::Reviews);
        next.schema = Schema {
            version: 2,
            migrations: &[|_| {}],
        };
        write_target_named(&next, &dir);
        let finished: Vec<String> = fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("reviews-v1-"))
            .collect();
        assert_eq!(finished.len(), 1, "{finished:?}");
        assert!(
            !backups
                .join(".reviews-v1-2026-01-01T00-00-00Z.partial")
                .exists(),
            "the stale partial copy is removed"
        );
    }

    #[test]
    fn a_migration_waiting_on_the_backup_lock_rechecks_for_the_backup_made_meanwhile() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = store.repo_dir("o/r").unwrap();
        write_target_named(&store, &dir);
        let agents = base.path().join("agents");
        let backups = agents.join("backups");
        fs::create_dir_all(&backups).unwrap();
        let guard = lock_file(&agents.join(".reviews-backup.lock")).unwrap();
        std::thread::scope(|scope| {
            let migrating = scope.spawn(|| {
                let mut next = Store::at(base.path(), Namespace::Reviews);
                next.schema = Schema {
                    version: 2,
                    migrations: &[|_| {}],
                };
                write_target_named(&next, &dir);
            });
            // The other process finishes its backup while this one waits on the lock.
            std::thread::sleep(std::time::Duration::from_millis(200));
            fs::create_dir(backups.join("reviews-v1-x")).unwrap();
            drop(guard);
            migrating.join().unwrap();
        });
        let names: Vec<String> = fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["reviews-v1-x".to_string()], "{names:?}");
    }

    #[test]
    fn concurrent_migrations_leave_one_whole_backup() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let keys: Vec<String> = (0..6).map(|i| format!("k{i}")).collect();
        let dir = store.repo_dir("o/r").unwrap();
        for key in &keys {
            store
                .update(&dir, key, NOW, || target(Vec::new()), |_| Ok(()))
                .unwrap();
        }
        let barrier = std::sync::Barrier::new(keys.len());
        std::thread::scope(|scope| {
            for key in &keys {
                let (barrier, dir, root) = (&barrier, &dir, base.path());
                scope.spawn(move || {
                    let mut next = Store::at(root, Namespace::Reviews);
                    next.schema = Schema {
                        version: 2,
                        migrations: &[|_| {}],
                    };
                    barrier.wait();
                    next.update(dir, key, NOW, || target(Vec::new()), |_| Ok(()))
                        .unwrap();
                });
            }
        });
        let names: Vec<String> = fs::read_dir(base.path().join("agents").join("backups"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].starts_with("reviews-v1-"), "{names:?}");
        assert!(!names[0].ends_with(".partial"), "{names:?}");
    }
}
