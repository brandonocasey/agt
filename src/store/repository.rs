use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use crate::core::error::{Result, ctx, fail};
use crate::git::Repo;

use super::{Namespace, Store, copy_tree, create_dir};
use crate::core::local::{lock_file, sync_dir, write_atomic};

impl Store {
    pub fn open_repo(namespace: Namespace, repo: &Repo, preview: bool) -> Result<Self> {
        Self::open_repo_mode(namespace, repo, preview, true)
    }

    pub(crate) fn inspect_repo(namespace: Namespace, repo: &Repo) -> Result<Self> {
        Self::open_repo_mode(namespace, repo, true, false)
    }

    fn open_repo_mode(
        namespace: Namespace,
        repo: &Repo,
        preview: bool,
        legacy: bool,
    ) -> Result<Self> {
        if namespace == Namespace::Tasks {
            return Self::open(namespace);
        }
        let main = repo.main_checkout_root()?;
        let common =
            fs::canonicalize(repo.common_dir()?).map_err(ctx("resolve repository identity"))?;
        let identity = repository_identity(&common, preview)?;
        let global = Self::open(namespace)?;
        for path in [
            main.join(".agt"),
            main.join(".agt").join(namespace.dir_name()),
        ] {
            require_directory(&path)?;
        }
        let mut store = Self {
            root: main.join(".agt").join(namespace.dir_name()),
            backups: global.backups.join("repositories").join(&identity),
            agents: global.agents.clone(),
            namespace,
            schema: namespace.schema(),
            local: true,
            fallback: Vec::new(),
            backed_up: AtomicBool::new(false),
        };
        if preview && legacy {
            store.preview_legacy(&global, repo, &identity)?;
        } else if !preview {
            store.import_legacy(&global, repo, &identity)?;
        }
        Ok(store)
    }

    fn preview_legacy(&mut self, global: &Self, repo: &Repo, identity: &str) -> Result<()> {
        let marker = global
            .agents
            .join("repository-imports")
            .join(identity)
            .join(format!("{}.done", self.namespace.dir_name()));
        if !marker.exists() {
            let records = marker.parent().expect("import marker has parent");
            let stage = records.join(format!(".{}.stage", self.namespace.dir_name()));
            let record = records.join(format!("{}.backup", self.namespace.dir_name()));
            let snapshot = if stage.is_dir() {
                Some(stage)
            } else if record.exists() {
                let name = fs::read_to_string(&record).map_err(ctx("read import backup record"))?;
                if Path::new(&name).components().count() != 1
                    || !name.starts_with(&format!("{}-v0-", self.namespace.dir_name()))
                {
                    return fail("invalid legacy backup record");
                }
                let backup = self.backups.join(name);
                backup.is_dir().then_some(backup)
            } else {
                None
            };
            let sources = if let Some(snapshot) = snapshot {
                match self.namespace {
                    Namespace::Reviews => vec![(snapshot, PathBuf::new())],
                    Namespace::Issues => {
                        let mut sources = Vec::new();
                        for entry in
                            fs::read_dir(snapshot).map_err(ctx("read saved import snapshot"))?
                        {
                            let entry = entry.map_err(ctx("read saved tracker snapshot"))?;
                            sources.push((entry.path(), PathBuf::from(entry.file_name())));
                        }
                        sources
                    }
                    Namespace::Tasks => unreachable!(),
                }
            } else {
                self.legacy_sources(global, repo)?
            };
            for (source, relative) in sources {
                let _locks = inspect_source(&source)?;
                let destination = self.root.join(relative);
                check_conflicts(&source, &destination)?;
                self.fallback.push((destination, source));
            }
        }
        Ok(())
    }

