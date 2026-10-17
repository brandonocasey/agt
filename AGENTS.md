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
`agt review <command>`, `agt issue <command>`, `agt cleanup`, `agt sessions <command>`, `agt wait`, `agt watch` and `agt ci`; the integration helper `Env::agt` prepends
`review` and `Env::issue*` (in `support/issue.rs`, with the fake issue tracker) prepend `issue`.

## Layout

`src/core` (errors, clock, output), `src/store` (locked documents and the
persisted `model`), `src/git.rs` and `src/forge` (services, transports, anchors,
markers) are foundations. `src/review` and `src/issue` are domain modules. Foundations never
import a domain module and domain modules never import each other;
`test/integration/agt.rs` scans `src/` and fails on the offending line. `src/task` is the `agt task` domain (records and the session index in `agents/tasks`, the
Node `agent-task` format). `src/cleanup` is not a domain: it may import every domain's `prune` module and deletes across all namespaces, cache files and orphan processes. `src/sessions` is the `agt sessions` domain (transcript reports; `py.rs` keeps the `agent-sessions` script's number and table formats) and `src/core/transcripts.rs` names the transcript roots it shares with `agt task`. Add new domains (`watch`, `ci`) as sibling modules.

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
