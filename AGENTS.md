# agt

Rust CLI (one binary, `agt`) whose `agt review` subcommand stores PR/MR-style review threads, runs and reviewer metadata per
repository in the main checkout's `.agt/reviews/`, shared by linked worktrees.
Issues live in `.agt/issues/`. Remote sync with GitHub/GitLab is opt-in.

## Checks

Run all three before committing:

```sh
mise run fmt
mise run clippy
mise run test
```

`mise run install` installs the binary into `~/.cargo/bin`. Commands run as
`agt review <command>`, `agt issue <command>`, `agt cleanup`, `agt sessions <command>`, `agt ps`, `agt worktree <command>`, `agt branch <command>`, `agt wait`, `agt ci`, `agt secrets scan`, `agt pr` and `agt doctor`; the integration helper `Env::agt` prepends
`review` and `Env::issue*` (in `support/issue.rs`, with the fake issue tracker) prepend `issue`.

## Layout

`src/core` (errors, clock, output), `src/store` (locked documents and the
persisted `model`), `src/git.rs` and `src/forge` (services, transports, anchors,
markers) are foundations. `src/review` and `src/issue` are domain modules. Foundations never
import a domain module and domain modules never import each other;
`test/integration/agt.rs` scans `src/` and fails on the offending line. `src/task` is the `agt task` domain (task records in `agents/tasks`). `src/cleanup` is not a domain: it may import every domain's `prune` module and deletes across all namespaces, cache files and orphan processes. `src/sessions` is the `agt sessions` domain (transcript reports and the session index, which lives in `agents/tasks/session-index`) and `agt ps` (`live.rs`) and `src/core/transcripts` is the one reader of Claude Code and Codex transcripts (discovery, session lookup, records, turns) that its reports and index share; `src/core/local.rs` holds the lock directory and private-file helpers `src/task` and the index use. `src/worktree` is the `agt worktree` domain (create, prepare, port, list, remove with a snapshot backup, cleanup of landed work, restore; ports live in `agents/worktrees/ports.json`; `prune.rs` is the offline candidate check `agt cleanup` lists, `src/forge/landed.rs` the read-only PR/MR lookup behind cleanup's forge evidence); `src/core/agents.rs` reads running processes (and `codex` ones with their directory) and live Claude Code session records for it, for `agt ps` and for `src/cleanup/procs.rs`. `src/ci` is the `agt ci` domain (`wait` for PR/MR checks and pipelines, `failures` for GitLab); its forge reads live in `src/forge/github/ci.rs` and `src/forge/gitlab/ci.rs`. `src/secrets` is the `agt secrets` domain (`scan`; its one rule table is `rules.rs`). `src/pr` is the `agt pr` domain (view, list, create, update, merge and auto-merge pause/restore for GitHub and GitLab through `src/forge` transports; `github.rs` and `gitlab.rs` hold the forge calls, `memo.rs` the paused auto-merge methods in `agents/pr/auto-merge.json`). `src/branch` is the `agt branch` domain (`pushed`, `landable`); the base selection it shares with `agt worktree create` lives in `src/git/base.rs`. `src/doctor` is the read-only `agt doctor` domain (host and repository setup checks; it imports only foundations, and its subprocesses go through `forge::transport::Cli::probe` under one deadline). Add new domains as sibling modules.

## Auto-sync

`agt issue` and `agt review` stay local by default. Global TOML configuration lives in
`$XDG_CONFIG_HOME/agt/config.toml`; the main checkout's `.config/agt.toml` overrides fields.
`[sync] auto_pull = true` enables stale reads; `auto_push = true` enables publication
of the changed issue or newly completed review run. `auto` aliases `auto_pull`.

`src/forge/auto.rs` holds shared policy and the 15-second automatic-call deadline.
`src/issue/auto.rs` and `src/review/commands/auto.rs` choose the eligible records.
Automatic failures preserve local changes. Private notes/links never leave the machine.
Explicit remote commands opt in independently; `--offline` and `AGT_OFFLINE=1` block them too.
Tests must opt in when checking automatic behavior and isolate HOME/XDG/git/forge tools.

## Release

Bump `version` in `Cargo.toml`, land it on `main`, then push a `v<version>` tag;
the release workflow builds `agt-<tag>-<target>.tar.gz`, one `agt` file each. Hosts install the version pinned in
the dotfiles mise config (`home/dot_config/mise/config.toml` in the chezmoi
source), so bump that pin, and the minimum version in the review skill's
`references/store.md` when its rules need the release, in one dotfiles commit.

## Conventions

- Integration tests live in `test/integration/` (one test binary) and fixtures
  in `test/fixtures/`; unit tests stay in `#[cfg(test)]` modules.
- Tests MUST use a temporary `HOME`, `XDG_STATE_HOME`, `XDG_CONFIG_HOME` and git
  repository, and fake `gh`/`glab` through `AGT_GH_BIN` and
  `AGT_GLAB_BIN`. They never touch real state or call a real forge.
- Build output goes to `dist/target` (see `.cargo/config.toml`).
- Git, `gh` and `glab` run as subprocesses so behavior matches the command line.