    fn import_legacy(&self, global: &Self, repo: &Repo, identity: &str) -> Result<()> {
        let records = global.agents.join("repository-imports").join(identity);
        create_durable_dir(&records)?;
        let name = self.namespace.dir_name();
        let _guard = lock_file(&records.join(format!(".{name}.lock")))?;
        let marker = records.join(format!("{name}.done"));
        if marker.exists() {
            return Ok(());
        }
        let sources = self.legacy_sources(global, repo)?;
        let mut source_locks = Vec::new();
        for (source, _) in &sources {
            source_locks.extend(inspect_source(source)?);
        }
        let stage = records.join(format!(".{name}.stage"));
        let backup_record = records.join(format!("{name}.backup"));
        if !sources.is_empty() || backup_record.exists() {
            create_durable_dir(&self.backups)?;
            let _backup_guard =
                lock_file(&self.backups.with_file_name(format!(".{name}-backup.lock")))?;
            let backup_name = if backup_record.exists() {
                fs::read_to_string(&backup_record).map_err(ctx("read import backup record"))?
            } else {
                let stamp = crate::core::clock::now()?.replace(':', "-");
                let backup_name = format!("{name}-v0-{stamp}");
                write_atomic(&backup_record, backup_name.as_bytes(), true, || Ok(()))?;
                backup_name
            };
            if Path::new(&backup_name).components().count() != 1
                || !backup_name.starts_with(&format!("{name}-v0-"))
            {
                return fail("invalid legacy backup record");
            }
            let backup = self.backups.join(&backup_name);
            if !backup.exists() {
                let partial = self.backups.join(format!(".{backup_name}.partial"));
                if partial.exists() {
                    fs::remove_dir_all(&partial).map_err(ctx("remove unfinished legacy backup"))?;
                }
                for (source, relative) in &sources {
                    snapshot_source(source, &partial.join(relative))?;
                }
                for (source, relative) in &sources {
                    validate_snapshot(source, &partial.join(relative))?;
                }
                fs::rename(&partial, &backup).map_err(ctx("finish legacy backup"))?;
                sync_dir(&self.backups)?;
            }
            if !stage.exists() {
                let partial = records.join(format!(".{name}.stage.partial"));
                if partial.exists() {
                    fs::remove_dir_all(&partial).map_err(ctx("remove unfinished import stage"))?;
                }
                copy_tree(&backup, &partial)?;
                fs::rename(&partial, &stage).map_err(ctx("finish import stage"))?;
                sync_dir(&records)?;
            }
            check_conflicts(&stage, &self.root)?;
            create_durable_dir(self.root.parent().expect("namespace has parent"))?;
            // The staged copy MUST remain until the durable marker exists, so an interrupted
            // merge can resume after checking every destination against the same snapshot.
            merge(&stage, &self.root)?;
        }
        write_atomic(&marker, b"imported\n", true, || Ok(()))?;
        if stage.exists() {
            fs::remove_dir_all(&stage).map_err(ctx("remove completed import stage"))?;
        }
        Ok(())
    }

    fn legacy_sources(&self, global: &Self, repo: &Repo) -> Result<Vec<(PathBuf, PathBuf)>> {
        let mut sources = Vec::new();
        match self.namespace {
            Namespace::Reviews => {
                let source = global.repo_dir(&repo.repo_key()?)?;
                let mut directory = source.as_path();
                while directory.starts_with(&global.root) {
                    require_directory(directory)?;
                    let Some(parent) = directory.parent() else {
                        break;
                    };
                    directory = parent;
                }
                if source.is_dir() {
                    sources.push((source, PathBuf::new()));
                }
            }
            Namespace::Issues => {
                require_directory(&global.root)?;
                if let Ok(entries) = fs::read_dir(&global.root) {
                    for entry in entries {
                        let entry = entry.map_err(ctx("read legacy tracker directory"))?;
                        if entry
                            .file_type()
                            .map_err(ctx("stat legacy tracker"))?
                            .is_symlink()
                        {
                            return fail(format!(
                                "legacy source {} must not be a symbolic link",
                                entry.path().display()
                            ));
                        }
                    }
                }
                if let Some((host, remote)) = repo
                    .origin_url()?
                    .as_deref()
                    .and_then(crate::git::split_remote)
                {
                    for dir in global.child_dirs()? {
                        let path = dir.join("tracker.json");
                        if fs::symlink_metadata(&path)
                            .is_ok_and(|meta| meta.file_type().is_symlink())
                        {
                            return fail(format!(
                                "legacy source {} must not be a symbolic link",
                                path.display()
                            ));
                        }
                        let Some(document) = super::read_document(&path)? else {
                            continue;
                        };
                        if document.get("host").and_then(serde_json::Value::as_str)
                            == Some(host.as_str())
                            && document.get("repo").and_then(serde_json::Value::as_str)
                                == Some(remote.as_str())
                        {
                            let relative = dir
                                .strip_prefix(&global.root)
                                .map_err(ctx("legacy tracker outside store"))?
                                .to_path_buf();
                            sources.push((dir, relative));
                        }
                    }
                }
            }
            Namespace::Tasks => unreachable!(),
        }
        Ok(sources)
    }

    pub(crate) fn at_root(namespace: Namespace, root: PathBuf) -> Result<Self> {
        let mut store = Self::open(namespace)?;
        store.local = root
            .file_name()
            .is_some_and(|name| name == namespace.dir_name())
            && root
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == ".agt");
        store.root = root;
        Ok(store)
    }
}

fn read_repository_identity(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() => {
            return fail(format!("{} must be a regular file", path.display()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).map_err(ctx("inspect repository identity")),
    }
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(ctx("open repository identity"))?;
    if !file
        .metadata()
        .map_err(ctx("stat repository identity"))?
        .is_file()
    {
        return fail(format!("{} must be a regular file", path.display()));
    }
    let mut identity = String::new();
    file.take(41)
        .read_to_string(&mut identity)
        .map_err(ctx("read repository identity"))?;
    if identity.len() != 40
        || !identity
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return fail(format!("invalid repository identity in {}", path.display()));
    }
    Ok(Some(identity))
}

