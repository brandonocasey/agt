# agt

**Keep coding work organized across agents, sessions, and branches.**

`agt` is a command-line toolkit for people working with coding agents.
It keeps review findings, issues, and task records outside the chat.
It also manages Git worktrees and reports agent activity, costs, and CI results.

Use it from your terminal, scripts, Claude Code, Codex, or another coding agent.
You or your agent do the coding and reviewing; `agt` records and coordinates that work.

[Get started](docs/getting-started.md) · [Command reference](docs/reference.md)

## Why use it?

Coding work often outlasts a chat session.
A reviewer finds a problem, another agent fixes it, and a later session checks it.
Meanwhile, branches, worktrees, and background jobs accumulate.

`agt` gives that work a record you can inspect and reuse:

- **Continue a review later.** Keep findings, replies, decisions, and fixes together.
- **See what still needs review.** Track reviewed changes across commits, rebases, and squashes.
- **Work on several tasks.** Give each branch its own checkout, dependencies, and development port.
- **Find earlier work.** Search past prompts and inspect running Claude Code and Codex sessions.
- **Control what gets shared.** Keep issues and reviews local, with optional GitHub or GitLab sync.

You can adopt one command without setting up the rest.
Local issues and reviews need a Git repository, but no hosted project or login.

## What it offers

### Reviews that survive the chat

`agt review` stores comments and findings against a branch or commit range.
Threads support file locations, replies, questions, and resolutions.
Review records capture who reviewed which changes and what happened to each finding.

A review can start before a pull request or merge request exists.
Later, you can import its discussions and publish local findings and summaries.
Coverage checks help identify changes that still need review after fixes or rebases.

`agt qa` records testing the same way: which checks ran, where, with what outcome and
evidence, and the bugs they found. Checks go stale when the content changes, and the PR/MR
summary shows them beside the review.

[Review commands](docs/reference.md#commands) · [QA](docs/reference.md#qa) · [Coverage rules](docs/reference.md#reviewed-counting-and-pruning-rules)

### Issues and tasks you can pick up later

`agt issue` keeps a local backlog with comments, private notes, and links to related work.
It can also mirror GitHub and GitLab issues and send queued updates.
Private notes and local links stay on your machine.

`agt task` stores task records for work that spans sessions.
Issues and reviews belong to a repository; task records live in your user state directory.

[Issues](docs/reference.md#issues) · [Tasks](docs/reference.md#tasks)

### Separate workspaces for parallel work

A Git worktree gives a branch its own directory.
`agt worktree` creates these directories and can install dependencies and assign development ports.
Its reports show branch state, changed files, and active agents.
Removal saves a backup that you can restore later.

`agt branch` checks whether a branch matches its remote and whether merging would conflict.
`agt cleanup` previews stale records and other leftovers before deletion.
Removing landed worktrees requires selecting them explicitly.

[Worktrees](docs/reference.md#worktrees) · [Branch checks](docs/reference.md#branch) · [Cleanup](docs/reference.md#cleanup)

### Visibility into agent work

`agt ps` shows running Claude Code and Codex sessions, including status and working directory.
`agt sessions` searches past prompts and reports token usage, tool errors, and permission denials.
Cost reports use transcript usage and your configured model prices.
Models without prices are marked as unpriced.

These reports help you find earlier decisions and understand where agent time and spending went.

[Running agents](docs/reference.md#ps) · [Session reports](docs/reference.md#sessions)

### Routine checks and GitHub/GitLab workflows

- `agt pr` views, creates, updates, and merges pull requests and merge requests.
- `agt ci` waits for checks; its GitLab failure report compares failures against default-branch history.
- `agt secrets` scans repository files for credential patterns without printing matched values.
- `agt doctor` checks host tools and repository setup, with hints for problems it finds.
- `agt check run` skips a test or lint command that already passed on the same git tree, recording a pass only if the code did not change during the run.
- `agt wait` reruns a check until it succeeds or reaches a timeout.

[Pull and merge requests](docs/reference.md#pr) · [CI](docs/reference.md#ci) · [Secrets](docs/reference.md#secrets) · [Doctor](docs/reference.md#doctor) · [Check](docs/reference.md#check) · [Wait](docs/reference.md#wait)

## How it fits into your workflow

`agt` is one Rust binary that uses Git and the GitHub/GitLab command-line tools.
Most commands return JSON for agents and scripts.
Activity and diagnostic commands also provide readable tables or text.

Issues and reviews stay local by default.
Syncing reads remote data; publishing or pushing sends eligible local changes.
Automatic pulls and pushes are separate settings, both off by default.
`--offline` (also `AGT_OFFLINE=1`) is a global flag that blocks network access: the issue and review commands stay local, `agt pr`, `agt ci` and the remote checks refuse to run, and `agt branch check` and `agt worktree` skip the remote refresh. The reference lists each command's behavior.

Repository records live in the main checkout's `.agt/` directory, shared by linked worktrees.
Keep that directory out of Git; it contains your local review and issue data.
You choose when to share records through GitHub or GitLab.

GitHub integration needs an authenticated `gh`; GitLab integration needs an authenticated `glab`.
Session reports support Claude Code and Codex transcripts.
Release binaries cover Apple Silicon macOS and Linux on x64 or arm64.
Windows is not supported.

## Start here

Follow [Getting started](docs/getting-started.md) to install `agt` and try local issues and reviews.
Use the [command reference](docs/reference.md) for flags, storage, and detailed behavior, and [configuration and environment](docs/reference.md#configuration-and-environment) for config files and `AGT_*` variables.
For development checks and repository conventions, see [AGENTS.md](AGENTS.md).
