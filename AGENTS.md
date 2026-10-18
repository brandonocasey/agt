# agt

Rust CLI (one binary, `agt`) whose `agt review` subcommand stores PR/MR-style review threads, runs and reviewer metadata per
repository under `$XDG_STATE_HOME/agents/reviews/`, and syncs them with GitHub
and GitLab.

## Checks

Run all three before committing:

```sh
mise run fmt
mise run clippy
mise run test
```

`mise run install` installs the binary into `~/.cargo/bin`. Commands run as
`agt review <command>`, `agt issue <command>`, `agt cleanup`, `agt sessions <command>`, `agt ps`, `agt worktree <command>`, `agt wait` and `agt ci`; the integration helper `Env::agt` prepends
`review` and `Env::issue*` (in `support/issue.rs`, with the fake issue tracker) prepend `issue`.

## Layout

`src/core` (errors, clock, output), `src/store` (locked documents and the
persisted `model`), `src/git.rs` and `src/forge` (services, transports, anchors,
markers) are foundations. `src/review` and `src/issue` are domain modules. Foundations never
import a domain module and domain modules never import each other;
`test/integration/agt.rs` scans `src/` and fails on the offending line. `src/task` is the `agt task` domain (task records in `agents/tasks`). `src/cleanup` is not a domain: it may import every domain's `prune` module and deletes across all namespaces, cache files and orphan processes. `src/sessions` is the `agt sessions` domain (transcript reports and the session index, which lives in `agents/tasks/session-index`) and `agt ps` (`live.rs`) and `src/core/transcripts` is the one reader of Claude Code and Codex transcripts (discovery, session lookup, records, turns) that its reports and index share; `src/core/local.rs` holds the lock directory and private-file helpers `src/task` and the index use. `src/worktree` is the `agt worktree` domain (create, prepare, port, list, remove with a snapshot backup, cleanup of landed work, restore; ports live in `agents/worktrees/ports.json`; `prune.rs` is the offline candidate check `agt cleanup` lists, `src/forge/landed.rs` the read-only PR/MR lookup behind cleanup's forge evidence); `src/core/agents.rs` reads running processes (and `codex` ones with their directory) and live Claude Code session records for it, for `agt ps` and for `src/cleanup/procs.rs`. `src/ci` is the `agt ci` domain (`wait` for PR/MR checks and pipelines, `failures` for GitLab); its forge reads live in `src/forge/github/ci.rs` and `src/forge/gitlab/ci.rs`. Add new domains as sibling modules.

## Auto-sync

`agt issue` and `agt review` reads sync first when the local copy is older than `[sync] max_age_secs`
(`src/forge/auto.rs` holds the policy, the one stderr line and the 15 s cap; `src/issue/auto.rs` and
`src/review/commands/auto.rs` decide what to sync). Only the auto path sets a deadline in
`src/forge/transport.rs`; explicit `sync` has none. Auto-sync only calls what `sync` calls, so it
stays read-only on the forge, and a failure never fails the read. A new read command syncs through
these modules or says why it stays local. Tests that check local behavior run offline
(`AGT_OFFLINE=1`, `Env::offline`).

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