fn repository_identity(common: &Path, preview: bool) -> Result<String> {
    let path = common.join("agt-repository-id");
    if let Some(identity) = read_repository_identity(&path)? {
        if !preview {
            sync_dir(common)?;
        }
        return Ok(identity);
    }
    if preview {
        // An uninitialized clone MUST inspect legacy data without creating an identity or lock.
        return Ok("uninitialized".to_owned());
    }
    let _guard = lock_file(&common.join(".agt-repository-id.lock"))?;
    if let Some(identity) = read_repository_identity(&path)? {
        sync_dir(common)?;
        return Ok(identity);
    }
    let identity = sha1_smol::Sha1::from(crate::core::random::token("repo-", 32))
        .digest()
        .to_string();
    use std::io::Write;
    let mut temp =
        tempfile::NamedTempFile::new_in(common).map_err(ctx("stage repository identity"))?;
    temp.write_all(identity.as_bytes())
        .map_err(ctx("write repository identity"))?;
    temp.as_file()
        .sync_all()
        .map_err(ctx("sync repository identity"))?;
    temp.persist_noclobber(&path)
        .map_err(ctx("persist repository identity"))?;
    sync_dir(common)?;
    Ok(identity)
}

fn check_conflicts(source: &Path, destination: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(destination)
        && !meta.is_dir()
    {
        return fail(format!(
            "legacy import conflict at {}",
            destination.display()
        ));
    }
    for entry in fs::read_dir(source).map_err(ctx("read import stage"))? {
        let entry = entry.map_err(ctx("read import stage entry"))?;
        if entry.file_name().to_string_lossy().ends_with(".lock") {
            continue;
        }
        let dest = destination.join(entry.file_name());
        if entry
            .file_type()
            .map_err(ctx("stat import stage"))?
            .is_dir()
        {
            check_conflicts(&entry.path(), &dest)?;
        } else if let Ok(meta) = fs::symlink_metadata(&dest)
            && (!meta.is_file()
                || fs::read(&dest).map_err(ctx("read import destination"))?
                    != fs::read(entry.path()).map_err(ctx("read import source"))?)
        {
            return fail(format!("legacy import conflict at {}", dest.display()));
        }
    }
    Ok(())
}

fn merge(source: &Path, destination: &Path) -> Result<()> {
    create_durable_dir(destination)?;
    for entry in fs::read_dir(source).map_err(ctx("read staged import"))? {
        let entry = entry.map_err(ctx("read staged entry"))?;
        let dest = destination.join(entry.file_name());
        if entry
            .file_type()
            .map_err(ctx("stat staged entry"))?
            .is_dir()
        {
            merge(&entry.path(), &dest)?;
        } else if !dest.exists() {
            let bytes = fs::read(entry.path()).map_err(ctx("read staged file"))?;
            write_atomic(&dest, &bytes, true, || Ok(()))?;
        }
    }
    sync_dir(destination)
}

fn require_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_dir() => fail(format!("{} must be a plain directory", path.display())),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(crate::core::error::Error(format!(
            "stat {}: {error}",
            path.display()
        ))),
    }
}

fn inspect_source(path: &Path) -> Result<Vec<fs::File>> {
    let meta = fs::symlink_metadata(path).map_err(ctx("inspect legacy source"))?;
    if meta.file_type().is_symlink() {
        return fail(format!(
            "legacy source {} must not be a symbolic link",
            path.display()
        ));
    }
    let mut held = Vec::new();
    if meta.is_dir() {
        for entry in fs::read_dir(path).map_err(ctx("read legacy source"))? {
            held.extend(inspect_source(
                &entry.map_err(ctx("read legacy entry"))?.path(),
            )?);
        }
    } else if meta.is_file()
        && path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".lock"))
    {
        let file = fs::File::options()
            .read(true)
            .write(true)
            .open(path)
            .map_err(ctx("open legacy lock"))?;
        file.try_lock()
            .map_err(ctx(format!("legacy source is busy: {}", path.display())))?;
        if matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some(".sync.publish.lock" | ".push.publish.lock")
        ) {
            held.push(file);
        }
    } else if !meta.is_file() {
        return fail(format!(
            "legacy source {} is not a regular file or directory",
            path.display()
        ));
    }
    Ok(held)
}

