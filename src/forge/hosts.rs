//! The user's hosts file: `$XDG_CONFIG_HOME/agt/hosts.json` names the forge of hosts agt cannot
//! tell from `gh`, `glab` or `tea` alone: `{"git.example.com": {"service": "forgejo"}}`, where
//! `gitea` means the same.

use std::collections::HashMap;

use serde::Deserialize;

use crate::core::error::{Error, Result, fail};
use crate::core::local;
use crate::store::model::Forge;

#[derive(Deserialize)]
struct HostEntry {
    service: Forge,
}

/// The forge the hosts file names for `host`, if any.
pub fn named(host: &str) -> Result<Option<Forge>> {
    let Ok(path) = local::agt_config_file("hosts.json") else {
        return Ok(None);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return fail(format!("read {}: {error}", path.display())),
    };
    // Entries stay unparsed until one names this host, so an entry for another host that this
    // build does not understand never breaks it.
    let hosts: HashMap<String, serde_json::Value> = serde_json::from_str(&text)
        .map_err(|error| Error(format!("parse {}: {error}", path.display())))?;
    let Some((_, entry)) = hosts
        .into_iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(host))
    else {
        return Ok(None);
    };
    let entry: HostEntry = serde_json::from_value(entry)
        .map_err(|error| Error(format!("{} entry for {host}: {error}", path.display())))?;
    Ok(Some(entry.service))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_is_named_forgejo_or_gitea_and_stored_forges_keep_their_names() {
        let forge = |name: &str| {
            serde_json::from_str::<HostEntry>(&format!(r#"{{"service": "{name}"}}"#))
                .unwrap()
                .service
        };
        assert!(forge("forgejo") == Forge::Forgejo);
        assert!(forge("gitea") == Forge::Forgejo);
        assert!(forge("github") == Forge::Github);
        assert!(forge("gitlab") == Forge::Gitlab);
        let stored = |forge: Forge| serde_json::to_string(&forge).unwrap();
        assert_eq!(stored(Forge::Github), r#""github""#);
        assert_eq!(stored(Forge::Gitlab), r#""gitlab""#);
        assert_eq!(stored(Forge::Forgejo), r#""forgejo""#);
    }
}
