use std::collections::{BTreeSet, HashMap, HashSet};

use crate::core::error::{Result, fail};
use crate::store::Store;
use crate::store::issues::{Fields, Issue, TRACKER_FILE};

use super::trackers::Tracker;

pub(super) struct Index {
    issues: HashMap<String, Issue>,
    aliases: HashMap<String, String>,
    children: HashMap<String, BTreeSet<String>>,
}

impl Index {
    pub fn load(store: &Store, tracker: &Tracker) -> Result<Self> {
        Self::from_issues(store.docs::<Issue>(&tracker.dir, &[TRACKER_FILE])?)
    }

    pub fn from_issues(issues: Vec<Issue>) -> Result<Self> {
        let mut index = Self {
            issues: HashMap::new(),
            aliases: HashMap::new(),
            children: HashMap::new(),
        };
        for issue in issues {
            for alias in [
                Some(issue.doc_name()),
                issue.local_key.clone(),
                Some(issue.key.to_ascii_lowercase()),
            ]
            .into_iter()
            .flatten()
            {
                if let Some(previous) = index.aliases.insert(alias.clone(), issue.key.clone())
                    && previous != issue.key
                {
                    return fail(format!("duplicate issue identity {alias}"));
                }
            }
            index.issues.insert(issue.key.clone(), issue);
        }
        for issue in index.issues.values() {
            if let Some(parent) = &issue.fields.parent {
                index
                    .children
                    .entry(index.canonical(parent).to_owned())
                    .or_default()
                    .insert(issue.key.clone());
            }
            for child in &issue.sub_issues {
                if index.get(child).is_none() {
                    index
                        .children
                        .entry(issue.key.clone())
                        .or_default()
                        .insert(child.clone());
                }
            }
        }
        Ok(index)
    }

    pub fn canonical<'a>(&'a self, key: &'a str) -> &'a str {
        self.aliases.get(key).map_or(key, String::as_str)
    }

    pub fn get(&self, key: &str) -> Option<&Issue> {
        self.issues.get(self.canonical(key))
    }

    pub fn issues(&self) -> impl Iterator<Item = &Issue> {
        self.issues.values()
    }

    pub fn remember(&mut self, issue: Issue) {
        if let Some(key) = &issue.local_key {
            self.aliases.insert(key.clone(), issue.key.clone());
        }
        self.aliases.insert(issue.doc_name(), issue.key.clone());
        self.aliases
            .insert(issue.key.to_ascii_lowercase(), issue.key.clone());
        self.issues.insert(issue.key.clone(), issue);
    }

    pub fn children(&self, key: &str) -> Vec<String> {
        self.children
            .get(self.canonical(key))
            .map(|children| children.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn has_children(&self, key: &str) -> bool {
        self.children
            .get(self.canonical(key))
            .is_some_and(|children| !children.is_empty())
    }

    pub fn validate_parent(&self, child: &str, parent: &str) -> Result<()> {
        let mut next = Some(parent);
        let mut seen = HashSet::new();
        while let Some(key) = next {
            let key = self.canonical(key);
            if key == self.canonical(child) {
                return fail("an issue cannot be its own ancestor");
            }
            if !seen.insert(key) {
                return fail("the parent hierarchy contains a cycle");
            }
            next = self
                .get(key)
                .and_then(|issue| issue.fields.parent.as_deref());
        }
        Ok(())
    }

    pub fn normalize(&self, issue: &mut Issue) {
        let normalize = |fields: &mut Fields| {
            if let Some(parent) = &mut fields.parent {
                *parent = self.canonical(parent).to_owned();
            }
        };
        normalize(&mut issue.fields);
        if let Some(base) = &mut issue.base {
            normalize(base);
        }
        for conflict in &mut issue.conflicts {
            normalize(&mut conflict.local);
            normalize(&mut conflict.remote);
        }
    }

    pub fn family(&self, name: &str) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut pending = vec![name.to_owned()];
        let mut names = Vec::new();
        while let Some(key) = pending.pop() {
            let Some(issue) = self.get(&key) else {
                continue;
            };
            if !seen.insert(issue.key.clone()) {
                continue;
            }
            names.push(issue.doc_name());
            if let Some(parent) = &issue.fields.parent {
                pending.push(parent.clone());
            }
            pending.extend(self.children(&issue.key));
        }
        names
    }

    pub fn push_order(&self, names: Vec<String>, force: bool) -> Result<Vec<String>> {
        let mut done = HashSet::new();
        let mut visiting = HashSet::new();
        let mut ordered = Vec::new();
        for name in names {
            let mut stack = vec![(name, false)];
            while let Some((name, expanded)) = stack.pop() {
                let Some(issue) = self.get(&name) else {
                    continue;
                };
                if done.contains(&issue.key) {
                    continue;
                }
                if expanded {
                    visiting.remove(&issue.key);
                    done.insert(issue.key.clone());
                    ordered.push(issue.doc_name());
                    continue;
                }
                if !visiting.insert(issue.key.clone()) {
                    return fail("the issue hierarchy contains a cycle");
                }
                stack.push((issue.key.clone(), true));
                let parent = if force {
                    issue
                        .conflicts
                        .iter()
                        .find(|conflict| conflict.field == "parent")
                        .map_or(&issue.fields.parent, |conflict| &conflict.local.parent)
                } else {
                    &issue.fields.parent
                };
                if let Some(parent) = parent.as_deref().and_then(|key| self.get(key))
                    && parent.number.is_none()
                {
                    stack.push((parent.key.clone(), false));
                }
            }
        }
        Ok(ordered)
    }
}
