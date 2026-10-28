# Getting started

This walkthrough installs `agt` and records an issue and a review question locally.
You need a Git checkout with at least one commit.
A GitHub or GitLab account is unnecessary for the local commands.

For the broader purpose, read the [overview](../README.md).
For command details, use the [reference](reference.md).

## Install

Release binaries support Apple Silicon macOS and Linux on x64 or arm64.
Windows is not supported. Use Git 2.38 or newer for full review coverage checks.

With mise installed, use the GitHub release:

```sh
mise use -g github:brandonocasey/agt
```

If repository access requires authentication, authenticate `gh` with an account that has access first.

Alternatively, build from an `agt` source checkout with Rust 1.95 or newer:

```sh
cargo install --locked --path .
```

This installs `agt` in `~/.cargo/bin`; ensure that directory is on your `PATH`.
Contributors using the repository's mise setup can run `mise run install` instead.

Check the installed version:

```sh
agt --version
```

## 1. Choose a repository

Run the remaining commands inside the Git repository where you want to track work.
Review commands use the current branch unless you select another target.

Add `/.agt/` to your global Git ignore before storing records.
It holds local issues and review data in the main checkout.
Linked worktrees share those records. `agt` does not edit your ignore file.

The examples use `--offline`, so they stay local even when automatic sync is configured.
No configuration file is required.

## 2. Save a piece of work

Create a local issue:

```sh
agt issue --offline create --title "Document retry behavior" --body "Explain when requests are retried and when they stop."
```

The JSON response contains the issue's generated key, beginning with `local-`.
That key identifies it in later commands.

List open issues, then find this one by its text:

```sh
agt issue --offline list
agt issue --offline list --text "retry"
```

The issue stays available after your chat or terminal closes.
Use [issue commands](reference.md#issues) to edit it, add notes, or link related work.

## 3. Keep a review question with the branch

Record a question for whoever reviews the current branch:

```sh
agt review --offline comment --type question --body "Does the documentation explain when retries stop?"
```

List open threads in a readable format:

```sh
agt review --offline list --open --format prompt
```

The output includes the question and its thread ID.
Use that ID when replying or resolving it with the [review commands](reference.md#commands).

Inspect the branch's review state:

```sh
agt review --offline status
```

Adding a question does not mark the branch reviewed.
You or your agent perform the review.
Review runs and passes record its findings and the changes reviewed.
`agt` stores that evidence; it does not launch an AI reviewer.

## 4. Choose what to connect

The issue and question above remain on your machine.
You can keep working locally or connect a supported GitHub or GitLab repository.
Remote commands use the repository's `origin` and an authenticated `gh` or `glab`.
Issues can also use an explicitly configured tracker.

Read [review sync and publication](reference.md#sync) or [issue sync](reference.md#issues) before sharing records.
Both offer previews of outgoing changes: `review publish --dry-run` and `issue push --dry-run`.
These previews can read remote data but do not publish it. Sending needs `[forge] post = true` in `~/.config/agt/config.toml`.

Automatic syncing is optional; [the sync settings](reference.md#issues) control pulls and pushes separately.
Use only the parts of `agt` that fit your work.
Config file paths, their keys and the `AGT_*` environment variables are listed in [configuration and environment](reference.md#configuration-and-environment).
