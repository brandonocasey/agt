# agent-review

Rust CLI that stores PR/MR-style review threads, runs and reviewer metadata per
repository under `$XDG_STATE_HOME/agents/reviews/`, and syncs them with GitHub
and GitLab.

## Checks

Run all three before committing:

```sh
mise run fmt
mise run clippy
mise run test
```

`mise run install` installs the binary into `~/.cargo/bin`.

## Release

Bump `version` in `Cargo.toml`, land it on `main`, then push a `v<version>` tag;
the release workflow builds the binaries. Hosts install the version pinned in
the dotfiles mise config (`home/dot_config/mise/config.toml` in the chezmoi
source), so bump that pin, and the minimum version in the review skill's
`references/store.md` when its rules need the release, in one dotfiles commit.

## Conventions

- Integration tests live in `test/integration/` (one test binary) and fixtures
  in `test/fixtures/`; unit tests stay in `#[cfg(test)]` modules.
- Tests MUST use a temporary `HOME`, `XDG_STATE_HOME`, `XDG_CONFIG_HOME` and git
  repository, and fake `gh`/`glab` through `AGENT_REVIEW_GH_BIN` and
  `AGENT_REVIEW_GLAB_BIN`. They never touch real state or call a real forge.
- Build output goes to `dist/target` (see `.cargo/config.toml`).
- Git, `gh` and `glab` run as subprocesses so behavior matches the command line.
