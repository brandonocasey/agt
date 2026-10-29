# Command reference

Use this page to look up commands, defaults, storage, and integration behavior.
For the project’s purpose, read the [overview](../README.md).
For installation and a local example, follow [Getting started](getting-started.md).

## On this page

- [Output and errors](#output-and-errors) and [configuration and environment](#configuration-and-environment)
- Review: [targets](#targets), [commands](#commands), [coverage](#reviewed-counting-and-pruning-rules), [sync](#sync), [publish](#publish), [store](#store), [finding input](#finding-input)
- [QA](#qa): checks, QA findings and the QA approval
- [Issues](#issues) and [tasks](#tasks)
- [Worktrees](#worktrees) and [branch checks](#branch)
- [Pull and merge requests](#pr) and [CI](#ci)
- [Sessions](#sessions) and [running agents](#ps)
- [Secrets](#secrets), [wait](#wait), [doctor](#doctor), and [cleanup](#cleanup)
- [Compatibility](#compatibility) and [testing](#testing)

## Compatibility

Git 2.31 or newer must be on `PATH`. Git 2.38 or newer supports the full
review coverage checks; older versions count every merge commit as unreviewed.
Repositories with SHA-1 or SHA-256 object IDs both work.

Up to 0.4.0, the binary was `agent-review`; its commands lacked the `review` prefix.
Its stores remain readable. Posted comment markers retain the `agent-review` name.

A `v*` tag publishes release archives through the [release workflow](../.github/workflows/release.yml).

## Output and errors

Most commands print JSON on stdout, indented at a terminal and compact when piped.
Exceptions include session reports, `ps`, `doctor`, `secrets`, and `review list --format prompt`.
Several offer `--json`; each command's section describes its output.
Task records and `sessions report --json` always use indented JSON, including when piped.
`worktree port --env` prints a shell assignment, and `wait` prints a completion message.
`cleanup --count` prints a count message when saved candidates remain.

Errors print `agt: error: <message>` on stderr and normally exit with status 1.
`agt ci` and `agt secrets` exit with status 2 for an error, with the same message line.
Usage errors exit with status 2. Some commands use additional exit statuses for
results such as conflicts, timeouts, or warnings; see their sections below.
Timestamps are RFC 3339 in UTC, such as `2026-10-16T12:00:00Z`.

## Configuration and environment

Configuration files live under `$XDG_CONFIG_HOME/agt/` (`~/.config/agt/` when
`XDG_CONFIG_HOME` is unset or relative). None is required.

| File | Keys and defaults | Described in |
| --- | --- | --- |
| `$XDG_CONFIG_HOME/agt/config.toml` | `[sync]` `pull = "manual"`, `push = "off"`, `max_age_secs = 300`, `remotes = "all"`; `[issues]` and `[pull_requests]` `pull`, `push` (unset: `[sync]`); `[[tracker]]` `host`, `repo`, `forge`, `remote`, `pull`, `push`, `enabled = true`, `issues`, `pull_requests`; `[worktree]` `roots` (unset: `~/Projects/*/*` and `~/projects/*/*`); `[worktree.hooks]` `create = []`, `remove = []` | [Issues](#issues), [worktrees](#worktrees) |
| `<main checkout>/.config/agt.toml` | The same keys, overriding the global file field by field (arrays replace); linked worktrees read the main checkout's file | [Issues](#issues) |
| `$XDG_CONFIG_HOME/agt/hosts.json` | Host name to `{"service": "github" \| "gitlab"}` | [Sync](#sync), [PR](#pr), [CI](#ci), [Cleanup](#cleanup) |
| `$XDG_CONFIG_HOME/agt/model-prices.json` | A `models` object of rates per model | [Sessions](#sessions) |

Unknown keys and invalid values in the TOML files are errors that name the file and the key.

Environment variables agt reads, or sets for programs it runs:

| Variable | Used by | Meaning | Kind |
| --- | --- | --- | --- |
| `AGT_OFFLINE` | every command | Anything but empty, `0` or `false` acts as the root `--offline` flag, which every command that reaches a remote honours (see [Offline](#offline)) | user |
| `AGT_PRICES` | `sessions` | Price file to use instead of `model-prices.json` | user |
| `AGT_LS_REMOTE_TIMEOUT` | `worktree`, `cleanup` | Seconds a remote check may take (default 10) | user |
| `AGT_HOOK`, `AGT_WORKTREE`, `AGT_MAIN_CHECKOUT`, `AGT_BRANCH` | `worktree` hooks | Set by agt for each hook command | set by agt |
| `AGT_NOW` | all | Unix seconds replacing the current time | test only |
| `AGT_SYNC_CAP_MS` | `review`, `issue` | Milliseconds replacing the 15-second auto-sync limit | test only |
| `AGT_GH_BIN`, `AGT_GLAB_BIN` | everything that calls a forge | Program to run instead of `gh` or `glab` | test only |
| `AGT_GIT_BIN`, `AGT_MISE_BIN` | `doctor` | Program to run instead of `git` or `mise` for its version checks; other commands still run `git` | test only |
| `AGT_CLEANUP_TMP` | `cleanup` | Directory standing in for `/tmp` when looking for session scratch files | test only |
| `AGT_CLEANUP_PID` | `cleanup` | Limits the orphan-process sweep to this pid; `0` looks at none | test only |

## Targets

A target is one thing under review. Commands pick it like this:

- By default, the current branch. The target key is the branch plus its
  merge-base with the default branch (`origin/HEAD`, else local `main`,
  `master` or `init.defaultBranch`; override with `--default-branch <ref>`, which the
  target remembers as `default_branch`, so later commands on it need not repeat the flag).
  The target also stores the branch's reflog birth (time and sha of the oldest
  `refs/heads/<branch>` reflog entry, shared by linked worktrees). Lookup takes,
  in order: a target of this branch with the same reflog birth; the exact key;
  a target of this branch that shares a reviewed commit or patch-id with it
  (covers rebases and merges from the default branch when the reflog is
  missing); else a new target. So a rebase keeps the target even when every
  commit conflicts, and a branch deleted and created again (same name, same
  base) starts fresh. Two births in the same second at the same sha cannot be
  told apart. A branch name longer than 100 encoded characters is cut and
  followed by a hash of the full name, so keys stay within the store's limit.
- `--range <from>..<to>` reviews a range; `--range <rev>` reviews one commit.
  Each gets its own target.
- `--target <key>` uses a stored target by its key. A branch target's head is
  `HEAD` when that branch is checked out or when `HEAD` is detached on top of
  the local branch or the synced PR/MR head (a review worktree with fix commits,
  also after a force-push); else the local branch; else the synced PR/MR head.
  An unrelated `HEAD` never stands in. With none of these, reading commands
  (`status`, `list`, `show`) use the last pass head, or report `head: null` and
  `current: null` when there is no pass; writing commands fail. With a `gh-<n>`
  or `gl-<iid>` target, `sync` and `publish` use that PR/MR without `--pr`, also
  on a detached `HEAD`.

On the default branch itself, the root commit names the target. A pass there
covers uncommitted work by default; `pass start --base <rev>` reviews
`<rev>..HEAD` and still counts as a whole-branch pass (never `partial`): it
covers its commits, advances `last_reviewed_head` and `review_from`, and
`run end` records its repair. `--range` reviews commits too.

## Commands

`agt review` records reviews performed by people or agents; it does not launch reviewers.
A run groups a review and its repairs. A pass records one round within that run.
Commands in this section use the `agt review` prefix.

A `full` pass without `--run` starts a run and its first pass. A `full` pass opens a
new cycle (`c1p0`, `c2p0`); `fix-check` and `reveal` passes continue the current
cycle (`c1p1`, `c1p2`) and need `--run`.

```sh
agt review pass start --kind full --reviewers - <<'EOF'
[{"model": "claude-opus-5-5", "effort": "xhigh", "harness": "claude-code",
  "harness_version": "2.1.296", "starting_context": "fresh",
  "session": "a0b5ad828155531d2"}]
EOF
# {"target": "branch-feat%2Flogin-1a2b...", "run": "rv-7k2m", "pass": {"id": "c1p0", ...}}
agt review pass start --run rv-7k2m --kind fix-check --reviewers - < reviewers.json
```

Reviewer ids are `r<n>`, assigned in order unless you pass `"id"`. Two ids are
reserved: `manual` for the main session's own pass and `fix` for the fixer,
who signs fix replies and shows as the main session in the summary; give them
model, effort, harness and version like any reviewer. `"session"` is the
harness's id for the reviewer, such as a subagent or session id; posted
comments name its first 8 characters (`agent a0b5ad82`, or `session …` for
`manual` and `fix`). A reviewer continued in a later pass
keeps its id; give only `"id"` and `"starting_context"` and the rest is copied
from its earlier pass. `--base <rev>` sets the start of the reviewed delta
(default: the merge-base for a full pass, the previous pass's head otherwise). A
pass whose base is after the merge-base, and that does not continue a
whole-branch pass from its head, is `partial`: it marks the commits it reviewed
but never covers the whole head. On a range or commit target, `--head <rev>`
reviews a later head, such as a fix for a fix check; it MUST descend from the
target's end, else the pass is refused. Pass output leaves out the per-commit
patch-ids (`commit_count` gives their number); `show <run>.<pass>` prints them.

A first pass that fails to start leaves no run behind.

```sh
agt review pass end --run rv-7k2m --pass c1p0 --lock
agt review pass end --run rv-7k2m --pass c1p0 --findings - --reviewer r1 < all.json
agt review finding kill rv-7k2m.c1p0.r1.f2 --reason "The caller checks it."
agt review run end --run rv-7k2m
agt review run end --run rv-7k2m --reveal-agree 2 --reveal-disagree 0
```

`pass end --findings <file|->` adds findings and ends the pass in one write:
each finding names its `"reviewer"`, or
takes `--reviewer`; `[]` ends a clean pass. Output lists the new ids under
`added` and `killed`, each in input order. The input may instead be an object,
`{"findings": [...], "usage": [{"reviewer": "r1", "tokens": 52000,
"duration_ms": 61000, "tool_uses": 9}]}`, which also stores what each reviewer
spent (as its harness reports it) on the pass. `pass end --lock` freezes the pass for a blind review:
adding findings to it then fails. `finding kill` moves a surviving finding to its pass's killed list, also
on a locked pass; `finding kill` refuses a finding that is posted or being posted to the
PR/MR, so resolve it with `--stale` instead. `run end` closes the run, records commit patch-ids
for every pass whose reviewed tree now equals `HEAD`'s tree (see below), records
`HEAD` (on a range or commit target, its end or `--head <rev>`, which MUST
descend from the end) as the run's `repaired_head` (commits after the last pass
up to it are the run's `repair`), stores the reveal counts for the summary, and prunes
old targets of this repository. Its output ends with `status`: the brief
`status` of that head, so no separate call is needed. `run end --usage <file|->`
stores what the main session spent, as an array of `{"role": "fix", "tokens",
"duration_ms", "tool_uses"}` (roles such as `fix`, `manual`, `main`). Its output's
`spent` gives the run's wall time, the time inside passes (`reviewing_secs`) and
outside them (`fixing_secs`), and the reported reviewer and main-session
tokens; the summary's steps show each run's time.

`publish --approve` first records an approval of the head as a general thread,
refused while a `bug`, `requirement` or `question` is open; approving the same
head again reuses the same thread, and `--formal` upgrades a comment approval.
`publish` posts no comment for it, except the `COMMENT` review or GitLab comment below (the text, default `Approved at <sha>.` or
`--body`, stays in the store, also after `sync` reads back the review or comment that carries a formal
approval).
The summary headline shows ` · 🔍 approved` while the PR/MR head is the approved head and
no open finding blocks approval. The author flags (`--author`,
`--reviewer`, `--model`, ...) sign the approval as for `comment`. With `--dry-run`
the approval is still recorded locally, and nothing is posted.
`--approve --formal` submits the host's approval: a GitHub `APPROVE` review whose body is
only the hidden marker, or GitLab's `approve` at that head. On your own GitHub PR, which
GitHub refuses to approve, a `COMMENT` review carries the approval text with its footer
instead, posted once, and `skipped` lists `<id>/formal`. When GitLab will not let you
approve (HTTP 401 or 403; not a 409 for a head that moved),
a comment (a note, which needs no resolving) carries the approval text with its footer
instead, posted once, and the refusal's `posted` entry adds `"fallback": "comment"`; a
comment that fails to post is sent by the next `publish`. A host
refusal of the formal approval (an HTTP 4xx: already approved, not allowed)
shows as an `error` under `posted`, blocks nothing and is remembered, so
`publish` does not send it again; `publish --approve --formal` again retries only the host's
approval. A failure that is not a refusal (a timeout, a 5xx, the tool failing)
shows as an `error` too but is not remembered: the next `publish` sends the
approval again. An accepted approval is never sent again. An approval posts
only while the PR/MR head is the head it approved and no blocking finding is
open, never counts as open, and goes to someone else's PR/MR with
`publish --not-own --approve` (the approval is named for you).

A wrong run, pass or reviewer id fails with the valid ones named: the open
runs, the run's passes, or the pass's reviewers. Run ids are unique in the repository. With no `--target` or `--range`, a
command that names a run (`--run`, a `--reviewer <run>.<pass>.<id>`, or a thread, killed
finding or pass id that starts with one) works on the range, commit or PR/MR target holding
that run, unless the current branch's own target holds it; so a `--range` review needs
`--range` only on the first `pass start`. Another branch's target is never picked, since it
would evaluate that branch's tip rather than the checked-out work.

Threads for humans and agents:

```sh
agt review comment --body "Why 3 retries?" --path src/net.rs --line 42
agt review comment --body - --type question < note.md   # general comment
agt review reply h1 --body "Matches the API limit."
agt review reply rv-7k2m.c1p0.r1.f1 --body "Fixed in 9e8d7c6" \
  --reviewer rv-7k2m.c1p0.fix --model claude-opus-5-5 --harness claude-code \
  --session dcec84d5-4c7a-492f-ba78-a9ddd3d1bc7f
agt review resolve h1 --answered
agt review resolve rv-7k2m.c1p0.r1.f3 --issue '#57'          # left for a follow-up issue
agt review comment --body "Same race as #12." --issue '#12'  # names a related issue
agt review resolve rv-7k2m.c1p0.r1.f2 --triage question     # held, stays open
# Fixed: triage, resolution and a "✅ Fixed in <sha> <subject>." reply for each, in one write
agt review resolve rv-7k2m.c1p0.r1.f1 rv-7k2m.c1p0.r2.f1 --fixed 9e8d7c6 \
  --reviewer rv-7k2m.c1p1.fix
# A fix check disagrees: reopen with a "❌ Not fixed: <reason>." reply
agt review resolve rv-7k2m.c1p0.r1.f1 --not-fixed "still loses data" \
  --reviewer rv-7k2m.c1p1.r1
```

- `comment` options: `--type bug|question|requirement|nit|comment` (default
  `comment`), `--severity high|medium|low`, `--path`, `--line`, `--old-line`,
  `--side new|old`, `--start-line`, `--commit <rev>`. Line numbers refer to
  `--commit`, else `HEAD`, else the uncommitted work tree when it is dirty.
- Author: `--author <login>` for a human, or agent flags `--model`,
  `--effort`, `--harness`, `--harness-version`, `--reviewer`, `--session`.
  `--reviewer <run>.<pass>.<id>` (such as `rv-7k2m.c1p0.r1`) names a reviewer of a stored
  pass and fills the rest from it, and records that run, pass and reviewer on the author; `--session` is the harness's id for the agent,
  such as the main session's. With no author flags the author is the forge login that
  `sync` last authenticated as for this repository, else git `user.name`,
  which the next `sync` (or `publish`) replaces with the forge login.
- `reply --local` keeps the reply in the store only, such as the user's answer
  to a held question: `list --format prompt` shows it and `publish` never posts
  it.
- `resolve` takes one outcome flag, or none to resolve the thread as it is:
  `--fixed <sha>` (triage `fixed`, resolution `fixed <sha>`, plus a reply
  `✅ Fixed in [<sha> <subject>](<commit URL>).`, linked once the target is linked
  to a PR/MR); `--not-fixed <reason>` (reopens with resolution `not fixed: <reason>`
  and a reply `❌ Not fixed: <reason>.`); `--rejected <reason>` (resolves a finding
  that will not be fixed with `rejected: <reason>` and a reply `🚫 Won't fix: <reason>.`;
  the summary counts it as `<n> rejected`); `--stale` (triage and resolution `stale`, reply
  `➖ No longer applies: the code changed.`);
  `--answered` (resolution `answered`, reply `💬 Answered above.`); `--issue <issue>` (resolves a finding left for a
  follow-up issue with `moved to <owner/repo#n>` and a reply `📋 Moved to <owner/repo#n>.`,
  which the host links; the issue must already be on the host, and its `links.reviews` gets the
  target key); a bare `resolve` (reply `✅ Resolved.`); `--triage valid|question` (only records the triage and leaves the thread open;
  `question` marks it held). A finding or QA thread resolved in any way before it was ever
  posted is not posted; the summary counts it, and lists one moved to an issue under
  follow-up issues. Another store that syncs a reply to a general comment reads a fix or
  rejection back.
- `comment --issue <issue>` and `agt qa finding --issue <issue>` record a related issue; the
  posted text and the summary end with `Related: <owner/repo#n>` and the thread stays open. `--reply <text>`
  replaces the default reply, or adds a reply to any `resolve`; the author flags sign it.
  `resolve` takes several ids and changes them all in one write, or none when one is
  missing. With one id the output is the `thread`; with more, `threads` lists each id with
  `resolved`, `resolution` and `held`.

Reading:

```sh
agt review list --open
agt review list --run rv-7k2m --format prompt   # fix-check and reveal input
agt review list --run rv-7k2m --cycle 2         # one cycle's findings
agt review show rv-7k2m.c1p0.r1.f1              # thread or killed finding
agt review show rv-7k2m                          # run
agt review show rv-7k2m.c1p0                     # pass
agt review status
agt review status --head 1a2b3c4
agt review status --full                         # plus commits, runs, records
agt review status --sync                         # import the PR/MR first
agt review status '!12'                          # a PR/MR, worktree or branch by name
agt review status --all                          # every open PR/MR, as a table
agt status [NAME]                                # review and QA together
agt qa status [NAME]                             # QA only
```

`NAME` is a PR/MR (`12`, `#12`, `!12` or its URL), a worktree (directory name or
path) or a branch, tried in that order, and defaults to the current branch. A PR/MR
uses the stored target with that number, else asks the forge for its head branch.
A name that matches two different branches fails and lists both. `NAME` does not
combine with `--target`, `--range` or `--all`.

At a terminal, the three status commands print a report; piped, or with `--json`,
they print JSON, so scripts see the same keys as before plus new ones. `review
status` hides QA and `qa status` shows only QA. The report shows the PR/MR (link,
branch, author, age, state), `N of M commits unreviewed` with each commit, the open
findings and held questions, the commits untested since the newest commit a check
ran on (a rebase with the same tree counts as tested), and what was fixed with each
fix commit. New JSON keys: `opened_at`, `untested_commits`, `tested_head` and
`fixed` (`threads`, `checks`).

`--all` prints `MR | BRANCH | OPENED | UNREVIEWED | UNTESTED | OPEN | FIXED | QA`:
open PRs/MRs from the forge, oldest first, then local targets that have records but
no PR/MR. Offline, or without a forge, it lists the stored open reviews and notes
that. Its JSON is `{repo, offline, note, targets: [...]}`.

`list` prints threads (each with `current`: the anchor mapped to `HEAD`, or
for imported threads the host's path, line and outdated state) and killed
findings. `--open` keeps the threads `status` counts as open. `--run` keeps only
that run's findings, and `--cycle <n>` (with `--run`) only that cycle's threads
and killed findings. `--format prompt` prints the text only of comments written
locally or by the authenticated forge user; every other comment shows as
`(comment by another author, text hidden)`, so reviewers never read other
people's comments. A posted thread also shows its forge URL on a `URL:` line.
`--head <rev>` maps anchors to that commit instead of the target's head, such
as a fix after a range target's end. `status` and `list` never write, unless `--sync`
(with `--pr <number|url>` to name the PR/MR) first imports the PR/MR of this branch as
`sync` does; `--offline` refuses it.

`status` never writes: not to the store, not to the git index or object
database. By default it leaves out `commits`, `runs` and tree ids; `--full`
adds them. It reports whether the head is reviewed, `reviews_at_head`, the passes
that cover it, each commit's reviewed state, `unreviewed_commits`,
`last_reviewed_head` (the nearest ancestor of the head that a whole-branch pass
reviewed or a run repaired, across this repository's branch and range targets,
to scope the next review), `review_from` (the parent of the oldest unreviewed
commit, else `last_reviewed_head`, else null: where the next review starts),
open threads, `outdated_threads`, held questions, runs, the linked PR/MR and the
trusted imported review records for the head. Without `--head`, when the
target's branch is checked out, `worktree` reports the same for uncommitted
work. Open threads are unresolved findings and threads still on the host; a
general comment or review body counts only when it is ours (written here, or
posted by `publish` with its marker), so other people's and my own unmarked ones
(a "PTAL") are not open.

Sync with a PR or MR:

```sh
agt review sync                     # refresh every stored review, then publish where push is on
agt review sync --pr 1235           # start (or refresh) the review of one PR/MR
agt review sync --pr https://gitlab.example.com/group/proj/-/merge_requests/3
```

`sync` always pulls (`GET` requests and GraphQL queries), then publishes each synced review
whose tracker has `push = "manual"` or `"auto"`, like `publish`; with the default `push = "off"`
it only pulls and reports the push as kept local. Without `--pr` it never imports a PR/MR the
store does not already have. With `pull = "auto"`, `list`, `show` and `status` refresh stale data; see
[Auto-sync](#auto-sync) and [Sync](#sync).

Publish to an own PR or MR:

```sh
agt review publish --dry-run        # print every payload; writes nothing
agt review publish
agt review publish --pr 1235 --not-own
agt review publish --only rv-7k2m.c1p0.r1.f1,h1
```

`publish` posts what changed locally since the last publish. See
[Publish](#publish).

Cleanup: `agt cleanup --only reviews` (plan) and `--yes` (delete), for every repository.

## QA

`agt qa` records what was tested, where, and what came up. It records tests people or agents
ran; it does not run them. QA lives in the same target as the review, so it takes the same
`--target`, `--range` and `--default-branch` flags, and its threads are review threads.

```sh
agt qa run start --env browser=firefox --env os=linux --model claude-opus-5-5 --harness claude-code
# {"target": "branch-feat%2Flogin-...", "run": "qa-4f2a", "tester": {...}}
agt qa check --run qa-4f2a --name "login with an expired password" --outcome pass \
  --steps - --evidence https://example.com/shot.png --evidence shots/login.png <<'EOF'
Open /login, enter the expired account, submit.
EOF
agt qa check --run qa-4f2a --name "checkout total" --outcome fail --env browser=chrome
agt qa finding --check qa-4f2a.k2 --type bug --severity high --body "The total ignores the coupon."
agt qa run end --run qa-4f2a
agt qa status                       # latest outcome per check, blockers, approvable (also `[NAME]`, `--all`)
agt qa approve                      # refused while anything blocks
agt qa publish --dry-run            # as agt review publish
```

- A check records its name, kind (`manual`, `automated`, `exploratory`), outcome (`pass`, `fail`,
  `blocked`, `skipped`), steps, command, environment, evidence and note, against the current
  head and tree. A dirty work tree is recorded by its tree, with no head.
- `--env key=value` on a check adds to the run's environment; the same key replaces the run's
  value.
- The latest check of a name replaces earlier ones. A check is current while the evaluated tree
  equals the tree it ran against, so a rebase that changes no content keeps it; new content
  makes it stale.
- `finding` opens a general thread linked to the check (`check` field). Its author defaults to
  the run's tester. Reply to and resolve it with `agt review reply` and `agt review resolve`.
  An open QA bug, requirement or question also blocks the review's approval.
- `agt qa comment` takes the arguments of `agt review comment` and an optional `--check <qa-id.kN>`
  that sets the thread's `check`; `agt qa reply` takes those of `agt review reply`. Both store
  the comment with `qa: true`, as `finding` does, and sign it as QA (`--reviewer` is refused).
- `approve` records an approval of the head on the newest QA run. It needs a clean work tree, at
  least one check, no latest check that fails, is blocked or is stale, and no open bug,
  requirement or question found by a check. It sends no host approval.
- `agt review status` carries the same QA state under `qa` (`null` before the first QA run).
  QA runs never count as reviews.
- `show` takes a QA run (`qa-4f2a`) or check (`qa-4f2a.k1`) id, as does `agt review show`.
- `publish` adds QA to the PR/MR summary: open QA findings and current failed or blocked checks
  no open thread explains join the list of what is left to fix (a check that ran against an
  older head does not; the table says `(out of date)`), and the checks table sits under Details. Evidence URLs are linked; local paths are only counted. Each covered QA run leaves
  a `<!-- qa-record v1 id=<run> head=<sha> -->` marker, distinct from review records, which
  `sync` reads to mark the run's record posted. A QA finding is headed
  `**🐛 bug (high)**` on its own line, with the check in the footer
  `> <sub>🧪 agt QA · <check name> · <model></sub>`; it follows the same posting rules as a review
  finding, so on GitHub, where a general comment cannot be resolved, only the summary lists it. With `push = "auto"` for the tracker, `run end`
  publishes the ended QA run, as `agt review run end` does.

## Cleanup

`agt cleanup` checks current and configured repository stores, legacy global stores,
old cache files and orphan processes. Linked worktrees share one repository store. It also lists leftovers for you to remove
by hand. By default it changes nothing and prints a JSON plan; `--yes` deletes. `--only`
limits the pass to `reviews`, `issues`, `checks`, `tasks`, `backups`, `files`,
`processes`, `manual` or `worktrees` (repeat it for several). `worktrees` runs only when named:
it removes landed worktrees and deletes their branches ([Worktrees](#worktrees)), so a plain
`--yes` never does. Before 0.8.0 it was `agt prune`; before 0.14.0 the `worktrees` kind was
`agt worktree cleanup`.

```sh
agt cleanup                            # print the plan
agt cleanup --only backups             # plan for one kind
agt cleanup --yes                      # delete; processes and worktrees are left alone
agt cleanup --only processes --yes     # stop orphan processes
agt cleanup --only worktrees [--all-repos] [--offline] [--dirty] [--recent] [--worktree NAME-OR-PATH]... [--yes]
agt cleanup --count                    # leftovers the last --yes run listed
```

Store documents are kept 90 days (`retention_days`). The rules:

- `reviews`: the review rule above, applied to every repository, and skipped for a
  target whose `.<key>.publish.lock` is held. It also lists the `locks` of the review stores. For legacy global stores, agt records the absolute git common
  directory of a `local/<name>-<hash>` repository in `<dir>/.repo.json`, with `last_seen`
  (refreshed by commands that write, at most once a day; a record without it counts from the
  file's modification time). A repository is listed under `local_repos` at any age, with all its
  targets, only when that directory no longer exists, agt last wrote there 30 or more days ago, and
  the directory is not below a mount root (`/Volumes`, `/media`, `/run/media`, `/mnt`), where
  it may only be unmounted. Otherwise, including a repository that was moved a moment ago, it is
  listed only when every target in it has expired, and its reason says which rule applied. A
  directory with no targets at all (only cache files) is listed once agt last wrote there 90
  or more days ago. Either
  way its directory may hold only targets, `.account.json`, `.repo.json`, `.auto-sync.json` and stray
  lock files; a subdirectory, link or any other file keeps it.
- `issues`: a closed issue, 90 days after the later of its last local and host update,
  and only with no queued change, unsent comment or create, conflict, failed send, note,
  local-only comment, comment deleted on the host, or link to another issue, review,
  task or branch or local PR link. An unreadable timestamp keeps the issue, and so does
  a document with fields this agt does not know or cannot read (with a warning). Open issues,
  `tracker.json` and anything holding such data stay. It also lists the `locks` of the issue stores.
- `checks`: an `agt check run` pass (`.agt/checks/<key>.json`) 7 days after its `ran_at`,
  the age at which its output log goes. An unreadable timestamp keeps the entry. An entry agt cannot read is listed at any age, since only a run of its exact key would replace it; one written by a newer agt is kept with a warning. It also lists
  the `locks` of the check stores.
- `tasks`: a `complete` task 90 days after its `updatedAt`. Active, paused and blocked
  tasks stay, and so does the session index, a cache that `agt sessions search` rebuilds.
- `locks` (a category of the `reviews` and `issues` kinds, not a kind of its own): `.<key>.lock`
  and `.<key>.publish.lock` files whose `<key>.json` is gone, except an issue tracker's
  `.push.publish.lock` while `tracker.json` exists.
- `backups`: `<namespace>-v<n>-<stamp>` directories that agt made before a schema
  migration, 90 days after they were taken, except the newest of each namespace (a
  stamp in the future does not count as the newest), and
  unfinished `.<namespace>-v<n>-<stamp>.partial` copies (`partial_backups`). Other
  names in the same backups directory belong to other tools and are never touched.
  Repository schema backups use `agents/backups/repositories/<clone-id>/`; newest-backup
  protection applies separately per clone and namespace.

The `files` kind covers cache files, which go by age (its categories are `output` and `scratch`). Each top-level entry goes whole, and only once nothing in it changed
for the whole period, so a task that is still writing keeps all of its files:

- `output` category: entries of `$XDG_CACHE_HOME/agents/output` (default `~/.cache`) untouched for
  7 days.
- `scratch` category: entries of `$XDG_CACHE_HOME/agents/scratch` (except `.keep`) and Claude Code
  session directories, `<root>/claude-<uid>/<cwd-slug>/<session>` under `~/.cache/claude-tmp`
  and `/tmp`, untouched for 30 days. A `<cwd-slug>` directory goes once its last session
  has gone. Links are never followed: a `claude-<uid>` directory that is a link or another
  user's is skipped with a warning, and nothing is deleted unless every directory above it,
  up to its root, is a real directory of this user. `agents/output` and `agents/scratch`
  themselves may be links (say, to a bigger disk); they are in the user's own cache.

`processes` lists the user's own processes that carry `CLAUDE_PID` and are a day or more
old, whose Claude Code session is gone. A session is gone when its pid no longer runs, or
when every `<pid>.json` record in `~/.claude*/sessions` (and `$CLAUDE_CONFIG_DIR/sessions`)
names a process that started at another time, by its `procStart` (clock ticks on Linux,
`ps` `lstart` text on macOS). A pid that runs with no record, or with a record that cannot
tell (no `procStart`, or a time inside a daylight saving shift), is left alone. Claude and Codex themselves are never listed, nor agt and its parents.
`--yes` signals them only when `--only processes` is named: SIGTERM, then SIGKILL to any
still running 10 seconds later, each only while its pid still has the planned start time.
Linux reads `/proc`. macOS hides the environment of Apple's own binaries, such as
`/bin/sleep`, so those are never found there.

`manual` lists what cleanup never deletes, in `manual` by section: `branches` (local branches in
the default branch, not protected and not checked out, as `git -C <repo> branch -d <name>`), for
the repositories under the `agt worktree` roots; this uses local ancestry, without network calls.
Merged worktrees are not listed: `agt cleanup --only worktrees --all-repos --offline` plans their
removal. `backups` (worktree backups as `agt worktree backups rm <id> --yes`, then everything in
the backups directory but agt's own, largest first); `codex_releases` (all but the current
and the one before it); and `playwright` (superseded browser builds, in
`$PLAYWRIGHT_BROWSERS_PATH`, else `~/Library/Caches/ms-playwright` and the cache root's
`ms-playwright`). Commands quote any path or branch name the shell would read differently.
`--yes` saves the list
to `$XDG_STATE_HOME/agents/reports/cleanup-candidates.md` (`manual_report`), and `--count`
reads it back without scanning: `agt: <n> cleanup candidates in <path>`, or nothing.

Each item is deleted under its own lock without waiting; one that another agt process
holds is skipped. The output has `plan` (per category a list of `path`, `key`, `reason`
and `bytes`), `totals` and `warnings`; a place that cannot be read is a warning and the
rest of the pass goes on. With `--yes` it adds `deleted`, `skipped` (with
`why`), `signalled` (`pid`, and `signal`: the last one sent) and `bytes_freed`.

## Reviewed, counting and pruning rules

- A pass records its `base..head`, the reviewed tree id, each commit's
  patch-id, the whole range's patch-id (target merge-base to reviewed tree) and
  the head commit's patch-id. For uncommitted work, the tree id comes from a
  copy of the real index in a temporary `GIT_INDEX_FILE` after `git add -A`
  and `git write-tree`, so tracked files that match `.gitignore` stay in the
  tree and the real index is untouched. The work tree counts only when `HEAD`
  is the target's head; otherwise (such as `--target` from another checkout)
  the pass reviews the head commit's tree and is not dirty.
- An ended pass that is not partial covers a head when the head's tree equals
  the pass tree, the range's patch-id equals the pass's whole-range patch-id,
  or the head is the only commit since the merge-base and its patch-id equals
  the pass's whole-range patch-id (a squash). A matching last commit alone
  never covers a longer head: its earlier commits may have changed. A
  partial pass's range patch-id runs from its own base. A commit is reviewed
  when its patch-id, tree or sha matches any ended pass of the target, or its
  sha or patch-id matches an ended pass of another target of this repository
  (such as a `--range` review of the branch's end point). The head is reviewed
  when a pass or trusted record covers it, or when every commit since the
  merge-base is reviewed. Squashes, fixups and plain rebases stay reviewed; a
  conflict resolution changes the patch and shows as unreviewed. Merge commits
  are listed only when their tree differs from git's automatic merge of their
  parents (`git merge-tree --write-tree`, run with objects in a temporary
  directory); such a merge is unreviewed until a whole-branch pass reviews a
  head that contains it (it is then an ancestor of `last_reviewed_head`), and
  while one sits after `last_reviewed_head` no pass covers the head. A run of
  only partial passes adds no repaired head to `last_reviewed_head`.
- `run end` adds commit patch-ids to passes whose tree now equals `HEAD`'s
  tree, so reviewed uncommitted work stays reviewed after it is committed and
  later rebased.
- `reviews_at_head` counts covering passes that are full reviews, plus fix
  checks that found no major finding (a `bug` of `high` or `medium` severity).
  Parallel reviewers in one pass count once. A pass of another target of this
  repository (such as a `--range` review of the branch's end point) also covers
  the head when its whole-range patch-id matches the head's, or a one-commit
  head's patch-id; a tree match alone does not, since it says nothing about the
  base. A run stored under two targets counts once. Each `covered_by` entry
  names its `target` and `run`, so `--target <key> list --run <run>` finds it,
  and the open threads and held questions of each target covering the head or
  the work tree show in `open_threads` and `held_questions`. Each open thread
  names its `target` (null for a target not yet stored) and whether it is `held`;
  `held_questions` lists bare ids, so act on another target's held question
  with `--target`.
- Anchors map through `git diff -U0 -M <anchor commit or tree> HEAD`. A
  new-side line inside a changed hunk, or a deleted file, is outdated; other
  lines shift and follow renames. An old-side anchor is outdated once its file
  changes. Anchors on uncommitted work name a tree that no ref points to;
  `agt review` never writes refs to pin it, so once `git gc` prunes it (by
  default about two weeks later, unless `run end` has since moved the anchor
  to a commit) the anchor counts as outdated.
- A target is deleted 90 days after the later of its PR/MR's close or merge
  and its last change, or, with no closed PR/MR, 90 days after its last change. `sync` and `run end`
  apply this to the current repository; `agt cleanup --only reviews` to every repository.
- Each trusted `review-record v2` marker whose `head` is the evaluated head
  adds its `reviews-at-head` to `reviews_at_head` and makes the head reviewed.
  A summary holds one marker per run; a run's newest marker counts.
  Records of this target's own runs, or of a run whose pass covers the head,
  are skipped; their passes already count.
- A run's `repair` commits (after its last pass, up to `HEAD` at `run end`),
  of this target or another target of this repository (such as a range
  review's `run end --head <fix>`), show as reviewed with `"match": "repair"` in `status` commits (`"pass"` for
  a pass match), but never add to `reviews_at_head`.

## Sync

- Forge, in this order: an entry in `$XDG_CONFIG_HOME/agt/hosts.json`, a JSON object of host to
  `{"service": "github"}` or `{"service": "gitlab"}`, such as
  `{"git.example.com": {"service": "gitlab"}}`, with host names compared without regard to
  case and entries for other hosts ignored; the
  names below that need no lookup (`github.com`, `gitlab.com`); the forge this
  repository last synced that host with; then the CLI checks below, which are
  the slowest. GitHub (`gh`, with `GH_HOST=<host>`) for `github.com` or a host
  `gh auth status --hostname <host>` knows; GitLab (`glab`, with
  `GITLAB_HOST=<host>`) for `gitlab.com`, the host in `GITLAB_HOST`, glab's
  configured host (`glab config get host`, the global default only), or a host
  `glab auth status --hostname <host>` knows (a self-hosted GitLab glab is
  logged in to). Any other host fails with
  `unsupported forge <host>`; the forge is never guessed. The host and
  repository come from `--pr <url>` (a GitHub `/pull/<n>` or GitLab
  `/-/merge_requests/<iid>` URL), `--remote <name|host/repo>` or the tracker of the `origin`
  remote ([Issues](#issues) lists how trackers are chosen).
- PR/MR lookup: the PR whose head is `<origin owner>:<branch>` (GitHub) or the
  MR whose source branch is the current branch (GitLab), first in `origin`,
  then in the repository `origin` was forked from. Open ones win, then the
  newest. On GitLab, only MRs whose source project is `origin` count, so a
  fork's MR with the same branch name is skipped. A closed PR/MR joins the
  branch's target only when its head is the branch head or an ancestor of it;
  an older, unrelated PR/MR of a reused branch name syncs into its own target.
- The target is renamed to `gh-<number>` or `gl-<iid>` under both locks, keeping
  its runs and threads. Away from the branch (such as on a detached `HEAD`),
  `sync` joins the local branch target of the PR/MR's head branch the same way,
  unless that target's passes do not lead to the PR/MR head; later commands on
  the branch find it by reflog birth or shared patch-ids. When the PR/MR target
  already exists, the two merge without dropping a thread: a local thread
  whose id is taken gets the next free `h<n>`, and the same remote thread
  merges its comments, keeping unposted local replies. The first sync that
  links a target to a PR/MR stores `attached_at`. `sync` also adopts patch-ids
  like `run end` and prunes.
- GitHub: review threads with all their comments (`reviewThreads` and inner
  comments are paginated), review bodies (empty and pending ones skipped) and
  issue comments, through GraphQL. GitLab: the MR, all discussions
  (`--paginate`) minus `system` notes, and your draft notes. An
  `individual_note` is a general comment; resolvability comes from the first
  note. GitHub threads take `isOutdated`. A GitLab position is outdated when
  its `head_sha` is not the MR's latest head and the line no longer maps to it
  (`git diff -U0 -M`); when its commits are not in the local repository,
  `outdated` is `null` (unknown). `sync` never fetches commits or writes to the
  repository.
- Merge: remote wins for posted text and resolved state, except a local resolve
  or reopen that `publish` has not sent yet (`resolve_pending`). The review or comment that carries an approval, whose body is only a marker or
  the rendered text, keeps the approval text stored. Local fields
  (type, severity, triage, held, killed findings, anchors of our findings) and
  comments not yet posted stay. Remote threads match local ones by remote id,
  then by a trusted `<!-- review-finding v1 id=<id> ... -->` marker in the first
  comment, then by a trusted `<!-- agent-review id=<local id> attempt=<attempt> -->`
  marker that `publish` adds to every body. That marker claims only a local
  comment in `posting` state with the same attempt id, never a `pending` one:
  another store's comment can carry the same local id (`h1/c1`). Unmatched threads are imported as
  `gh-t<comment id>` (review thread), `gh-r<review id>` (review body),
  `gh-c<comment id>` (general comment) or `gl-n<note id>`, with `-2`, `-3`
  added when that id is taken; an id is never reused, including a killed
  finding's. A GitLab discussion keeps its thread when a reply turns an
  individual note into a discussion. `publish`'s own review bodies are not
  imported.
- Trust: a marker or record counts only when its comment's author is the
  authenticated user, `editor` is null or that user, and (for a marker) it is
  in the thread's first comment. Untrusted ones import as plain comments.
  GitLab notes report no editor (the API has no `last_edited_by`, and
  `updated_at` also changes on resolve), so a maintainer could edit a note of
  mine unseen. On GitLab, records, `Re:` answers, and both `review-finding`
  and `agent-review` marker claims count only when the note's body matches
  a body this store sent (`sent` on the comment or record); anything else imports as a plain
  comment. A record posted from another clone thus never counts on GitLab.
  A record's `earlier` bodies count too: the run's own record comment from
  builds before the summary, and the summary text an edit replaces until the
  edit's attempt shows on the host. Only the last `agent-review` and
  `review-finding` marker of a body counts, and records only in the marker
  lines that end it, so text that quotes a marker claims nothing.
  Trusted records are stored under `records`; a trusted general comment ending
  in `Re: <thread id>` after `Fixed in <sha>` (or after an `agent-review`
  marker), or marked `<thread id>/c<n>` for `n` above 1 (a quote reply), answers that thread: it is added as a reply and, when the thread is
  not resolvable, resolves it as `fixed <sha>` (a `Fixed in` answer, also with
  the sha as a link) or `rejected: <reason>` (a `Won't fix:` or `Rejected:` answer; a quote
  reply's outcome line follows its quote block), or reopens it (any other answer).
- A draft note with a marker for a local comment and its attempt id marks it
  `posting` with its `draft_id`, so a later publish publishes it; a `draft_id` whose draft is gone
  is cleared.
- Threads and comments the host no longer returns are kept and flagged
  `deleted_remote: true` with `deleted_seen_at`; they are never dropped and
  are not open. The flag clears if they come back.
- Malformed GitHub connection pages or repeated pagination cursors fail the fetch.
  The failed fetch leaves stored threads and publication retry markers unchanged.
- The authenticated login is cached in `.account.json` in the repository's
  store directory.

### Auto-sync

Review commands stay local by default. Set `pull = "auto"` to fetch before
`list`, `show` and `status`, and `push = "auto"` to publish after `run end`. Both are
per tracker and come from `[pull_requests]`, `[sync]` or the `[[tracker]]`; see
[Issues](#issues) for the table and the order. The values of `pull_requests` also govern
`agt pr`. Global settings live in `$XDG_CONFIG_HOME/agt/config.toml`; the main checkout's
`.config/agt.toml` overrides individual fields.

Automatic pulls refresh a known PR/MR after `max_age_secs` (default 300).
Unknown or closed PRs/MRs are looked up at most once per interval. Without a
tracker and with unsupported hosts, reads stay local. Only the tracker `agt` picks without being told (the one of
`origin`) is synced. `.auto-sync.json` remembers failed
requests and unsuccessful branch lookups.

Automatic publication saves the completed run first. It sends only that run's
eligible findings, comments and record, excluding unrelated queued content.
The completed run can attach to an existing PR/MR within the same command.
Ownership, approval and review-completion checks still apply. agt never creates
PRs/MRs automatically.

Automatic remote work has a 15-second aggregate limit. Failure prints a warning
and preserves the successful local operation. Pending publications remain available
for explicit `publish`; automatic publication never forces conflicts or ownership overrides.
A push may fetch remote state for reconciliation even with automatic pulls disabled.

`--offline` and `AGT_OFFLINE=1` prohibit automatic and explicit review network calls.
Explicit `sync` and `publish` otherwise remain deliberate network operations,
independent of automatic settings. Explicit sync has no automatic time limit.
An unreadable configuration prevents automatic network work and names its error;
local reads continue.

## Publish

- Sends only when `push` is `manual` or `auto` for the tracker (`[pull_requests]`, `[sync]` or
  the `[[tracker]]`, see [Issues](#issues)). With `off`, the default, it stays local: it exits 0,
  posts nothing, prints why on stderr, naming the key, and returns `kept_local`; `--approve`
  still records the approval. `--remote <name|host/repo>` picks another tracker than `origin`'s.
- Only on a PR/MR authored by the authenticated login, unless `--not-own`. On
  someone else's PR/MR, `--not-own` posts only replies, resolves and reopens on
  threads imported from the host (other people's threads); our findings, local
  threads and the summary are listed under `skipped`. `--not-own --only
  <id,...>`, when the user asks for it, also posts the named surviving
  findings and local threads the user wrote, with their replies and resolves
  (naming them is the consent); the summary never goes to someone else's PR/MR.
  `--approve` (with `--formal` and `--body`) records the head approval before publishing;
  on someone else's PR/MR it is named for you.
- `--only <id>[,<id>...]` limits writes to those threads and their pending
  replies and resolves; every other thread and the summary are listed under
  `skipped`.
  `publish` syncs first, then posts in this order: new threads, sync, replies,
  resolves and reopens, the summary, then a last sync to fill in ids.
- Posted: new surviving findings of ended passes (a locked pass's findings wait
  until a later `reveal` pass of the same cycle, or the run, has ended) and
  local comment threads, new replies, resolve and reopen changes, and the
  summary when a run ended since it was posted or its text changed. Never posted: killed findings,
  findings and QA threads resolved before they were posted (the summary counts them), QA
  threads of runs that ended before the target's `attached_at`, imported comments, local
  comments by another human author, and runs that ended before the target's
  `attached_at` (history from before the PR/MR) with their findings, replies,
  resolves and records (listed under `skipped`).
- A finding opens with its kind and an emoji on a line of its own, then the text:
  `**🐛 bug (high)**`, `**❓ question**`, `**🎯 requirement**`, `**✏️ nit**`. An agent's
  comment, first or reply, ends with a footer in a grey quote, `> <sub>🔍 agt review · <model></sub>`
  or `> <sub>🧪 agt QA · <check> · <model></sub>` (a QA thread or comment); people sign
  nothing. Every body ends with
  `<!-- agent-review id=<local id> attempt=<attempt> -->`, with one attempt id
  per `publish` (`<attempt>` in a dry run). Finding threads also carry
  `<!-- review-finding v1 id=<finding id> sha=<reviewed sha> -->`.
- A thread anchored to lines of a file the PR/MR changes posts where those
  lines sit in its diff. Lines the diff does not show (an unchanged line outside the
  hunks, or lines a later commit changed, such as a fix) post as a thread on the whole file
  where the host has one, so it can be resolved: on GitHub a file-level review comment
  (`subject_type: file`), on GitLab a file discussion (`position_type: file`, or a diff
  discussion on the unchanged line with both line numbers); its first line names the lines.
  A GitLab position the host refuses posts as a general thread that names its lines; the
  draft note is deleted, and a draft left by an earlier attempt gets the same fallback. A
  file thread the host refuses is shown as an `error` under `posted`, and its finding stays
  unposted (an agent's finding on GitHub has no general comment to fall back to); that and
  the host's approval (above) are the only refusals `publish` reports and goes on from; any
  other refused write fails the publish. A thread on a file the PR/MR does not change posts as a general comment whose
  first line names its location. On GitHub an agent never posts a general comment: on someone
  else's PR a finding or comment there, or without an anchor, goes in the body of a review
  (one paragraph each, with its title and footer and its own
  `<!-- agent-review id=<local id> attempt=<attempt> -->` marker, so `sync` claims it after a
  lost response; a review of only a body when no in-diff comment goes out, and more such reviews
  when the paragraphs pass one body's 65,000 characters), where it cannot be resolved or
  answered in place. On your own PR a finding is listed in the summary with its whole text and
  an agent's plain comment is not posted (`skipped` says so). A
  line range (`start_line`..`line`) posts only as one multi-line comment, with
  both ends on the new side of one hunk. Other threads without an anchor post as
  general comments.
- GitHub: in-diff comments go out as one `COMMENT` review per reviewed commit
  (`POST repos/<repo>/pulls/<n>/reviews`; comment ids are read back from the
  review), whose body is its marker after the comments that have no diff line (each with its
  own marker), with `start_line` and `start_side` for a range. Replies and resolves use the GraphQL
  `addPullRequestReviewThreadReply`, `resolveReviewThread` and
  `unresolveReviewThread` mutations; your reply to a general comment is a new
  issue comment in GitHub's quote form (`> <first line>`, `> — @<author>, [comment](<url>)`,
  then your text). An agent's reply to one is not posted.
- GitLab: in-diff comments are draft notes at the latest MR diff, each
  published on its own (never `bulk_publish`), with a `line_range` for a range
  (line codes are the SHA-1 of the path plus the old and new line counters).
  General comments are discussions;
  replies are discussion notes; resolves are `PUT .../discussions/<id>`. The
  summary is a plain note, so it never blocks merging.
- In-diff means the line is in a hunk of `git diff -U3 -M` between the PR/MR
  base and the reviewed commit (GitHub) or MR head (GitLab, after mapping the
  line to it); when the commits are not local it is out of diff.
- The summary is one general comment per PR/MR: posted with the first ended
  run, then edited in place (`PATCH .../issues/comments/<id>` on GitHub,
  `PUT .../notes/<id>` on GitLab), and posted anew when the host no longer has
  it. The whole PR/MR, whatever `--run` selects to post. Its headline is the only status:
  `**⚠️ <N> to fix** at `<head>`` (plus ` · <n> commits not reviewed`),
  `**⏳ <n> new commits not reviewed** at `<head>`` when nothing is open but a PR/MR
  commit no run read, or `**✅ Nothing to fix** at `<head>``, then ` · 🔍 approved` (only while no open finding blocks
  approval) and ` · 🧪 QA approved` for approvals of that head. When the PR/MR head is not
  in the local repository, `<head>` is the newest head a run read and the head is listed under
  `Not reviewed` as not in the local repository. "To fix" counts open findings and current
  failed or blocked QA checks no open thread explains; out-of-date checks never count. Each
  item has two lines: the emoji and the first sentence in bold, then severity, `Waiting on
  the PR author`, `QA: <check>`, the place (`path:line` or `whole PR`), `Related: <issue>`
  and `[thread](url)`, or `not posted as a thread, full text below` and the text quoted.
  `Not reviewed: ...` names the commits no run read. Findings moved to issues are listed under
  **📋 Follow-up issues**. A collapsed Details section holds the QA checks table (a result
  that tested an older head says `(out of date)`) and the closed findings with their outcome,
  with a count of those closed before they were posted. After approval, the follow-ups and
  closed findings drop (a finding closed before the approval stays dropped after a later
  push, by the time it was closed, and a blocking finding opened since shows them again); only the QA table stays. A grey footer names the agt version, what
  ran (🔍 review, 🧪 QA), the reviewers and their models, and the review time. A summary
  longer than 65,000 characters continues in part comments, split at item boundaries and
  edited in place: `<!-- agent-review id=summary/p<N> attempt=<attempt> -->`, each starting
  `Part <N> of the agt summary` and holding items only; the summary ends with a pointer to
  them. The summary ends with one
  `<!-- review-record v2 id=<run> head=<repaired head> reviews-at-head=<N> open=<O> -->`
  per run it covers and `<!-- agent-review id=summary attempt=<attempt> -->`.
  `N` counts the run's passes covering the repaired head by the same rules as
  `status`. Each covered run's `record` names the summary comment and its `parts`.
- Concurrent `publish` commands for one repository and PR/MR run one at a time.
  Each command syncs after waiting, so it does not repeat earlier writes.
- Crash safety: each item is saved as `posting` with the attempt id and the
  exact body it sends (`sent`) before its write and gets its remote id right
  after. A rerun syncs first and matches
  `posting` items by their `agent-review` marker before retrying, and publishes
  a GitLab draft that was created but not published. A `posting` item that a
  sync finds neither as a note nor as a draft goes back to `pending` (keeping
  its attempt id), so it is retried or can be killed.
- `--dry-run` syncs in memory and prints every payload, including the
  summary; it writes nothing to the store, the repository or the forge.

## Store

Review files live in `<main-checkout>/.agt/reviews/`. Linked worktrees use the same
store automatically, even when commands run from a subdirectory. The stored
repository identity remains `<host>/<owner>/<repo>` from `origin`, or
`local/<name>-<hash>` without one. Outside a checkout, or without an available
non-bare main checkout, issue and review commands fail clearly.

Git's common directory holds a private `agt-repository-id` shared by linked worktrees.
It keeps migration and backup ownership stable when the checkout moves.

Records an older agt kept under `$XDG_STATE_HOME/agents/reviews/` and `agents/issues/`
are no longer imported; `agt cleanup` still prunes them by age.
Tasks, session indexes, ports and backups
remain global. Add `/.agt/` to your global Git ignore; agt does not edit that file.

Each target is one JSON file, mode 0600, in a 0700
directory. Writes take a per-target lock (`.<key>.lock`), write a temporary
file in the same directory, fsync it and rename it over the target. Symbolic
links are refused. After `sync`, the key is `gh-<number>` or `gl-<iid>`. Other JSON
documents in a repository directory (their `kind` is not `branch`, `range` or `commit`)
are ignored. A file with a newer `version` than the binary supports is refused with a
message to upgrade, and so is writing a file that holds fields the binary does not know.

Target file:

```json
{
  "version": 1,
  "revision": 7,
  "key": "branch-feat%2Flogin-1a2b3c...-1792152000-5d6e7f...",
  "repo": "github.com/owner/repo",
  "kind": "branch",
  "branch": "feat/login",
  "branch_birth": "1792152000 5d6e7f...",
  "base": "1a2b3c...",
  "head": null,
  "default_branch": "develop",
  "created_at": "2026-10-16T12:00:00Z",
  "updated_at": "2026-10-16T13:10:00Z",
  "review": {
    "forge": "github", "host": "github.com", "repo": "owner/repo",
    "number": 42, "url": "https://github.com/owner/repo/pull/42",
    "state": "open", "closed_at": null, "head_branch": "feat/login",
    "head_sha": "5d6e7f...", "base_sha": "1a2b3c...", "start_sha": null,
    "author": "brandonocasey", "login": "brandonocasey",
    "synced_at": "2026-10-16T13:00:00Z", "attached_at": "2026-10-16T12:50:00Z"
  },
  "records": [
    { "run": "rv-zz99", "head": "5d6e7f...", "reviews_at_head": 2, "open": 0,
      "comment_id": "2001", "url": "https://github.com/...#issuecomment-2001",
      "created_at": "2026-10-16T12:45:00Z" }
  ],
  "runs": [
    {
      "id": "rv-7k2m",
      "started_at": "2026-10-16T12:00:00Z",
      "ended_at": "2026-10-16T13:10:00Z",
      "passes": [
        {
          "id": "c1p0", "cycle": 1, "number": 0, "kind": "full",
          "base": "1a2b3c...", "head": "5d6e7f...", "tree": "9f8e7d...",
          "dirty": false,
          "commits": [{ "sha": "5d6e7f...", "patch_id": "aa11..." }],
          "range_patch_id": "aa11...", "head_patch_id": "aa11...",
          "partial": false,
          "reviewers": [
            { "id": "r1", "model": "claude-opus-5-5", "effort": "xhigh",
              "harness": "claude-code", "harness_version": "2.1.296",
              "starting_context": "fresh" }
          ],
          "started_at": "2026-10-16T12:01:00Z",
          "ended_at": "2026-10-16T12:30:00Z",
          "locked": false,
          "major_found": true,
          "killed": [
            { "id": "rv-7k2m.c1p0.r1.f2", "type": "bug", "severity": "low",
              "anchor": null, "scenario": "...", "evidence": null,
              "tests_run": [], "post_reveal": false,
              "reason": "The guard moved to the caller.",
              "created_at": "2026-10-16T12:20:00Z" }
          ]
        }
      ],
      "repaired_head": "9e8d7c...",
      "repair": [{ "sha": "9e8d7c...", "patch_id": "bb22..." }],
      "reveal": null,
      "record": { "post": "posted", "attempt": "at-x1y2", "sent": "**✅ Nothing to fix** · ...",
                  "head": "9e8d7c...", "remote_id": "2002",
                  "url": "https://github.com/...#issuecomment-2002" }
    }
  ],
  "threads": [
    {
      "id": "rv-7k2m.c1p0.r1.f1",
      "type": "bug",
      "severity": "high",
      "anchor": { "path": "src/net.rs", "line": 42, "old_line": null,
                  "side": "new", "start_line": null,
                  "commit": "5d6e7f...", "tree": "9f8e7d..." },
      "resolved": true,
      "closed_at": "2026-10-16T12:45:00Z",
      "resolution": "fixed 9e8d7c...",
      "triage": "fixed",
      "held": false,
      "finding": { "run": "rv-7k2m", "pass": "c1p0", "reviewer": "r1",
                   "evidence": "...", "tests_run": ["cargo test net"],
                   "post_reveal": false },
      "comments": [
        { "id": "rv-7k2m.c1p0.r1.f1/c1",
          "author": { "kind": "agent", "model": "claude-opus-5-5",
                      "effort": "xhigh", "harness": "claude-code",
                      "harness_version": "2.1.296", "run": "rv-7k2m",
                      "pass": "c1p0", "reviewer": "r1" },
          "body": "Retry loop drops the last response when ...",
          "created_at": "2026-10-16T12:20:00Z",
          "post": "posted", "draft_id": null, "attempt": "at-q3r4",
          "sent": "**bug (high):** Retry loop ...",
          "deleted_remote": false, "deleted_seen_at": null,
          "remote": { "id": "1001", "url": "https://github.com/...#discussion_r1001",
                      "login": "brandonocasey", "is_me": true, "editor": null,
                      "review_id": "5001" } },
        { "id": "rv-7k2m.c1p0.r1.f1/c2",
          "author": { "kind": "human", "login": "Brandon", "git_name": false },
          "body": "Fixed in 9e8d7c6.", "created_at": "2026-10-16T12:40:00Z",
          "post": "pending", "remote": null, "draft_id": null, "attempt": null,
          "sent": null, "deleted_remote": false, "deleted_seen_at": null }
      ],
      "remote": { "kind": "review-thread", "id": "PRRT_kwDO...", "url": "...",
                  "resolvable": true, "outdated": false, "path": "src/net.rs",
                  "old_path": null, "line": 42, "old_line": null,
                  "original_line": 42, "start_line": null,
                  "original_start_line": null, "side": "new",
                  "commit": "5d6e7f...", "review_state": null },
      "resolve_pending": true,
      "deleted_remote": false,
      "deleted_seen_at": null,
      "created_at": "2026-10-16T12:20:00Z",
      "updated_at": "2026-10-16T12:40:00Z"
    }
  ]
}
```

Anchors carry `commit` when the line numbers refer to a commit; for uncommitted
work `commit` is `null` and `tree` names the reviewed tree (`run end` fills in
`commit` once `HEAD` has that tree). Human thread ids are `h<n>`; finding ids are
`<run>.<pass>.<reviewer>.f<n>`. Comment ids are `<thread id>/c<n>` for local
comments and `remote-<id>` for imported ones. Comment `post` is `pending` for
comments written locally, `posting` while `publish` writes it (`attempt` names
the try, `sent` the exact body; `draft_id` a GitLab draft), and `posted` once
the forge has it; `remote` and the target's `review` and `records` are filled by
`sync`. Imported threads have no `anchor`; their position is in `remote`.

## Finding input

`pass end --findings` reads one JSON object or an array of them (or the object form
described above). Unknown fields are rejected.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `type` | `bug`, `question`, `requirement`, `nit`, `comment` | yes | Finding class |
| `severity` | `high`, `medium`, `low` | for `bug` | Bug severity |
| `scenario` | string | yes | Failure scenario; becomes the first comment |
| `path` | string | with any line field | File, relative to the repository root |
| `line` | integer | no | New-side line; omit with `path` for a file-level finding |
| `start_line` | integer | no | First line of a new-side multi-line range ending at `line` |
| `old_line` | integer | for `side: old` | Old-side line |
| `side` | `new`, `old` | no | Default `new`, or `old` when only `old_line` is set |
| `evidence` | string | no | Evidence for the finding |
| `tests_run` | array of strings | no | Tests or commands the reviewer ran |
| `post_reveal` | boolean | no | Found during the cycle 2 reveal |
| `killed` | boolean | no | `true` stores it as a killed finding on the pass |
| `reason` | string | when `killed` | Why it was killed; only for killed findings |

```json
[
  { "type": "bug", "severity": "high", "path": "src/net.rs", "line": 42,
    "scenario": "Retry loop drops the last response when ...",
    "evidence": "src/net.rs:42 breaks before storing.",
    "tests_run": ["cargo test net::retry"] },
  { "type": "bug", "severity": "low", "path": "src/net.rs", "old_line": 17,
    "side": "old", "scenario": "Removed guard was needed.",
    "killed": true, "reason": "The caller checks it." }
]
```

Line numbers refer to the pass's reviewed state: its head commit, or its tree
for uncommitted work. The output lists the new ids under `added` and
`killed`.

## Tasks

`agt task` takes `create`, `list`, `show` and `update`; `agt task --help` lists them. The id is
the one positional argument: `agt task create <id> --objective ...`, `agt task update <id> --revision N ...`.
`update` also records a completed check with `--check-name` and `--check-result` (both required
together), optionally `--check-code-revision` (default: the task's HEAD as this update records it),
`--check-inputs` and `--check-environment` (default: the platform and host name); the update and
the check are one revision bump.
Arguments are parsed like every other `agt` command, so a usage error (a missing value, an
unknown flag or command, a bad `--status`, a `--revision` below 1, no command at all) exits with
2 and clap's message; other errors exit with 1.

Task records are one pretty-printed JSON file each in `$XDG_STATE_HOME/agents/tasks/<id>.json`
(mode 0600, directory 0700), guarded by a `.<id>.flock` file (an advisory lock that goes when the process dies) and a `revision` that every
write must name (record `version` 1). The command that finds and searches sessions is
`agt sessions search`.

## Offline

The root `--offline` flag (or `AGT_OFFLINE=1`) keeps agt from running `git fetch`, `git ls-remote`
and `git push` and from calling `gh` or `glab`. A step that only refreshes is skipped; a command
whose answer comes from the remote refuses with `offline mode forbids <what>` (exit 1, or 2 for
`ci`) before it calls anything.

| Command | Under `--offline` |
| --- | --- |
| `pr` (all), `ci` (all) | Refuse. |
| `review sync`, `publish`, `status --sync`, `list --sync` | Refuse. Other review commands stay local: no automatic pull before a read, no automatic publish at `run end`. |
| `issue sync`, `issue push` | Refuse. Other issue commands stay local: no automatic pull or push. |
| `branch check` | `--checks pushed` refuses (the remote tip and the PR/MR are the answer, also by default). `--checks landable` weighs the local default branch only and fetches no remote. |
| `worktree create` | Weighs the local default branch only. A branch with no local branch and no `origin/<branch>` tracking ref refuses unless `--base` is given, since only origin can say whether it exists. Submodules are cloned over no network transport: one whose URL is a local path (including one the main checkout has already registered) is filled, any other is skipped with a `warnings` entry and the hooks still run. |
| `worktree remove` | Checks as `agt cleanup --only worktrees --offline` does: a tag that no commit elsewhere holds counts as unheld, so removal refuses naming it. |
| `worktree prepare`, `create --prepare` | Refuse when `mise install` or a lockfile install would run, since those download; `create --prepare` reports the refusal as a failed `prepare` step (exit 1) and still runs the create hooks, and a `--background` setup stays offline. `--hooks`, `--wait` and `--clone-deps` without other installs run. |
| `cleanup --only worktrees` | No `ls-remote` for tags and no forge evidence. |
| `doctor` | Skips the `gh`/`glab` login checks. |
| `task`, `sessions`, `ps`, `secrets`, `wait`, `worktree list/port/restore` | Local already. `wait` and the worktree hooks run your commands, which agt does not restrict. |

## Branch

`agt branch check` runs push and merge checks and prints one JSON report.

```sh
agt branch check [BRANCH] [--checks pushed,landable] [--remote NAME|HOST/REPO] [--base BRANCH]
```

`check` runs both checks by default. `--checks pushed` or `--checks landable` selects one.
Its `checks` object holds each check's `status` and `report`, or `error` when it fails
(for a single selected check too). Every selected check runs even when one fails. The exit status
prioritizes errors (1), then conflicts (5), then an unpushed branch (4), otherwise success (0).
`--remote` applies to the pushed check: a git remote name or a tracker as `host/repo`; without it the
tracker is chosen as `agt pr` does (the tracker of `origin`, else the only one; several trackers and
no `origin` is an error in the check's report that lists the `--remote` choices) and its git remote
is compared, `origin` when no tracker names one. `--base` applies to the landable check. `--offline` refuses the pushed check (see [Offline](#offline)).

`BRANCH` defaults to the current branch. The `pushed` check fetches `refs/heads/<branch>` from the remote,
pins the fetched commit and compares it with the local tip: `state` is `equal`, `ahead`, `behind`,
`diverged` or `missing` (not on the remote), with `ahead` and `behind` counts. `uncommitted`
counts the changed paths of the worktree that has the branch checked out (null when none does).
When an open PR/MR of the branch exists in the repository of that tracker (none when the repository has no usable remote), `pr` holds its `number`,
`url`, `head`, `head_matches` (its head equals the local tip) and `pipeline` (`sha` and `exists`: a
check run, status or pipeline exists for the PR/MR head). When only the check or pipeline query
fails, the PR/MR fields stay, `pipeline.exists` is null and a warning names the cause. When the
forge lookup fails (`gh` or `glab` missing or not logged in), `pr` is null, a warning on stderr names the cause, and the exit
code follows the git state alone. Status 0 only when `state` is `equal` and
any PR/MR head matches; 4 otherwise.

The `landable` check compares the branch with `--base`, which defaults to the newest default base among
every remote's live default branch and the local one (the choice `agt worktree create` makes); under `--offline` the local one alone.
It reports `behind` and `ahead` counts, `conflicts` (the paths `git merge-tree --write-tree`
cannot merge) and `already_in_target` (commits of the branch whose patch the target has, from
`git cherry`). Status 0 without conflicts, 5 with conflicts.

## CI

`agt ci` waits for PR/MR checks and triages GitLab failures.

```sh
agt ci wait TARGET... [--timeout SEC] [--interval SEC] [--max-interval SEC] [--remote NAME|HOST/REPO]
agt ci failures <mr> [--limit N] [--remote NAME|HOST/REPO]
```

`wait` takes one or more targets: GitHub `OWNER/REPO#PR[@SHA]`, GitLab `GROUP/PROJECT!IID[@SHA]`,
a PR or MR URL, `#PR` or `!IID` in the current repository (the tracker `agt pr` picks: `--remote <name|host/repo>`, else the
tracker of `origin`, else the only one; several trackers and no `origin` fail and list the `--remote`
choices; no tracker at all is an error), and a
GitLab pipeline id (current repository) or pipeline URL. Without `@SHA` the head read by the first
poll is pinned, and a later head is a change. `@SHA` on a PR or MR target is for right after a
push: the target stays pending (`waiting_for_pipeline`) until the head and, on GitLab, a pipeline
(or merged-results pipeline) is on that commit.

GitHub targets read the PR and its required checks in one GraphQL query; GitLab targets read the
MR, its head pipeline and the pipeline's jobs, on `GITLAB_HOST` or `glab config get host`.
Every call goes through the forge layer (`gh` or `glab`, replaced by `AGT_GH_BIN` and
`AGT_GLAB_BIN`). A timeout, a 5xx, a rate limit or a failed spawn is retried with the normal backoff and becomes a `query_error` after 5 in a row for a target; authentication and permission errors fail at once. Stdout gets one JSON event per line: `initial` and `change` per target when its
state first appears or changes, then one `final` event with every result and the failed jobs or
checks of each under `failed`. A result's `reason` is, for instance, `merged`,
`required_checks_passed`, `checks_pending`, `check_failed`, `check_cancelled`,
`manual_action_required`, `head_changed`, `closed` or `query_error`. Defaults: 3600 seconds, a
15 second interval doubling up to 120 seconds. A GitHub head whose status check rollup holds no checks at all is `checks_pending` during the first 120 seconds of the wait (right after a push the checks may not exist yet) and `no_required_checks` after; a rollup that has checks but none required is `no_required_checks` at once. Exit status: 0 all targets passed or merged, 1 a
target needs action (failed, canceled, skipped or manual, head changed, closed), 2 usage or
query error, 3 timeout, 130 or 143 after SIGINT or SIGTERM.

Compared with the removed `agt watch`, query-failure events no longer carry `command`, `exitCode` or `signal` (only `detail`), a deadline exits 3 instead of 124, usage errors exit 2 instead of 64, and a target needing action exits 1 instead of 2.

`failures <mr>` is GitLab only (`123`, `!123`, `group/project!123` or an MR URL; without a project the current repository
is used, or the tracker `--remote` names). It lists the failed and retried jobs of the MR's head pipeline, the failing
specs in their logs (Playwright, WebdriverIO, Karma, Jest) and how often each failed in the last
`--limit` (10) finished default-branch pipelines, as a verdict per spec: `new`, `pre-existing`, `flaky`,
`pending` or `unclear`.

## PR

`agt pr` is one GitHub/GitLab-neutral command for the PR/MR of the current repository; the forge
comes from the tracker of the `origin` remote (or the tracker `--remote <name|host/repo>`, or a PR/MR URL, names; see
[Issues](#issues)), and `gh` or `glab` (`AGT_GH_BIN`, `AGT_GLAB_BIN`) must be installed and logged in. `create`, `update`,
`merge` and `auto-merge` are refused when `push` was set to `off` for the tracker. Output is JSON.

```sh
agt pr [--remote R] view [<n>|<url>] [--branch B]
agt pr list [--mine] [--branch B] [--state open|merged|all]
agt pr create --title T [--body B] [--base BRANCH] [--draft]
agt pr update <n> [--title T] [--body B] [--add-label L] [--remove-label L] [--draft[=false]]
agt pr merge <n> --sha HEAD [--method merge|squash|rebase] [--auto]
agt pr auto-merge off|on <n> [--sha HEAD]
```

`view` without a number reads the PR/MR of `--branch` or the current branch (an open one first,
else the newest) and prints `number`, `url`, `title`, `state` (`open`, `merged`, `closed`),
`draft`, `head_branch`, `base_branch`, `head_sha`, `author`, `labels`, `auto_merge` (`on` or
`off`) with `auto_merge_method`, `queue` (the merge queue or train state, `null` when not
queued), `mergeable` (`mergeable`, `conflicting`, `blocked` or `unknown` while the forge computes it), the forge's own `merge_state` and `conflicts`. `list` prints an array of the same rows
(newest first, at most 100). `create` fails, printing the existing one, when the branch has an
open PR/MR. `merge` fails unless the PR/MR is open, not a draft and its head starts with `--sha`
(at least 7 characters), then sends the full head to the forge's own guard (GitHub `sha`, GitLab
`sha`); `--auto` merges when checks pass (GitHub needs `--method` unless auto-merge was on, and merges a
PR that can merge now at once, as gh does, unless its base branch requires a merge queue, where
auto-merge queues it; `auto-merge on` does the same; plain `merge` without `--auto` on such a PR also adds it to the queue this way (so it needs `--method`); GitLab
cannot rebase, and refuses `--method merge` on an MR with squash on, which GitLab would squash). `update --draft` converts to a draft and `--draft=false` marks it ready for review. Branch lookups (`view`, `create`) ignore PRs/MRs from forks. `list --mine` asks the forge for your own PRs/MRs, so older ones are found. `create` without `--base` needs a resolvable default branch, else pass `--base`. It never deletes the branch.

`auto-merge off` records the method it disables in `$XDG_STATE_HOME/agents/pr/auto-merge.json`
(keyed by host, repository and number) and reads the state back; `auto-merge on` re-enables it with
the recorded method at the current head (or `--sha`); when the checks had already passed and the forge merges at once, it reports `merged: true`. A PR/MR in a merge queue or train is refused.
Failures print `agt: error: ...` and exit 1.

## Sessions

`agt sessions` reports on Claude Code transcripts (`~/.claude*/projects`)
and Codex rollouts (`~/.codex/sessions`, `~/.codex/archived_sessions`). Reports are text
tables; `report --json` prints the cost section as indented JSON with `rows`, `unpriced` and `fast_calls`.

```sh
agt sessions report [--sections cost,time,tools,cache-gaps,errors,denials,friction,agents,hooks] [--by agent,model] [--gap MIN]
                    [--json] [--tsv [--width N]] [FILE...]
agt sessions search [--query TEXT] [--session ID] [--limit N] [--file FILE] [--rebuild]
```

`report` prints the first seven sections by default (`agents` and `hooks` only when selected), or the selected sections in the given order. One
selected section prints without its heading, so `--sections cost --json` is the JSON alone. It
discovers transcripts once and parses each transcript once for the selected reports.
`cost` is USD by `--by` group (`agent`, `model`, `project`, `session`, `file`, `harness`; one
session's cost is `--by session`, per-agent totals `--by agent`); `FILE...` prices those
transcript or stream-json files instead of the roots. `time` splits wall-clock time by the same
`--by` groups: the time before each model response (`model`; a Codex response ends at its last
item), before each tool result (`tools`, which includes permission prompts) and before each of your
messages or answers to `AskUserQuestion` and `ExitPlanMode` (`you`; a wait over 30 minutes is idle
and left out). Time before an injected message, such as a task notification, a reminder or a
compaction summary, counts nowhere. `active` is model plus tools. Its `Slowest tools` table times each tool
from its call to its result, keyed as in `tools` (`Bash:git`, `Read:skills/rules`, `mcp:slack`),
with `failed` (error results other than permission denials, which `denials` lists) and `est tok`
(input and result tokens, estimated at four characters per token). `Slowest runs` lists single tool
calls, longest first, with their start, whether they failed, their estimated tokens, agent, session
and command (60 characters). A shell command's key skips leading `cd DIR &&`, `export`, `VAR=value`,
`env`, `time`, `nohup`, `command` and `timeout N` steps that a command follows and a `for` or `while` loop's head, and
adds the subcommand of `git`, `cargo`, `npm`, `mise`, `gh` and similar tools: `Bash:git status`.
Subagent time also counts in the main agent's `Agent` tool time. `tools` counts tool calls and the share of
cost their results carry. `--gap` sets the idle minutes of the cache-gaps section. `errors` lists
tool errors and `denials` permission denials. `friction` lists interrupts, corrections, short
replies and repeated prompts; `--tsv` prints every user message instead, text cut to `--width`
(300). `--json` applies to the cost section and `--tsv` to the friction section.

`agents` reports sub-agent spawns for each harness, from the parent's `Agent` or `Task` call
(Claude Code) or `spawn_agent` call (Codex). Its first table groups spawns by agent type (Claude
`subagent_type`, Codex `agent_type`, else `general-purpose` or `default`) with the estimated tokens
of the prompts sent (total, average, largest) and of the results that came back to the parent
(a Claude spawn's tool result plus the `task-notification` of a background one; a Codex child's
`agent_message` or `subagent_notification`). Codex stores spawn prompts and replies encrypted, so
those are estimated from their size and Codex prompts are not compared line by line. `overlap`
counts the files that a parent and one of its sub-agents in the same session both read, with the
tokens the repeat reads cost (every read of the file but the largest) and the top files, as home-relative paths with
the reads on each side. It counts `Read` calls and shell reads of one absolute or `~/` file by
`cat`, `head`, `tail`, `sed` or `rg`/`grep`; a pipeline, a relative path or several files are not
counted. The last table lists prompt lines, trimmed and with whitespace collapsed, of at least 40
characters that occur in 3 or more spawn prompts, with their prompt count and total tokens, cut
to 120 characters. A Codex spawn that an older rollout makes from an `exec` script is counted from
the child rollouts that name the parent thread, with no prompt size. `hooks` lists Claude Code hook
outputs by event and command. Its rows count recorded outputs, not runs: a hook that prints
nothing leaves no record to count. The `outputs` column is how many were recorded; `to model` is how
many gave the model text, with the estimated tokens of that text (average, largest, total). Only
output that reaches the model counts: `hook_success` content of `SessionStart`,
`UserPromptSubmit`, `Setup` and `SubagentStart`, `hook_additional_context`,
`hook_blocking_error`, a blocking `PreToolUse` hook's `tool_result` text (`PreToolUse:<Tool> hook
error: [command]: ...`) and a `Stop` hook's feedback (`Stop hook feedback:` user text, listed under that name rather than a command); cancelled
hooks, non-blocking errors, system messages and the output of tool hooks do not. A hook output
copied into a resumed session counts once. The cadence column says how often the event fires: per
prompt, per tool call, per turn (`Stop`), per sub-agent, per start, per compact, else per event.
Codex rollouts record no hook output, so Codex gets one line saying so. Neither section prints
file contents or hook output.

`report` takes `--days N` (30) or `--since YYYY-MM-DD`, `--project TEXT`, `--scratch`
(scratch and probe projects are skipped otherwise), `--root DIR` (repeatable), `--limit N` (25 rows per table) and
`--harness all|claude|codex`.
A transcript last changed before the start date is not read. A root named with `--root` can hold either harness; a file whose
first line is a Codex `session_meta` is read as Codex. Prices come from
`$XDG_CONFIG_HOME/agt/model-prices.json`, or
the file `AGT_PRICES` names; models it does not list are reported as unpriced.

The prices file is a JSON object with a `models` object that maps a model name to its rates in
USD per million tokens: `input`, `write_5m` and `write_1h` (cache writes with a 5-minute or
1-hour lifetime), `read` (cache reads) and `output`. A model name matches a transcript's model
when it equals it or is a prefix of it that ends before a `-`; the longest match wins. A model
may add `fast_multiplier`, which scales every rate for responses billed at `"speed": "fast"`,
and `over`, an object with a `prompt_tokens` threshold and its own rates that replace the
model's when the prompt (input plus cache tokens) exceeds it:

```json
{"models": {"claude-sonnet-4": {"input": 3, "write_5m": 3.75, "write_1h": 6, "read": 0.3,
  "output": 15, "over": {"prompt_tokens": 200000, "input": 6, "write_5m": 7.5, "write_1h": 12,
  "read": 0.6, "output": 22.5}}}}
```

Codex rollouts are read as Claude Code records: each billed `token_count` is one call, with
`input_tokens` net of cached tokens. A forked rollout's replay of its parent, everything before the
fork's first `thread_settings_applied` event, is left to the parent. Shell calls (`exec`, `exec_command`, `shell`)
count as `Bash`. An output starting `Script failed` or `aborted by user`, holding ` failed: `, with
a `Process exited with code N` or `Exit code: N` header line other than 0, or an `exec` script
result whose `exit_code` is not 0 is an error, read as Claude Code's `Exit code N`. Injected `AGENTS.md` and
`<environment_context>` text is not a user message, and `codex exec` prompts are left out of
`friction` as `claude -p` ones are. In `cache-gaps` a Codex cache miss shows as uncached input,
priced at the input rate. A session is labelled by the last 8 characters of its thread id (UUIDv7
ids start with a timestamp); a subagent rollout is grouped under its role and its parent's
session. Without `--root`, reports and `search` read the same directories: Codex's `sessions` and
`archived_sessions`, and each Claude Code profile's `projects`, `archive` and `archives` (links
followed). Reports therefore also count transcripts kept in the `archive` directories, whose
harness is read off each file's first line; `search` additionally reads the prompt history files.

`search` finds sessions and what was typed in them, with a private index
of the human turns in Codex and Claude transcripts, prompt history files and
`--file` in `agents/tasks/session-index/index.json` (version 4). Every run first updates the
index incrementally; `--rebuild`, a rewritten file or another index version scans again. Without
`--query` and `--session` it prints the coverage as JSON (`updatedAt`, the counts of files,
turns and unreadable sources). With `--session ID` and no `--query` it prints the session's
sources and turn count and fails when the session is not found. With `--query` it prints up to
`--limit` (1 to 100, 20) user turns that contain the text, case-insensitively, newest first, with
the turns around each, and the coverage as one JSON line on stderr. `--session` limits the
search to that session, whose transcripts are found wherever they are, and turns a fork replays
from its parent (marked `inherited`) are left out of a search without it.
There is no limit on files or bytes: a run reads every transcript, and the coverage has no
scan or byte limit fields.

## Ps

`agt ps [--json] [--watch ...]` lists the agents running now, one row each, busy first and then longest in
their status. A Claude Code session is a record in `~/.claude*/sessions/<pid>.json` (and
`$CLAUDE_CONFIG_DIR/sessions`) whose pid runs with a matching `procStart`; a stale record or a reused
pid is not listed. A row has `pid`, `name`, `kind`, `status` as Claude Code wrote it (`busy`, `idle`,
`shell` or `waiting`), `status_secs` (how long in that status), `waiting_for` (the text Claude Code
records with `waiting`, such as `dialog open`), `cwd`, `repository` (the main checkout), `branch`,
`worktree` (the linked worktree holding `cwd`, else `null`), `session`, and from the session's
transcripts `model` and `context_tokens` of the last call of the main agent and `usd` spent so far,
subagents included. `usd` is `null` when the price file lists none of the session's models or cannot be read (one warning on stderr, the agents are still listed), and
`unpriced_calls` counts calls it does not list. The table shows `unpriced` then, and `12.34+` for a
partial sum. `--json` prints `{"agents": [...]}`.

Running `codex` processes are listed too, with `harness: "codex"` and no name or kind. Codex
writes no live record, so the session is the newest rollout in `~/.codex/sessions` whose
`session_meta` `cwd` equals the process's working directory and that changed after the process
started. A subagent's rollout does not count. When no rollout matches, several `codex` processes
share the directory, or the platform cannot read the process's directory, `session` is `null` and
`note` says why. A matched session is `busy` when the last `task_started` event in the end of its
rollout has no later `task_complete` or `turn_aborted`, else `idle`; `status_secs` counts from
that event's timestamp (only the file's last 256 KiB is read).

`agt ps --watch [--interval SECS]` polls every `--interval` seconds (default 5) until
interrupted and prints one JSON line per event for Claude Code sessions: `blocked` (the session
has been `waiting` for 60 seconds; once per waiting stretch, with `waiting_for`), `cleared` (a
blocked session left `waiting`, with `reason` `answered`, or a new waiting stretch replaced the
blocked one, with `reason` `new_prompt`) and `ended` (its process is gone). Each line holds the
row's fields plus `event` and `at`. A session whose process still runs but is missing from one
poll keeps its state and is not reported `ended`.

## Worktrees

`agt worktree` creates git worktrees on the newest default base, installs their dependencies,
gives each a dev-server port, lists them, removes one after backing it up, sweeps away landed work, and restores a backup.
Output is JSON.

```sh
agt worktree create BRANCH [--base C] [--path P] [--prepare] [--no-hooks] [--background]
agt worktree prepare [PATH] [--clone-deps]  # install dependencies from the lockfiles
agt worktree prepare --hooks [PATH]  # rerun only the create hooks, no installs (after a failed one)
agt worktree prepare --wait [PATH] [--timeout SECS]  # wait for the background setup `create --background` started
agt worktree port [PATH] [--env]            # a stable dev-server port
agt worktree list [--all-repos]            # this repository's linked worktrees, or every repository under the roots
agt worktree remove PATH [--yes] [--no-hooks]  # without --yes: print the plan, change nothing
agt cleanup --only worktrees [--all-repos] [--yes] [--offline] [--dirty] [--recent] [--worktree NAME-OR-PATH]...  # remove landed worktrees, delete landed branches
agt worktree trust [PATH] [--remove]        # trust (or forget) the repository's [worktree.hooks]
agt worktree trust --list                   # trusted files and whether each still matches
agt worktree restore ID [--path P]          # recreate the worktree and its uncommitted files
agt worktree backups [ID]                   # list backups, or only the one with this id
agt worktree backups rm ID [--yes]          # delete the backup's ref and directory
```

Hooks run shell commands around a worktree's life. Set them globally in `$XDG_CONFIG_HOME/agt/config.toml`; the main checkout's `.config/agt.toml` overrides each key (`create`, `remove`) as a whole:

```toml
[worktree.hooks]
create = ["npm run build"]   # after `create` makes a new worktree (not a reused one)
remove = ["docker compose down"]  # before `remove --yes` or `cleanup --yes` backs the worktree up
```

Each entry runs as `sh -c <entry>` in the worktree root, one after another, stopping at the first failure; stdin is empty and the output goes to a log under `$XDG_CACHE_HOME/agents/output/`. The environment has `AGT_HOOK` (`create` or `remove`), `AGT_WORKTREE`, `AGT_MAIN_CHECKOUT`, `AGT_BRANCH` (empty when detached) and `PORT`: a `create` hook records the worktree's port first (as `agt worktree port` does), a `remove` hook gets the port only when one is already recorded. `create` reports them under `hooks` (`create`: the steps with `command`, `exit`, `ok`, `duration_ms`, `log`; `ok`) and exits 1 when one fails; `--no-hooks` skips them. A failing `remove` hook is an error: nothing is backed up or removed. The removal checks run again after the `remove` hooks, so work a hook leaves behind (a nested repository, say) still blocks the removal. The `remove` plan (no `--yes`) lists the commands under `hooks`; `remove --no-hooks` skips them and `agt cleanup --only worktrees` always runs them.

Repository hooks are gated like `mise trust`: the global hooks always run, but `[worktree.hooks]` in a repository's `.config/agt.toml` run only while that file is trusted. `agt worktree trust [PATH]` (PATH is any checkout, default the current directory) records the main checkout's file with the SHA-256 of its exact bytes in `$XDG_STATE_HOME/agents/worktrees/trust.json` and prints `file`, `sha256`, and the `create` and `remove` commands it trusts; it fails when the file is missing or has no `[worktree.hooks]`. Trust is keyed on the canonical main checkout joined with the literal `.config/agt.toml`; when `.config` or `.config/agt.toml` is a symlink its hooks are untrusted (reported with a `reason`) and `trust` refuses. Any edit untrusts the file. `--remove` forgets it; `--list` prints every trusted file with its hash and `current` (`true`, `false` or `missing`). Trust covers only hooks; other repository keys apply untrusted. An untrusted file's hooks are skipped (the global hooks alone apply, per key) without failing, and the result reports `hooks.untrusted` with `file`, the skipped `create` and `remove` commands, and `next` (a note to ask the user to review the commands, with the quoted trust command), or `reason` when the file cannot be trusted: on `create` (also the background child's report), on `remove` (the plan's `hooks` is `{"remove": [commands], "untrusted": {...}}`, `untrusted` only when present) and on `agt cleanup --only worktrees` plan rows (`hooks.error` when the config or trust file cannot be read; the plan continues).

`agt worktree prepare --hooks [PATH]` runs only the create hooks (no install steps) in the foreground (a failed or timed-out create hook cannot be rerun by `create`, which reuses the worktree); the report is `{path, hooks, ok}`, and it exits 1 when a step failed. An inherited `PORT` is removed from every step; a `remove` hook sees `PORT` only from a recorded port. The hook config is validated before `create` makes the worktree. A second `create --background` while a setup is running starts no child and reports the running one with `background.already_running: true`. While it runs, `prepare`, `prepare --hooks` and `create --prepare` in that worktree fail and point to `prepare --wait`, so no install or hook runs twice at once.

`create --background` returns at once: `--prepare` and the create hooks run in a detached process, and the output has `background` with its `pid`, the `status` file and the `log` (`background.started: false` when there is nothing to run). The status file `agt-setup.json` is in the worktree's own git directory, so it goes with the worktree. `agt worktree prepare --wait [PATH] [--timeout SECS]` (default 270; `--max` is an accepted alias) waits until the setup is done, prints the stored report and exits 1 when it failed, or exits 3 when `--timeout` passes first. It is an error when the worktree has no background setup or the setup process died without finishing. `--wait` cannot be combined with `--clone-deps`.

`list` prints per worktree its `path`, `branch` (or `detached`), `head`, `locked`, counts of `modified` tracked, `untracked` files and `ignored` paths that are not disposable (removal deletes them; no backup keeps them), `ahead` and `behind` the default branch, and `merged` (HEAD is an ancestor of it) (a worktree on a branch with no commit yet has `unborn: true` and `null` for those three), and `agents`: the live Claude Code sessions whose working directory is inside it, each with `pid`, `name`, `status` and `session` (the sessions `agt ps` lists). The default branch is the one `refs/remotes/origin/HEAD` names, else `main`, else `master`. `--all-repos` scans the roots in `$XDG_CONFIG_HOME/agt/config.toml` (a repository it cannot read becomes an entry of `warnings` and the rest are still listed, as `agt cleanup --only worktrees --all-repos` does); globs and `~` are allowed, and the default is the layout `agt cleanup` assumes:

```toml
[worktree]
roots = ["~/Projects/*/*", "~/projects/*/*"]
```

`remove` refuses the main checkout, a locked worktree, a path that is not a linked worktree, a worktree that holds the current directory, a worktree where a live Claude Code session other than the calling one runs (`~/.claude*/sessions/<pid>.json` with a `cwd` inside it, a running pid and a matching `procStart`; a record whose pid is an ancestor of agt or the `CLAUDE_PID` in agt's environment is the calling session and is ignored), a worktree where any running process other than agt and its ancestors has its current directory (native process APIs on macOS, `/proc/*/cwd` on Linux; unreadable individual processes are skipped, but enumeration failures refuse removal; the refusal names the pid and command; the calling session's own processes, meaning every descendant of an ancestor of agt that has a live session record or of `CLAUDE_PID`, never block, and are listed under `warnings` as pid and command so you can stop task servers; `cleanup` lists them in the repository's `warnings`), a worktree with a directory agt cannot read, write or search (git stops at the first directory it cannot empty, after deleting part of the worktree and its registration; the error names the directory and the `chmod -R u+rwX` that clears it), and a worktree whose removal would delete work that nothing else holds. The rule is that `remove` deletes only what the backup holds or another clone holds; anything else refuses, naming the repository and the reason. `git worktree remove --force` deletes every submodule clone under the worktree's git directory (`.git/worktrees/<id>/modules/`, initialized or not, whatever `git submodule status` says) and every `.git` under the worktree, so each of those repositories must hold nothing unique: the stash refuses; every other ref except the remote-tracking ones (branches, tags, notes, anything else) and its HEAD must lead only to commits that a remote-tracking branch of that repository or the main checkout's clone of the same submodule has (decided by commits, in one `rev-list` per repository, however many tags there are), and a tag whose commits nothing else has is still held when `git ls-remote --tags` (10 s limit, or `AGT_LS_REMOTE_TIMEOUT` seconds; no prompt of any kind: `GIT_TERMINAL_PROMPT=0`, askpass helpers disabled, ssh with `BatchMode=yes` and `ConnectTimeout=5` added to your own ssh command, chosen as git does: `GIT_SSH_COMMAND`, then the repository's `core.sshCommand` (read even when a leftover clone's recorded work tree is gone), then `GIT_SSH` (a program path, quoted), git in its own process group that is killed with ssh at the deadline and when agt gets SIGINT, SIGTERM or SIGHUP (one agt was started to ignore, as under `nohup`, stays ignored), as are the `gh` and `glab` calls of an auto-sync; the `agt cleanup` report and `agt cleanup --only worktrees --offline` never ask, so such a worktree is simply not listed) shows the remote advertising that tag at the same object; offline or on failure it refuses, naming the tag, it must have no linked worktrees of its own, no uncommitted or untracked files (whatever `status.showUntrackedFiles` says; only an untracked `.DS_Store`, `Thumbs.db` or `desktop.ini` is let through, as at the top level), and no rebase, merge, cherry-pick or bisect in progress. A submodule that is not at the commit the worktree records refuses too. The worktree itself refuses with a `refs/worktree/*` or `refs/bisect/*` ref, with a rebase, merge, cherry-pick, revert or bisect in progress, with a path that has both a staged and an unstaged change (the backup keeps only the working file), and with files in a submodule directory that is not initialized (git lists none of them; the directories are the gitlinks of the index, in the worktree and in each submodule, whatever `.gitmodules` says; only `.DS_Store`, `Thumbs.db` and `desktop.ini` there never refuse, since nothing there is ignored and a `build/` may hold real work). A nested git repository that no `.gitmodules` registers (a directory with its own `.git` at any depth, also inside submodules and inside ignored or disposable directories) always refuses, naming each path: a backup would keep only a gitlink, so move or push it first. Commits that only a reflog holds are not protected. A leftover clone under the worktree's git directory is checked in its own work tree only while that path still belongs to it. The backup also keeps the worktree's own `info/sparse-checkout` and `config.worktree`, and `restore` puts them back, removes either one the backup does not have, and, when the config (the saved one, or the shared one where an old-style sparse checkout is set) turns the sparse checkout on, reapplies it (`sparse_checkout: true` in its output; an old backup's copied ignored files come back after the reapply, so one in a directory outside the cone stays; a deletion of a file outside the cone comes back staged, since the reapply would otherwise hide it and the next commit would keep the file, while one inside the cone stays unstaged; a disabled one, or a backup with no saved files, restores as a full checkout, silently, even from a sparse main checkout); if that fails it restores a full checkout and warns, naming the saved files. `agt cleanup`'s manual report lists none of these worktrees either. With `--yes` it first snapshots, then runs `git worktree remove --force` (which removes the worktree's own admin directory; other worktrees' metadata is never pruned). A failed backup leaves no ref and no backup directory. It never deletes the branch.

`agt cleanup --only worktrees` finds work that already landed in the default branch and removes it: in the current repository, or with `--all-repos` in every repository under the roots. Its report is the `worktrees` field of the cleanup report. Without `--yes` it prints the plan (`plan: true`; per repository `remove`, `delete`, `kept` and `warnings`, each candidate with its `verdict`, `evidence` and the exact `action`) and changes nothing except fetching PR heads into the object store. With `--yes` the lists are `removed` (`path`, backup `id` and `ref`), `deleted` (`branch`, `tip`, `verdict`, `evidence`, `restore`: the `git branch <name> <tip>` that brings it back), `kept` (`branch` or `path`, `verdict`, `reason`) (a `closed` row also carries the PR/MR's `head` sha, `title` and `closed_at`, read from the forge lookup) and `warnings`. `--worktree <branch-or-path>` (repeatable, with or without `--yes`) limits the pass to the named rows: a name is a local branch or a worktree path, a worktree row and its branch go together, and unnamed rows are never judged or touched. Each named row is judged exactly as the plan judges it, so it is acted on only while it still qualifies; one that does not is listed under the repository's `skipped` (the `kept` row, with its reason, in place of `kept`), and a name that matches no row (protected, unknown or gone) is listed in `worktrees.skipped` with a reason. It never touches the default branch, `main`, `master` or `develop` by name, the main checkout's branch, the current branch or the worktree holding the current directory, and it never deletes a remote branch, pushes, closes a PR, resets or discards files.

A branch gets a verdict, strongest evidence first. `merged`: its tip is an ancestor of the local default branch; it is deleted with `git branch -d`, so git can still refuse. Otherwise, unless `--offline` or the repository has no tracker that serves pull requests (every git remote on a known GitHub or GitLab host is one under the default `[sync] remotes = "all"`; `pull = "off"` drops one), the PRs/MRs for that source branch are listed on every such tracker (GitHub GraphQL, GitLab REST; every state, every page) through `gh`/`glab`. A PR merged on any one tracker lands the branch, even when another tracker has a PR open for it; when none landed it, an open PR on any tracker gives `open` (on one tracker an open PR wins over that tracker's older merged PRs; kept); a PR closed without merging gives `closed` (kept, with its number and URL). A merged PR counts only when it merged into the default branch, its source branch is the local branch, it is from the same repository (not a fork), its head commit is local (else `refs/pull/<n>/head` or `refs/merge-requests/<iid>/head` is fetched from the tracker's git remote and must equal the head SHA) and the local tip is the PR head or an ancestor of it: `squash-merged`, deleted with `git branch -D` after re-reading the tip, and the tip is reported. A tip past or beside the merged head is `diverged` (kept, `diverged from merged PR #<n>`). A failed forge call, a head SHA mismatch or anything unexpected is `unknown` (kept), and no evidence is `unmerged` (kept). Each forge host is found as `agt review` finds it, including its `hosts.json`.

A linked worktree whose branch is `merged` or `squash-merged`, or whose detached HEAD is in the default branch, is removed first, exactly as `remove --yes` does (snapshot backup, then `git worktree remove --force` once), and then its branch is deleted. It is kept when it is locked, missing, holds a submodule or a nested git repository `remove` would refuse, has a live Claude Code session in it (as `remove`), or any process with its current directory inside it (native process APIs on macOS, `/proc/*/cwd` on Linux; unreadable individual processes are skipped, but a failed process enumeration keeps the worktree and reports the reason). It is kept as `recent` when anything in it changed in the last 3 days (its HEAD commit time, the mtime of its git dir's `HEAD`, `index` or `logs/HEAD`, or of any tracked or untracked non-ignored file), so a worktree just made by `create` survives, unless `--recent` is passed. It is also kept as `dirty` when it has modified tracked files or untracked files that are not disposable, unless `--dirty` is passed; the snapshot preserves them either way. It is kept as `dirty` too when its `ignored_lost` list is not empty, unless `--dirty` is passed: no backup keeps those files. It is checked again after the remove hooks run, so a file a hook leaves keeps the worktree too. A kept worktree keeps its branch. Each `remove` row also lists `ignored_lost`, as `remove` does. With `--yes`, stderr announces each step as it starts (`agt: removing <path> (<verdict>)`, `agt: deleting branch <branch>`); stdout keeps the JSON report.

The snapshot is a commit of the whole working tree (tracked changes plus untracked, non-ignored files) with the worktree's HEAD as parent, built in a temporary index (where a tracked file marked skip-worktree or assume-unchanged that exists on disk is taken as it is, so its edits are kept and counted; a file missing from a sparse checkout keeps its index entry, and files outside the sparse patterns are kept too; git older than 2.35 cannot add those, so there `remove` refuses such a worktree) and pinned at `refs/agt/backups/<id>`, so the commits outlive the branch. Regular file permissions are saved separately from the Git snapshot and restored independently of the current umask. Older backups without this metadata retain Git checkout permissions. Ignored files are never backed up: removal deletes them. The plan and the result list under `ignored_lost` the ignored paths, in the worktree and its submodules, that no build or install recreates (`.env*`, credentials, notes), naming a directory only when nothing in it is disposable (a directory holding a `CACHEDIR.TAG` with its standard signature counts as disposable; paths in a submodule are judged relative to it); disposable paths are left out of that list: `node_modules/`, `.venv/`, `vendor/`, `target/`, `dist/`, `build/`, `coverage/`, regenerable caches (`__pycache__/`, `.pytest_cache/`, `.mypy_cache/`, `.ruff_cache/`, `.next/`, `.gradle/`, `.turbo/`, `.parcel-cache/`, `.tox/`, `.hypothesis/`, `.svelte-kit/`, `.terraform/`, `htmlcov/`, `*.egg-info/`), `*.log`, `*.tsbuildinfo`, `.coverage*`, `.eslintcache`, `.DS_Store`, `Thumbs.db`, `desktop.ini`, editor swap files and `.cache/agents/scratch/`.

A backup lives in `$XDG_STATE_HOME/agents/backups/<repo>/<YYYYMMDD-HHMM>-worktree-<name>/` as `backup.json` (id, repository, worktree path, branch, head, snapshot commit, ref, created, `skipped` (the ignored paths left out), `copied` (empty; only backups from agt 0.20.0 and older copied ignored files, under `files/`), regular file modes). The id is the directory name. The `<name>` is the worktree's directory name with every run of characters a git ref refuses (space, `~`, `^`, `:`, `..`, a `.lock` ending) replaced by `-`, and a `-2`, `-3`, ... suffix keeps the id unique across the backups of every repository and across the backup refs. `remove` refuses a worktree on a branch with no commit yet (`git worktree add --orphan`): a backup needs a commit to restore on. Backups are never pruned automatically.

`restore` recreates the worktree at its original path (or `--path`) and refuses an existing path. It checks out the original branch when it exists and is not checked out elsewhere and detaches at the snapshot's parent when it is checked out elsewhere or its tip moved off the snapshot's parent (the warning names the tip it has now, so newer commits never show up as reverted changes), and recreates the branch at the snapshot's parent when it is gone. The working tree then matches the snapshot, with modified, deleted and untracked files left unstaged (a deletion outside a sparse checkout's patterns comes back staged, since git cannot hold it unstaged there), and an old backup's copied ignored files return. At the original path `claude --continue` finds the old session.

`agt worktree create <branch> [--base COMMIT-ISH] [--path P] [--prepare]` puts a branch in
a worktree at `<main checkout>/.worktrees/<branch>` (slashes make subdirectories) or at `--path`,
and prints JSON. The main checkout is the first entry of `git worktree list`; when it is bare,
`--path` is required. A branch that a worktree already has checked out returns that worktree
(`reused: true`); another existing branch gets a worktree without moving. A branch that exists only as `origin/<branch>` (no local branch, no `--base`) gets a local branch tracking it, checked out in the worktree (`tracking` names the upstream). When neither a local branch nor `origin/<branch>` exists, `create` first asks `origin` (`git ls-remote --exit-code`, whose exit code 2 means no such branch, so git's message language does not matter) and then fetches that one branch (`git fetch origin +refs/heads/<branch>:refs/remotes/origin/<branch>`), so a branch pushed after the last fetch is found; if `origin` has no such branch the new branch starts as below, and any other fetch failure (offline, authentication) is an error naming the branch that suggests `--base`, instead of a silent new branch. A new branch starts at
`--base`, or at the newest default base: for each remote, its live default
(`git ls-remote --symref <remote> HEAD`) is fetched and its commit recorded at once, and the local
default branch (`origin/HEAD`'s, else `main`, `master` or `init.defaultBranch`) joins them. The candidate that
contains all the others wins; when they diverge, origin's fetched default is used and
`base.diverged` is true. `base.candidates` lists every candidate with its commit. Choosing
moves no remote-tracking branch, and a remote that cannot be queried or fetched is an
error naming it. A `.gitmodules` file triggers `git submodule update --init --recursive`.

After a new branch starts at the newest default base (not with `--base`), `create` fast-forwards
the local default branch to the fetched default of its upstream (`branch.<name>.remote` and
`.merge`, else `origin`'s branch of the same name), and `local_default` reports it: `branch`,
`from`, `upstream`, `to`, `behind` or `ahead`, and `status`. The upstream's remote-tracking
branch (`refs/remotes/<remote>/<branch>`), when it exists and is an ancestor of the fetched
commit, moves up to it as a fetch would, so `git status` does not count the new commits as
unpushed; this is best effort, and while another git process holds that ref's lock it stays.
`updated` means the local default moved: by
`git merge --ff-only --no-overwrite-ignore --no-autostash` in the worktree that has it checked
out (`path`), which keeps local changes the merge does not touch and never stashes, else by
`git fetch . <commit>:refs/heads/<name>`, which git refuses for a branch being rebased or
bisected. `current` means it already matched, and `ahead` that it holds commits its upstream
lacks; neither moves it. `skipped` leaves it where it was, with the `reason`: its upstream is
not a fetched remote default, it diverged, the worktree that has it checked out no longer
exists (`git worktree prune` drops that entry), a submodule pointer changes in a checked-out
branch (a merge would leave the submodule at its old commit), or git refused, as it does when the merge
would overwrite a local change or an ignored file. A refusal also gives `retry`, the command to
run once that is resolved. Every `skipped` adds `local <name> not updated: <reason>` to
`warnings`, which is otherwise empty; it never fails `create`. Without remotes or a local default
branch there is no `local_default`.

`agt worktree prepare [PATH] [--clone-deps]` installs dependencies in a worktree (default: the
current one). It runs `mise install` first when `mise.toml`, `.mise.toml` or `.tool-versions`
exists and `mise` is on PATH, then one install per lockfile in the root: `package-lock.json`
`npm ci`; `pnpm-lock.yaml` `pnpm install --frozen-lockfile`; `yarn.lock` `yarn install --immutable`
(Yarn 2+: a `.yarnrc.yml` or a `packageManager` field of `yarn@2` or later) else
`yarn install --frozen-lockfile`; `bun.lock`/`bun.lockb` `bun install --frozen-lockfile`;
`uv.lock` `uv sync --frozen`; `poetry.lock` `poetry install`; `Cargo.lock` `cargo fetch --locked`;
`go.sum` `go mod download`. Each step reports its command, `exit`, `duration_ms` and `log`
(under `$XDG_CACHE_HOME/agents/output/`); the exit status is 1 when a step fails. Dependency
directories are never symlinked. When a node install (`npm`, `pnpm`, `yarn` or `bun`) fails, its lockfile is byte-identical to the
main checkout's and the main checkout has a `node_modules`, the step and the report carry
`clone_available: true`; no other install offers a clone, because a clone copies only
`node_modules`. `--clone-deps` copies every `node_modules` of the main checkout (nested ones too)
through native recursive copying instead of the node installs. It preserves symlinks and permissions,
uses copy-on-write cloning when supported, and falls back to copying file contents. It
then runs `npm run prepare --if-present` when there is a `package-lock.json`; the other installs
(`uv`, `poetry`, `cargo`, `go`) still run and are reported, so `ok` is false when any step fails.
It reports `deps_from: "clone"`.

`agt worktree port [PATH] [--env]` gives a worktree a stable TCP port for its dev server. The
first call picks a free port between 10000 and 32767 (below the OS ephemeral ranges) and records
it in `$XDG_STATE_HOME/agents/worktrees/ports.json` under a lock, keyed by the worktree's absolute
path; later calls return it. A port recorded for another worktree is never handed out, and
entries whose path no longer exists are dropped on every write. `--env` prints `PORT=<n>`.

## Secrets

`agt secrets [PATH...] [--staged] [--json]` looks for leaked credentials without ever printing
them. It reads the files `git ls-files -co --exclude-standard` lists (tracked and unignored), or
only the staged blobs with `--staged`, limited to `PATH`s when given, and skips binary files.
Output is `path:line rule` per hit, or with `--json` rows of `{path, line, rule}`; the matched
text is never printed, not even in errors. Rules (one table in `src/secrets/rules.rs`):
`github-token`, `gitlab-token`, `slack-token`, `aws-key-id`, `private-key`, `jwt` and
`generic-assignment` (`key`, `token`, `secret` or `password` set to a quoted value of 8 or more characters, or a bare 16+ character token ending the line as in `.env`, `export` and YAML lines; neither
may be a placeholder, `$VAR`, `${VAR}`, an env var name or `changeme`). A line
containing `agt-secrets: allow` is skipped. Exit status: 0 clean, 1 on any match, 2 on an error (reported as `agt: error: <message>`), including a `PATH` that does not exist (with `--staged`, one that is not on disk, in the index or in the staged diff).
Files that disappear after listing are skipped. Other filesystem inspection or read errors
fail the scan with exit status 2 and identify the path.

## Check

`agt check run --name NAME [--force] -- COMMAND [ARGS...]` never reruns a test or lint command on
code it already passed on. With a clean work tree (untracked files that are not git-ignored count
as dirty whatever `status.showUntrackedFiles` says, and so do submodule changes) the key is the `HEAD` tree (`git rev-parse HEAD^{tree}`), `NAME`, the directory below
the repository root and the exact argv, so an empty commit or a rebase that changes no content
hits and a rebase onto a moved base misses. A recorded pass prints `{"cached": true, "name", "key",
"tree", "command", "ran_at", "log"}` and exits 0 without running anything. Otherwise the command
runs with inherited stdin, its stdout and stderr go to agt's stderr and to
`$XDG_CACHE_HOME/agents/output/check-NAME-*.log` (pruned by `agt cleanup`'s `output` rule), and
stdout holds only the JSON summary, printed last: `cached` (false), `name`, `key`, `dirty`,
`recorded`, `reason`, `warnings`, `command`, `exit`, `ran_at`, `duration_ms`, `log`. The exit status is the command's
(128 plus the signal when it was killed). Only exit 0 is recorded, as one document per key in the
main checkout's `.agt/checks/`, shared by linked worktrees; failures are never cached. The pass
is recorded only if the work tree is still clean and `HEAD` still has the same tree after the
run; otherwise `recorded` is false and `reason` says why (else `reason` is null). A dirty
work tree always runs and records nothing (`key` is null). `--force` runs even on a hit and
records the new pass. The store opens only when there is a key; where it cannot open (a bare
repository with worktrees has no main checkout) the command runs uncached and `warnings` says
so. Once the command exits agt waits at most half a second for its remaining output, so a
background process that keeps the pipes open cannot hang it; after that agt stops reading, and a descendant that keeps writing may get EPIPE or SIGPIPE.
`agt cleanup --only checks` removes entries, and their lock files, that ran 7 days or more ago,
the retention of the output logs. A hit whose log file no longer exists (for example after `agt cleanup --only files`) is a miss and the command reruns. A cache entry that cannot be read (also with `--force`) is deleted, so the run records a fresh pass in its place; one written by a newer agt (a higher `version`) is kept and the run is not recorded. Neither an entry that cannot be read or deleted nor a store that cannot be written fails the command: the entry counts as a miss or `recorded` is false, and `warnings` says why.

## Wait

`agt wait [--timeout S] [--interval S] -- CHECK [ARGS...]` reruns `CHECK`, output discarded, every
`--interval` (5) seconds until it succeeds, for at most `--timeout` (270) seconds (`--every` and `--max` are accepted aliases). Agents run it in the background so they wake within the prompt cache's five
minutes to look at a long job. Exit status: 0 the check passed (`agt wait: done after Ns`), 3
`--timeout` passed first, 2 usage.
The deadline includes checks and pauses between checks. An overdue check and its children
are terminated; a check finishing after the deadline cannot report success.

## Doctor

`agt doctor [--json]` reports whether this host and repository are set up for agt, one row per
check (name, `ok`/`warn`/`fail`, detail, fix hint), as a table or, with `--json`,
`{"status", "checks": [...]}`. It is read-only: it writes nothing, reads no credentials (a forge
login is only the exit status and account name of `gh auth status` / `glab auth status`) and
makes no forge write. Checks: `git` (>= 2.38), `gh`, `glab`, `mise` and their versions; the
login for the origin forge (a fail when missing) and for the other installed forge CLIs (a
warning); the running agt version against the one `mise ls --current` pins for
`github:brandonocasey/agt` (older: warn, `mise install`; when `mise ls` fails, the detail is its first stderr line containing `ERROR`, else its last line and the fix is
`mise trust` or "fix the mise error above"); the global git ignore covering
`.worktrees/`, `/.agt/` and `/.cache/agents/scratch/`; the state and config directories and both config
files parsing; and, inside a repository, the origin, the default branch and stray agt lock
files. The scratch-ignore and mise-pin checks are agt conventions, not requirements of git or
mise: a missing `/.cache/agents/scratch/` ignore is a warning, and the version is compared only
when mise pins `github:brandonocasey/agt` (no pin is ok). All subprocesses share a
30-second budget. Exit status: 0 all ok, 1 any fail, 2 only warnings.

## Issues

`agt issue` manages local issues without configuration, credentials or a remote.
`create` produces a local issue. Notes and local links stay private.
GitHub/GitLab mirrors and remote publication are optional.

Global configuration lives in `$XDG_CONFIG_HOME/agt/config.toml` (`~/.config` when unset).
The main checkout's `.config/agt.toml` overrides fields, except that it cannot lower a `push` mode;
arrays replace inherited arrays. Linked worktrees read the main checkout's configuration.
No configuration file is required. Unknown keys and invalid values name their file and key.

```toml
[sync]                      # defaults for issues and pull requests
pull = "manual"             # manual | auto (pulling is always allowed on a command)
push = "off"                # off | manual | auto
max_age_secs = 300          # minimum age before an automatic pull
remotes = "all"             # all | origin | none: which git remotes become trackers

[issues]                    # overrides [sync] for issues
push = "manual"

[pull_requests]             # overrides [sync] for reviews and `agt pr`
pull = "auto"

[[tracker]]                 # shared by issues and pull requests (the default)
host = "github.com"
repo = "owner/repo"         # GitLab: the full project path
push = "auto"               # this tracker, both kinds
issues = { scope = "assigned" }   # issues only: scope (involved | assigned | all), since_days

[[tracker]]                 # issues only
forge = "gitlab"            # only for a host agt cannot identify
host = "gitlab.example.com"
repo = "team/tickets"
pull_requests = false

[[tracker]]                 # drop a remote that became a tracker for pull requests
host = "github.com"
repo = "upstream-org/agt"
pull_requests = false
```

One `[[tracker]]` list serves issues and pull requests. "Pull requests" means review sync and
publish, and the writes of `agt pr`.

Trackers come first from the git remotes. `[sync] remotes` chooses which: `all` (the
default) every remote (`origin`, `upstream`, then the rest by name), `origin` only `origin`,
`none` only the configured entries. A remote becomes a tracker only when its URL names a
repository on a host agt knows to be GitHub or GitLab. A `[[tracker]]` at the same host and
repository (ASCII case ignored) overrides the fields of that tracker; any other entry adds one.
`enabled = false` drops a tracker, and `issues = false` or `pull_requests = false` drops it for that
kind. `issues` and `pull_requests` can also be a table that overrides `pull` and `push` for that
kind; for issues also `scope` and `since_days`. `forge` is needed only for a host agt cannot
identify. `remote` names the git remote of an entry (filled in for remotes) and lets `--remote`
select it.

Each tracker has a `pull` and a `push` mode per kind, from the first of these that sets it: the
kind table of the tracker, the tracker, `[issues]` or `[pull_requests]`, `[sync]`, then the
defaults `pull = "manual"` and `push = "off"`.

| Mode | Meaning |
| --- | --- |
| `off` | `push` only, and its default. Nothing is sent; explicit commands keep work local and say which key to set. |
| `manual` | Only explicit commands reach the forge. This is the `pull` default. |
| `auto` | Explicit commands, plus automatic stale reads (`pull`) and automatic sending after a change (`push`). |

`pull` has no `off`: pulling is always allowed on an explicit command, and `pull = "off"` is a
configuration error naming the key. The old `[sync] auto_pull = false` means `manual`.

`agt issue sync` and `agt review sync` always pull, then push the local changes of every tracker
whose `push` is `manual` or `auto`, with the same sending code and safety rules as `issue push`
and `review publish` (a review of someone else's PR/MR is never published). With the default
`push = "off"` they only pull, and the report says the push was kept local. `issue push` and
`review publish` only push. `issue sync` pulls every issue tracker, each with its own `scope`
(default `involved`). `review sync` without `--pr` refreshes only the reviews already in the
local store, on whichever tracker each belongs to (`--remote` limits it to one), and never
imports a new PR/MR; `--pr <n|url>` starts a local review of one, as `review run` on a branch can.

`agt pr create`, `update`, `merge` and `auto-merge` are refused only when `push` was set to
`off` explicitly; unset, they stay allowed. Reads of any command are never refused by `push`.
`review publish`, `qa publish` and `issue push` need `manual` or `auto`: with `off`, or unset,
they keep everything local, exit 0 and print why on stderr, naming the key. Their `--dry-run`
previews still work.

Commands that act on one PR/MR choose the tracker in this order: `--remote <name|host/repo>`,
the repository of the PR/MR URL given, the tracker a synced review target stores, the tracker
of `origin`, the only tracker. When several remain and none is `origin`, the command fails and
lists the `--remote` choices. Without any tracker for a repository whose `origin` names a host
agt cannot identify, `origin` is used as before. `agt ci` and `agt branch check` choose their tracker
the same way and take `--remote`. `agt cleanup` instead asks every pull request tracker for the PRs/MRs of a branch, and a PR/MR merged on any of them counts as landed. `--tracker` on `agt issue` selects an issue
destination by key or `owner/repo`; `issue sync` pulls every issue tracker; automatic issue work uses only the tracker of `origin` unless `--tracker` is given.

The repository's `.config/agt.toml` can raise a `push` mode but never lower the global one:
where both files set the same `push`, the higher of `off`, `manual`, `auto` wins. A
`[[tracker]]` in the repository file whose host and repository match no remote of that
repository is ignored, with a warning.

The older keys still load and are rewritten, with one `agt: config: ... is deprecated; use ...`
line on stderr per key and file:

| Old | New |
| --- | --- |
| `[forge] post = false` | nothing; remove it, since `push` is off by default (an explicit `push = "off"` would also refuse `agt pr` writes) |
| `post = true` | `[sync] push = "manual"`, or `"auto"` with `[sync] auto_push = true` |
| `[sync] auto_pull = true` or `false` | `[sync] pull = "auto"` or `"manual"` |
| `[[issues.tracker]]` | `[[tracker]]` for issues only; `scope` and `since_days` move into `issues = { ... }`. A remote it names stays a pull request tracker |
| `push = false` on an issue tracker | `issues = { push = "off" }` |

An old key and its new key in one file are an error that names both.

`involved` includes issues you created, are assigned or (GitHub only) are mentioned in.
GitLab has no mentions filter. Credentials come from `gh` and `glab`.
Issues live under `<main-checkout>/.agt/issues/<tracker>/<number-or-local-id>.json`.
Remote tracker keys are `gh-<host>-<owner>-<repo>` or `gl-<host>-<group>-<project>`.
Local issues use the local tracker. `--tracker` accepts a key or `owner/repo`;
issue references accept `owner/repo#12`, `#12`, `12` or `local-xxxxxx`.
Different destinations sharing a tracker key cause an identity collision error.
The collided tracker remains unchanged, and no request reaches its forge.
Host and repository comparisons ignore ASCII case; existing tracker keys stay unchanged.

### Auto-sync of issues

Reads (`list`, `show`, `status`) stay local unless a tracker's issue `pull` is `auto`.
Enabled pulls refresh applicable trackers when unsynced
or at least `max_age_secs` old. Failures back off for that interval and print
`agt: sync skipped (<reason>); showing local data`. Local output and exit status remain intact.

With `push = "auto"` for the tracker, mutations with a remote representation send only
the changed issue after saving it. Its pending content is eligible; untouched
issues remain local. Notes and local links never trigger publication or get sent.
Automatic failures preserve queued changes. Private data is never included in remote bodies.

Automatic work has a 15-second aggregate limit. Explicit `sync` and `push`
opt into remote access independently of automatic settings. `--offline` and
`AGT_OFFLINE=1` block automatic and explicit issue network operations.

| Command | What it does |
| --- | --- |
| `sync` | Lists what changed since the last watermark (five minutes of overlap), fetches those issues with their comments and the PRs/MRs the host links, merges them in. Skips an issue whose `updated_at` is unchanged. Reports `listed`, `new`, `updated`, `unchanged`, `claimed`, `conflicts` per tracker. |
| `list [--state open\|closed\|all] [--label L] [--assignee A] [--author A] [--pending] [--text T] [--pr <n\|url>] [--branch B] [--title T]` | Summaries; `pending` is whether anything is queued for the issue. `--state` defaults to `open`, or to `all` when `--text`, `--pr`, `--branch` or `--title` is given. `--text` is a case-insensitive match over title, body, comments and notes; `matched` says where. `--pr`, `--branch` and `--title` look issues up: issues linked to a PR/MR; issues named by a number in the branch (`12-fix`, `feat/12-x`, `issue-12`, `gh-12`) or a PR/MR branch the host reported or `link --branch` recorded; issues a `#12`, `owner/repo#12` or issue URL in the title names. `via` says which; an issue that matches any of the three lookups is kept. All filters combine with AND. |
| `show <key>` | The issue, its comments, links, notes, what is queued (`queued`), conflicts and errors. |
| `status` | Per tracker: counts, issues with queued changes, conflicts, errors, `push` and the last sync. |
| `link <key> --pr\|--branch <value>` | A local link, never sent. With `--pr <n\|url> --comment` it also queues a comment `Linked: <url>`, once per PR/MR. GitHub shows the mention on the PR's timeline, not the issue's; a later `sync` links the PR to the issue only when the PR says `Fixes #n`. GitLab lists the MR as related. |

Queued writes (`<text>` is read from stdin when it is `-`):

| Command | Queues |
| --- | --- |
| `comment <key> <text> [--private]` | A comment. With `--private` it is kept as a note on the issue instead, never queued and never sent. |
| `create --title T [--body B] [--add-label L]... [--add-assignee A]...` | A local issue under a `local-` key. `--tracker` selects a remote destination. `--label` and `--assignee` are accepted for `--add-label` and `--add-assignee`. |
| `edit <key> [--title T] [--body B] [--state open\|closed [--reason completed\|not-planned]] [--add-label L]... [--remove-label L]... [--add-assignee A]... [--remove-assignee A]...` | Any of these in one change; at least one is required. A reason is GitHub only. GitLab takes one assignee here; removals apply first, so one call can swap it. |

`discard <key>` drops every queued, unpushed change of one issue and never calls a host. A
`local-` issue that was never pushed is removed with its queued comments and private notes. For
an issue on a tracker it drops the queued edits (fields reset to the synced copy), the queued
comments, and any conflicts and send errors for them; the synced copy, links and private notes
stay. It prints `key`, `tracker` and `discarded` (`issue`, `comments`, and for a tracker issue
`edits`, the fields reset; for a local issue `notes`). An unknown key, or an issue with nothing
queued, is an error; so is any discard while another agt holds the tracker's push lock (a push is
running). With that lock held, a create or comment still marked as being sent is stale, left by
a push that failed or was killed, so it is discarded too and listed in `discarded.possibly_sent`
(`create`, or the comment id): the host may already have received it, and `agt issue sync` shows
whether it landed. A document written by a newer agt is refused with a hint to upgrade agt. Other
issues are untouched.

`push [--dry-run] [--force]` sends the queue, per issue in this order: create, then
one update with every changed field, then each comment. `--dry-run` prints the exact
`gh`/`glab` argument list and body of every write (`writes`; a real run lists `sent`)
and reads only. A tracker with `push = false` is skipped, or an error when named with
`--tracker`. An issue with nothing queued costs no call.

Before sending, `push` re-fetches the issue and merges three ways against the last
synced copy (`base`). A field only you changed is sent, one only the host changed is
taken. A title, body or state both sides changed to different values is a conflict:
the host's value wins, `push` reports it under `conflicts` (and `status` shows it),
and `push --force` sends your value on top. Labels and assignees merge as additions
and removals. After a send `base` moves to what was sent, so a second `push` sends
nothing.

A comment is marked `posting` with an attempt id before it is sent, and carries a
hidden `<!-- agent-review id=<id> attempt=<attempt> -->` marker. If the response is
lost, the next `sync` or `push` finds the comment by that marker and records it
instead of sending it again. A create works the same way (the marker is in the body;
`sync` or `push` then gives the local issue its number). A send the host refuses (a
4xx other than a timeout or rate limit) puts the comment back in the queue with the
error kept under `errors`; any other failure leaves it marked as possibly sent. A
failure does not make `push` exit non-zero: it is listed under `failed`. A create with no answer, whether the call failed, its response could not be read, or the process died mid-call, is not sent again for two minutes after the later of its send and its last error, since the hosts list a new issue late and a search right after proves nothing; `push` reports it under `deferred`, and a later push claims it or sends it again.

## Testing

`AGT_GH_BIN` and `AGT_GLAB_BIN` replace the `gh` and `glab`
executables; tests point both at `test/fixtures/fake-forge.sh`, which replays
canned responses and fails on any other call. `test/fixtures/fake-steps.sh` replays numbered steps in call order for `agt ci wait`, and `test/fixtures/ci/*.json` hold the responses and the output for `agt ci failures`. `AGT_NOW=<unix seconds>` replaces the current time. It exists only so
tests can check age pruning and sync age; do not set it otherwise. `AGT_SYNC_CAP_MS` replaces the 15 seconds an auto-sync may take, for the same
reason. Default issue/review tests assert local operation; automatic-sync tests opt in explicitly. `AGT_OFFLINE=1` (`Env::offline`) also blocks explicit remote calls. Tests use temporary
`HOME`, `XDG_STATE_HOME`, `XDG_CONFIG_HOME` and git repositories with
`GIT_CONFIG_GLOBAL`/`GIT_CONFIG_NOSYSTEM`. See [AGENTS.md](../AGENTS.md#checks) for the required checks.
