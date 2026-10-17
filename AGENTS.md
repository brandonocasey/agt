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
`agt review <command>`; the integration helper `Env::agt` prepends `review`.

## Layout

`src/core` (errors, clock, output), `src/store` (locked documents and the
persisted `model`), `src/git.rs` and `src/forge` (services, transports, anchors,
markers) are foundations. `src/review` is a domain module. Foundations never
import a domain module and domain modules never import each other;
`test/integration/agt.rs` scans `src/` and fails on the offending line. Add new
domains (`issue`, `watch`, `ci`, `task`) as sibling modules.

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
  `AGT_GLAB_BIN` (the pre-0.5 `AGENT_REVIEW_*` names still work). They never touch real state or call a real forge.
- Build output goes to `dist/target` (see `.cargo/config.toml`).
- Git, `gh` and `glab` run as subprocesses so behavior matches the command line.
