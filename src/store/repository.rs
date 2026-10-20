use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use crate::core::error::{Result, ctx, fail};
use crate::git::Repo;

use super::{Namespace, Store};
use crate::core::local::{lock_file, sync_dir, write_new};

impl Store {
    pub fn open_repo(namespace: Namespace, repo: &Repo, preview: bool) -> Result<Self> {
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
        Ok(Self {
            root: main.join(".agt").join(namespace.dir_name()),
            backups: global.backups.join("repositories").join(&identity),
            agents: global.agents.clone(),
            namespace,
            schema: namespace.schema(),
            local: true,
            backed_up: AtomicBool::new(false),
        })
    }

    pub(crate) fn inspect_repo(namespace: Namespace, repo: &Repo) -> Result<Self> {
        Self::open_repo(namespace, repo, true)
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
    write_new(&path, identity.as_bytes())?;
    sync_dir(common)?;
    Ok(identity)
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