fn document_locks(path: &Path) -> Result<Vec<fs::File>> {
    let mut held = Vec::new();
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return Ok(held);
    };
    for suffix in ["lock", "publish.lock"] {
        if suffix == "publish.lock" && matches!(stem, "sync" | "push") {
            continue;
        }
        let lock = path.with_file_name(format!(".{stem}.{suffix}"));
        match fs::symlink_metadata(&lock) {
            Ok(meta) if !meta.is_file() => return fail("legacy lock must be a regular file"),
            Ok(_) => {
                let file = fs::File::options()
                    .read(true)
                    .write(true)
                    .open(&lock)
                    .map_err(ctx("open legacy lock"))?;
                file.try_lock()
                    .map_err(ctx(format!("legacy source is busy: {}", lock.display())))?;
                held.push(file);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(crate::core::error::Error(format!(
                    "inspect legacy lock: {error}"
                )));
            }
        }
    }
    Ok(held)
}

// Documents MUST be copied under their own locks; tracker-wide locks remain held by the caller.
// The completed snapshot MUST be validated before it becomes the immutable import backup.
fn snapshot_source(source: &Path, destination: &Path) -> Result<()> {
    create_durable_dir(destination)?;
    for entry in fs::read_dir(source).map_err(ctx("read legacy snapshot source"))? {
        let entry = entry.map_err(ctx("read legacy snapshot entry"))?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).map_err(ctx("inspect legacy snapshot entry"))?;
        let dest = destination.join(entry.file_name());
        if meta.is_dir() {
            snapshot_source(&path, &dest)?;
        } else if !meta.is_file() {
            return fail(format!(
                "legacy source {} must be a regular file or directory, not a symbolic link",
                path.display()
            ));
        } else if !entry.file_name().to_string_lossy().ends_with(".lock") {
            let _locks = document_locks(&path)?;
            let bytes = fs::read(&path).map_err(ctx("read legacy snapshot document"))?;
            write_atomic(&dest, &bytes, true, || Ok(()))?;
        }
    }
    sync_dir(destination)
}

fn validate_snapshot(source: &Path, snapshot: &Path) -> Result<()> {
    let entries = |dir: &Path| -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        for entry in fs::read_dir(dir).map_err(ctx("read legacy snapshot validation"))? {
            let entry = entry.map_err(ctx("read legacy validation entry"))?;
            if !entry.file_name().to_string_lossy().ends_with(".lock") {
                paths.push(PathBuf::from(entry.file_name()));
            }
        }
        paths.sort();
        Ok(paths)
    };
    let names = entries(source)?;
    if names != entries(snapshot)? {
        return fail("legacy source changed during snapshot; retry");
    }
    for name in names {
        let path = source.join(&name);
        let saved = snapshot.join(name);
        let meta = fs::symlink_metadata(&path).map_err(ctx("inspect legacy validation source"))?;
        if meta.is_dir() && saved.is_dir() {
            validate_snapshot(&path, &saved)?;
        } else if meta.is_file() && saved.is_file() {
            let _locks = document_locks(&path)?;
            if fs::read(&path).map_err(ctx("read legacy validation source"))?
                != fs::read(&saved).map_err(ctx("read legacy validation snapshot"))?
            {
                return fail("legacy source changed during snapshot; retry");
            }
        } else {
            return fail(
                "legacy source changed during snapshot or contains a symbolic link; retry",
            );
        }
    }
    Ok(())
}

impl Store {
    pub(super) fn read_path(&self, path: &Path) -> PathBuf {
        if path.exists() {
            return path.to_path_buf();
        }
        for (destination, source) in &self.fallback {
            if let Ok(relative) = path.strip_prefix(destination) {
                return source.join(relative);
            }
        }
        path.to_path_buf()
    }

    pub(super) fn read_entries(&self, dir: &Path) -> Result<Vec<fs::DirEntry>> {
        let mut entries = std::collections::BTreeMap::new();
        let mut paths = vec![(dir.to_path_buf(), None)];
        for (destination, source) in &self.fallback {
            if let Ok(relative) = dir.strip_prefix(destination) {
                paths.push((source.join(relative), None));
            } else if destination.parent() == Some(dir) {
                paths.push((
                    source
                        .parent()
                        .expect("legacy tracker has parent")
                        .to_path_buf(),
                    source.file_name(),
                ));
            }
        }
        for (path, only) in paths {
            let found = match fs::read_dir(&path) {
                Ok(found) => found,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return fail(format!("read {}: {error}", path.display())),
            };
            for entry in found {
                let entry = entry.map_err(ctx("read store entry"))?;
                if only.is_none_or(|only| entry.file_name() == only) {
                    entries.entry(entry.file_name()).or_insert(entry);
                }
            }
        }
        Ok(entries.into_values().collect())
    }
}

fn create_durable_dir(path: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let mut current = path;
    while !current.exists() {
        missing.push(current.to_path_buf());
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    create_dir(path)?;
    for created in missing {
        sync_dir(&created)?;
        if let Some(parent) = created.parent() {
            sync_dir(parent)?;
        }
    }
    Ok(())
}
