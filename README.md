# agt

`agt` is one command-line binary for coding agents. `agt issue` syncs GitHub and GitLab issues (see [Issues](#issues)). Reads work on the local copy and pull from the forge first when it is older than five minutes; see [Auto-sync](#auto-sync). `agt review` keeps PR/MR-style code review threads on your machine, one store
per repository. It records review runs and passes, each reviewer's model,
effort, harness and harness version, the findings reviewers kept or killed, and
line-anchored threads with replies and resolutions. It works without a PR or
MR, so Claude Code, Codex, Cursor or a person can pick up where a review left
off. When a GitHub PR or GitLab MR exists, `sync` imports all of its threads
and comments.

## Install

On macOS (arm64) and Linux (x64, arm64), from the private GitHub releases;
mise reads the token of an authenticated `gh`:

```sh
mise use -g github:brandonocasey/agt
```

From a checkout, `mise run install` runs `cargo install --locked --path .` and
puts `agt` in `~/.cargo/bin`. A `v*` tag publishes a release
(`.github/workflows/release.yml`); Windows is not supported. Git 2.31 or newer must be on `PATH`; before git 2.38 (no
`merge-tree --write-tree`) every merge commit in a range counts as unreviewed.
`sync` also needs an authenticated `gh` (GitHub) or `glab` (GitLab).

Run `agt --version` to check the install; it prints `agt <version>`. Every review
command below is a subcommand of `agt review`. Up to 0.4.0 the binary was
`agent-review` and took the same arguments without the `review` word; the store
location, schema, output and exit codes are unchanged, and a store written by
0.4.0 reads as is. The `agent-review` markers in posted comments and the
`$XDG_CONFIG_HOME/agent-review/hosts.json` path keep their names.

Repositories with SHA-1 or SHA-256 object ids both work.

## Output and errors

Every command prints JSON on stdout, except `list --format prompt`, which prints
plain text. JSON is indented at a terminal and one compact line otherwise, since
agents pay for every byte they read. On failure a command prints `agt: error: <message>` on stderr (the word `error:` matches
clap's usage errors, so one pattern finds both) and
exits with status 1. Timestamps are RFC 3339 in UTC, such as
`2026-10-16T12:00:00Z`.

## Targets

A target is one thing under review. Commands pick it like this:

- By default, the current branch. The target key is the branch plus its
  merge-base with the default branch (`origin/HEAD`, else local `main`,
  `master` or `init.defaultBranch`; override with `--default-branch <ref>`).
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

Start a run, then passes inside it. A `full` pass opens a new cycle (`c1p0`,
`c2p0`); `fix-check` and `reveal` passes continue the current cycle (`c1p1`,
`c1p2`).

```sh
agt review run start
# {"run": "rv-7k2m", "target": "branch-feat%2Flogin-1a2b..."}

agt review pass start --run rv-7k2m --kind full --reviewers - <<'EOF'
[{"model": "claude-opus-5-5", "effort": "xhigh", "harness": "claude-code",
  "harness_version": "2.1.296", "starting_context": "fresh",
  "session": "a0b5ad828155531d2"}]
EOF
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

`run start --kind <kind>` also starts the run's first pass and takes the same
`--base`, `--reviewers` and `--head`, saving a call:

```sh
agt review run start --kind full --base 1a2b3c4 --reviewers - < reviewers.json
# {"target": "...", "run": "rv-7k2m", "pass": {"id": "c1p0", ...}}
```

```sh
agt review finding add --run rv-7k2m --pass c1p0 --reviewer r1 < findings.json
agt review pass end --run rv-7k2m --pass c1p0 --lock
agt review pass end --run rv-7k2m --pass c1p0 --findings - --reviewer r1 < all.json
agt review finding kill rv-7k2m.c1p0.r1.f2 --reason "The caller checks it."
agt review run end --run rv-7k2m
agt review run end --run rv-7k2m --reveal-agree 2 --reveal-disagree 0
```

`pass end --findings <file|->` adds findings and ends the pass in one write, as
`finding add` would for each reviewer: each finding names its `"reviewer"`, or
takes `--reviewer`; `[]` ends a clean pass. Output lists the new ids under
`added` and `killed`, each in input order. The input may instead be an object,
`{"findings": [...], "usage": [{"reviewer": "r1", "tokens": 52000,
"duration_ms": 61000, "tool_uses": 9}]}`, which also stores what each reviewer
spent (as its harness reports it) on the pass. `pass end --lock` freezes the pass for a blind review:
`finding add` to it then fails. `finding kill` moves a surviving finding to its pass's killed list, also
on a locked pass; `finding kill` refuses a finding that is posted or being posted to the
PR/MR, so resolve it with `--triage stale` instead. `run end` closes the run, records commit patch-ids
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

`approve` records an approval of the head as a general thread, refused while a
`bug`, `requirement` or `question` is open; approving the same head again
returns the same thread, and `--formal` upgrades a comment approval. `publish` posts it as a comment (default text
`Approved at <sha>.`, or `--body`; a GitLab note, which never blocks merging).
`approve --formal` also submits the host's approval: a GitHub `APPROVE` review,
or GitLab's `approve` at that head. On your own GitHub PR, which GitHub refuses
to approve, the comment still posts and `skipped` lists `<id>/formal`. A host
refusal of the formal approval (an HTTP 4xx: already approved, not allowed)
shows as an `error` under `posted`, blocks nothing and is remembered, so
`publish` does not send it again; `approve --formal` again retries only the host's
approval. A failure that is not a refusal (a timeout, a 5xx, the tool failing)
shows as an `error` too but is not remembered: the next `publish` sends the
approval again. An accepted approval is never sent again. An approval posts
only while the PR/MR head is the head it approved and no blocking finding is
open, never counts as open, and goes to someone else's PR/MR with
`publish --not-own --only <id>`.

A wrong run, pass or reviewer id fails with the valid ones named: the open
runs, the run's passes, or the pass's reviewers. Run ids are unique in the repository. With no `--target` or `--range`, a
command that names a run (`--run`, or a thread, killed finding or pass id that
starts with one) works on the range, commit or PR/MR target holding that run,
unless the current branch's own target holds it; so a `--range` review needs
`--range` only on `run start`. Another branch's target is never picked, since it
would evaluate that branch's tip rather than the checked-out work. `run start
--kind` that fails to start its pass leaves no run behind.

Threads for humans and agents:

```sh
agt review comment --body "Why 3 retries?" --path src/net.rs --line 42
agt review comment --body - --type question < note.md   # general comment
agt review reply h1 --body "Matches the API limit."
agt review reply rv-7k2m.c1p0.r1.f1 --body "Fixed in 9e8d7c6" \
  --run rv-7k2m --pass c1p0 --reviewer fix --model claude-opus-5-5 --harness claude-code \
  --session dcec84d5-4c7a-492f-ba78-a9ddd3d1bc7f
agt review resolve h1 --resolution answered
agt review resolve rv-7k2m.c1p0.r1.f1 --triage fixed --resolution "fixed 9e8d7c6"
agt review resolve rv-7k2m.c1p0.r1.f2 --triage question     # held, stays open
agt review resolve rv-7k2m.c1p0.r1.f1 --reopen --resolution "not fixed: still loses data"
# Fixed: triage, resolution and a "✅ Fixed in <sha> <subject>." reply for each, in one write
agt review resolve rv-7k2m.c1p0.r1.f1 rv-7k2m.c1p0.r2.f1 --fixed 9e8d7c6 \
  --run rv-7k2m --pass c1p1 --reviewer fix
# A fix check disagrees: reopen with a "❌ Not fixed: <reason>." reply
agt review resolve rv-7k2m.c1p0.r1.f1 --not-fixed "still loses data" \
  --run rv-7k2m --pass c1p1 --reviewer r1
```

- `comment` options: `--type bug|question|requirement|nit|comment` (default
  `comment`), `--severity high|medium|low`, `--path`, `--line`, `--old-line`,
  `--side new|old`, `--start-line`, `--commit <rev>`. Line numbers refer to
  `--commit`, else `HEAD`, else the uncommitted work tree when it is dirty.
- Author: `--author <login>` for a human, or agent flags `--model`,
  `--effort`, `--harness`, `--harness-version`, `--run`, `--pass`,
  `--reviewer`, `--session`. `--reviewer` with `--run` and `--pass` fills the
  rest from the stored reviewer; `--session` is the harness's id for the agent,
  such as the main session's. With no author flags the author is the forge login that
  `sync` last authenticated as for this repository, else git `user.name`,
  which the next `sync` (or `publish`) replaces with the forge login.
- `reply --local` keeps the reply in the store only, such as the user's answer
  to a held question: `list --format prompt` shows it and `publish` never posts
  it.
- `resolve`: `--resolution` is one of `fixed <sha>`, `not fixed: <reason>`,
  `rejected: <reason>`, `stale` or `answered`. `--triage fixed|stale` resolves (`stale` implies the
  `stale` resolution; `fixed` needs `--resolution "fixed <sha>"`).
  `--triage valid|question` only records the triage and leaves the thread open;
  `question` marks it held. `--reopen` marks it unresolved. `resolve` takes
  several ids and changes them all in one write, or none when one is missing.
  `--fixed <sha>` means `--triage fixed --resolution "fixed <sha>"` plus a reply
  `✅ Fixed in [<sha> <subject>](<commit URL>).` (the link once the target is
  linked to a PR/MR); `--not-fixed <reason>` means `--reopen --resolution
  "not fixed: <reason>"` plus a reply `❌ Not fixed: <reason>.`; `--rejected
  <reason>` resolves a finding that will not be fixed, with `rejected: <reason>`
  and a reply `🚫 Rejected: <reason>.`, and the summary counts it as
  `<n> rejected`. A finding rejected before it was ever posted is not posted, as
  with `stale`, and the summary lists it by its first sentence only; another store that syncs the reply reads the rejection back. `--reply <text>`
  replaces that text, or adds a reply to any `resolve`; the author flags sign
  it. With one id the output is the `thread`; with more, `threads` lists each
  id with `resolved`, `resolution` and `held`.

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
```

`list` prints threads (each with `current`: the anchor mapped to `HEAD`, or
for imported threads the host's path, line and outdated state) and killed
findings. `--open` keeps the threads `status` counts as open. `--run` keeps only
that run's findings, and `--cycle <n>` (with `--run`) only that cycle's threads
and killed findings. `--format prompt` prints the text only of comments written
locally or by the authenticated forge user; every other comment shows as
`(comment by another author, text hidden)`, so reviewers never read other
people's comments. A posted thread also shows its forge URL on a `URL:` line.
`--head <rev>` maps anchors to that commit instead of the target's head, such
as a fix after a range target's end.

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
agt review sync                     # the PR/MR whose head is this branch
agt review sync --pr 1235
agt review sync --pr https://gitlab.example.com/group/proj/-/merge_requests/3
```

`sync` is read-only on the forge: it sends only `GET` requests and GraphQL
queries. `list`, `show` and `status` run it for you when it is stale; see
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

## Cleanup

`agt cleanup` cleans up after agents in one pass: the whole store (every repository and
namespace), old cache files and orphan processes. It also lists leftovers for you to remove
by hand. By default it changes nothing and prints a JSON plan; `--yes` deletes. `--only`
limits the pass to `reviews`, `issues`, `tasks`, `backups`, `locks`, `output`, `scratch`,
`processes` or `manual` (repeat it for several). Before 0.8.0 it was `agt prune`.

```sh
agt cleanup                            # print the plan
agt cleanup --only backups             # plan for one kind
agt cleanup --yes                      # delete; processes are left alone
agt cleanup --only processes --yes     # stop orphan processes
agt cleanup --count                    # leftovers the last --yes run listed
```

Store documents are kept 90 days (`retention_days`). The rules:

- `reviews`: the review rule above, applied to every repository, and skipped for a
  target whose `.<key>.publish.lock` is held. agt records the absolute git common
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
  a document with fields this agt does not know (with a warning). Open issues,
  `tracker.json` and anything holding such data stay.
- `tasks`: a `complete` task 90 days after its `updatedAt`. Active, paused and blocked
  tasks stay, and so does the session index, a cache that `agt sessions index` rebuilds.
- `locks`: `.<key>.lock` and `.<key>.publish.lock` files whose `<key>.json` is gone,
  except an issue tracker's `.push.publish.lock` while `tracker.json` exists.
- `backups`: `<namespace>-v<n>-<stamp>` directories that agt made before a schema
  migration, 90 days after they were taken, except the newest of each namespace (a
  stamp in the future does not count as the newest), and
  unfinished `.<namespace>-v<n>-<stamp>.partial` copies (`partial_backups`). Other
  names in the same backups directory belong to other tools and are never touched.

Cache files go by age. Each top-level entry goes whole, and only once nothing in it changed
for the whole period, so a task that is still writing keeps all of its files:

- `output`: entries of `$XDG_CACHE_HOME/agents/output` (default `~/.cache`) untouched for
  7 days.
- `scratch`: entries of `$XDG_CACHE_HOME/agents/scratch` (except `.keep`) and Claude Code
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

`manual` lists what cleanup never deletes, in `manual` by section: `worktrees` (linked worktrees
whose HEAD is already in the default branch, clean, unlocked, untouched for 3 days (see `agt worktree cleanup`) and with no live agent session, as
`agt worktree remove <path> --yes`) and `branches` (local branches in the default branch, not
protected and not checked out, as `git -C <repo> branch -d <name>`), for
the repositories under the `agt worktree` roots; this is local ancestry only, with no network and no
`lsof`. `backups` (everything in the
backups directory but agt's own, largest first); `codex_releases` (all but the current
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

- Forge, in this order: an entry in `$XDG_CONFIG_HOME/agent-review/hosts.json`
  (`{"git.example.com": {"service": "gitlab"}}`, `github` or `gitlab`); the
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
  `/-/merge_requests/<iid>` URL) or the `origin` remote.
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
  or reopen that `publish` has not sent yet (`resolve_pending`). Local fields
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
  marker) answers that thread: it is added as a reply and, when the thread is
  not resolvable, resolves it as `fixed <sha>` (a `Fixed in` answer, also with
  the sha as a link) or `rejected: <reason>` (a `Rejected:` answer), or reopens
  it (any other answer).
- A draft note with a marker for a local comment and its attempt id marks it
  `posting` with its `draft_id`, so a later publish publishes it; a `draft_id` whose draft is gone
  is cleared.
- Threads and comments the host no longer returns are kept and flagged
  `deleted_remote: true` with `deleted_seen_at`; they are never dropped and
  are not open. The flag clears if they come back.
- The authenticated login is cached in `.account.json` in the repository's
  store directory.

### Auto-sync

`list`, `show` and `status` (the only review commands that just read) sync
before they read, so a review sees the PR/MR as it is now without a manual
`sync`. Writes such as `pass end` and `resolve` never sync; `publish` syncs as
before. It is the same sync as `agt review sync`, so it only reads the forge.

- The target has a PR/MR and its `synced_at` is older than `sync.max_age_secs`
  (default 300): sync it. A fresh one makes no call.
- The target has none, or its PR/MR is closed or merged: look the branch's PR/MR
  up, as `sync` does, at most once per max age, so a newer PR/MR of the branch
  is found. A miss is remembered in `.auto-sync.json` in the repository's store
  directory (older agt ignores the file), so the reads that follow call
  nothing. Range and commit targets have no PR/MR to look up.
- No `origin`, or a host no forge is known for: skipped without a word.
- The forge failing (`gh`/`glab` missing, not logged in, network, rate limit) or
  the calls taking longer than 15 seconds in all (the tool is killed) prints
  `agt: sync skipped (<reason>); showing local data` to stderr, then reads the
  local data. Stdout and the exit status are what the local read gives. A
  successful auto-sync prints nothing. A failed attempt is written to
  `.auto-sync.json` and that sync (the PR/MR, or the branch lookup) is not tried
  again for the max age, so an offline machine pays the cap and prints the line
  once, not on every read; a success or an explicit `sync` clears it. A lock held
  by another auto-sync or by a push or publish of the same review skips the sync the same
  way instead of waiting. An explicit `agt review sync` takes no sync lock and is never
  skipped.
- `--offline`, `AGT_OFFLINE=1` or `auto = false` in the `[sync]` table of
  `$XDG_CONFIG_HOME/agt/config.toml` keep a read local; see
  [Issues](#issues) for the table. A config file that cannot be read (an unknown
  key, a bad value) does not break the read: the skip line carries the config
  error and the local data follows. `--offline` never reads the file; an
  explicit `sync` still fails on it.

An explicit `agt review sync` is unchanged: it has no time cap and fails with
its own errors.

## Publish

- Only on a PR/MR authored by the authenticated login, unless `--not-own`. On
  someone else's PR/MR, `--not-own` posts only replies, resolves and reopens on
  threads imported from the host (other people's threads); our findings, local
  threads and the summary are listed under `skipped`. `--not-own --findings
  --only <id,...>`, when the user asks for it, also posts the named surviving
  findings and local threads the user wrote, with their replies and resolves;
  `--findings` without `--only` is refused, and the summary never goes to
  someone else's PR/MR.
- `--only <id>[,<id>...]` limits writes to those threads and their pending
  replies and resolves; every other thread and the summary are listed under
  `skipped`.
  `publish` syncs first, then posts in this order: new threads, sync, replies,
  resolves and reopens, the summary, then a last sync to fill in ids.
- Posted: new surviving findings of ended passes (a locked pass's findings wait
  until a later `reveal` pass of the same cycle, or the run, has ended) and
  local comment threads, new replies, resolve and reopen changes, and the
  summary when a run ended since it was posted or its text changed. Never posted: killed findings,
  findings resolved `stale` before they were posted (the summary lists them by
  their first sentence only), imported comments, local
  comments by another human author, and runs that ended before the target's
  `attached_at` (history from before the PR/MR) with their findings, replies,
  resolves and records (listed under `skipped`).
- A finding opens with its kind and an emoji: `**🐛 bug (high):**`,
  `**❓ question:**`, `**📋 requirement:**`, `**✏️ nit:**`. An agent signs in a
  grey quote under its text, `> <sub><model> <effort> · <harness> <version> ·
  agent <id></sub>` (`session <id>` for the main session, plus ` · 🙈 blind
  review` or ` · post-reveal`; a default effort is left out); a one-line agent
  reply is all quote, `> ✅ Fixed in ... <sub>signature</sub>`. People sign
  nothing. Every body ends with
  `<!-- agent-review id=<local id> attempt=<attempt> -->`, with one attempt id
  per `publish` (`<attempt>` in a dry run). Finding threads also carry
  `<!-- review-finding v1 id=<finding id> sha=<reviewed sha> -->`.
- A thread anchored to lines of a file the PR/MR changes posts only where those
  lines sit in its diff; otherwise it is listed under `skipped`. A thread on a
  file the PR/MR does not change, or on a whole file, posts as a general comment
  whose first line names its location. A
  line range (`start_line`..`line`) posts only as one multi-line comment, with
  both ends on the new side of one hunk. Threads without an anchor post as
  general comments. On GitLab, lines a later commit changed (such as a fix)
  are not in the latest MR diff, so they too post as a general comment with
  their location.
- GitHub: in-diff comments go out as one `COMMENT` review per reviewed commit
  (`POST repos/<repo>/pulls/<n>/reviews`; comment ids are read back from the
  review), with `start_line` and `start_side` for a range. Replies and resolves use the GraphQL
  `addPullRequestReviewThreadReply`, `resolveReviewThread` and
  `unresolveReviewThread` mutations; a reply to a general comment is a new
  issue comment ending in `Re: <thread id>`.
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
  it. It opens with `**✅ Agent review: <O> open at <head>.**`, the outcome
  counts and how many PR/MR commits a pass reviewed, then collapsed sections:
  the findings (kind, first sentence, where, raised in, outcome, and the text
  of each one not posted inline, except one withdrawn before it was posted,
  which shows only its first sentence), the commits and
  how each was reviewed, and the numbered steps of every run with its time. A
  grey quoted table names each agent's model, harness and id, above
  `Posted by agt <version>`. A summary longer than GitHub's 65,536 characters
  is cut at a line and says so. It ends with one
  `<!-- review-record v2 id=<run> head=<repaired head> reviews-at-head=<N> open=<O> -->`
  per run it covers and `<!-- agent-review id=summary attempt=<attempt> -->`.
  `N` counts the run's passes covering the repaired head by the same rules as
  `status`. Each covered run's `record` names the summary comment.
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

Files live under `$XDG_STATE_HOME/agents/reviews/` (default
`~/.local/state/agents/reviews/`), in a directory per repository:
`<host>/<owner>/<repo>` from the `origin` URL, or
`local/<name>-<hash>` without one (the hash is from `git hash-object` of the git
common directory path). Each target is one JSON file, mode 0600, in a 0700
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
      "record": { "post": "posted", "attempt": "at-x1y2", "sent": "**✅ Agent review: ...",
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

`finding add` reads one JSON object or an array of them from stdin. Unknown
fields are rejected.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `type` | `bug`, `question`, `requirement`, `nit`, `comment` | yes | Finding class |
| `severity` | `high`, `medium`, `low` | for `bug` | Bug severity |
| `scenario` (alias `body`) | string | yes | Failure scenario; becomes the first comment |
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

`agt task` takes `create`, `list`, `show`, `update` and `check`; `agt task --help` lists them.
Arguments are parsed like every other `agt` command, so a usage error (a missing value, an
unknown flag or command, a bad `--status`, a `--revision` below 1, no command at all) exits with
2 and clap's message; other errors exit with 1.

Task records are one pretty-printed JSON file each in `$XDG_STATE_HOME/agents/tasks/<id>.json`
(mode 0600, directory 0700), guarded by a `.<id>.lock` directory and a `revision` that every
write must name (record `version` 1). Commands that find or search sessions are
`agt sessions index`, `resolve` and `search`.

## CI

`agt ci` waits for PR/MR checks and triages GitLab failures.

```sh
agt ci wait TARGET... [--sha SHA] [--timeout SEC] [--interval SEC] [--max-interval SEC]
agt ci failures <mr> [-n N] [-R group/project]
```

`wait` takes one or more targets: GitHub `OWNER/REPO#PR[@SHA]`, GitLab `GROUP/PROJECT!IID[@SHA]`,
a PR or MR URL, `#PR` or `!IID` in the current repository (named by its `origin` remote), and a
GitLab pipeline id (current repository) or pipeline URL. Without `@SHA` the head read by the first
poll is pinned, and a later head is a change. `--sha SHA` is for a single PR or MR right after a
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

`failures <mr>` is GitLab only (`123`, `!123` or an MR URL; `-R` or the current repository
names the project). It lists the failed and retried jobs of the MR's head pipeline, the failing
specs in their logs (Playwright, WebdriverIO, Karma, Jest) and how often each failed in the last
N (10) finished default-branch pipelines, as a verdict per spec: `new`, `pre-existing`, `flaky`,
`pending` or `unclear`.

## Sessions

`agt sessions` reports on Claude Code transcripts (`~/.claude*/projects`)
and Codex rollouts (`~/.codex/sessions`, `~/.codex/archived_sessions`). Reports are text
tables; `cost --json` prints indented JSON with `rows`, `unpriced` and `fast_calls`.

```sh
agt sessions cost [--by agent,model,project,session,file,harness] [--json] [FILE...]
agt sessions cost --session ID [--json]   # one session's cost so far, subagents included
agt sessions agents          # per agent type: runs, start and peak context, calls, USD
agt sessions tools           # tool calls and the share of cost their results carry
agt sessions cache-gaps [--gap MIN]
agt sessions errors          # tool errors grouped by tool and message
agt sessions denials         # permission denials grouped by source and reason
agt sessions friction [--tsv [--width N]]
agt sessions index [--session ID] [--file FILE] [--rebuild] [--max-files N] [--byte-limit N]
agt sessions resolve SESSION [index options]
agt sessions search --query TEXT [--session ID] [--limit N] [index options]
```

Every command takes `--days N` (30) or `--since YYYY-MM-DD`, `--project TEXT`, `--scratch`
(scratch and probe projects are skipped otherwise), `--root DIR` (repeatable), `--top N` (25) and
`--harness all|claude|codex`. `cost --session ID` takes the full session id and prints that one
session's cost so far from its transcript and its subagents' (it ignores the other options).
A transcript last changed before the start date is not read. A root named with `--root` can hold either harness; a file whose
first line is a Codex `session_meta` is read as Codex. Prices come from
`~/.config/agents/model-prices.json`, or the file `AGT_PRICES` names; models it does not list are
reported as unpriced.

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
session. Default Claude Code roots are every `~/.claude*/projects`, links followed.

`index`, `resolve` and `search` find sessions and what was typed in them, with a private index
of the human turns in Codex and Claude transcripts, prompt history files and
`--file` in `agents/tasks/session-index/index.json` (version 4). The index is updated
incrementally; `--rebuild`, a rewritten file or another index version scans again. `index`
prints the coverage as JSON; `resolve` prints the session's sources and turn count; `search`
prints up to `--limit` (1 to 100, 20) user turns that contain the query, case-insensitively,
newest first, with the turns around each, and the coverage as one JSON line on stderr. Turns a
fork replays from its parent are marked `inherited` and left out of a search without
`--session`. `--max-files` and `--byte-limit` (or the environment variables
`AGENT_TASK_MAX_FILES` and `AGENT_TASK_BYTE_LIMIT`) bound a run; `--session` and `resolve` still
find an exact session beyond `--max-files`.

## Ps

`agt ps [--json]` lists the agents running now, one row each, busy first and then longest in
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

Running `codex` processes are listed too, with `harness: "codex"` and no name, status or kind. Codex
writes no live record, so the session is the newest rollout in `~/.codex/sessions` whose
`session_meta` `cwd` equals the process's working directory and that changed after the process
started. A subagent's rollout does not count. When no rollout matches, several `codex` processes
share the directory, or the platform cannot read the process's directory, `session` is `null` and
`note` says why.

## Worktrees

`agt worktree` creates git worktrees on the newest default base, installs their dependencies,
gives each a dev-server port, lists them, removes one after backing it up, sweeps away landed work, and restores a backup.
Output is JSON.

```sh
agt worktree create BRANCH [--base C] [--path P] [--prepare] [--port]
agt worktree prepare [PATH] [--clone-deps]  # install dependencies from the lockfiles
agt worktree port [PATH] [--env]            # a stable dev-server port
agt worktree list [--all]                   # this repository's linked worktrees, or every repository under the roots
agt worktree remove PATH [--yes]            # without --yes: print the plan, change nothing
agt worktree cleanup [--all] [--yes] [--offline] [--dirty] [--recent] [--only NAME-OR-PATH]...  # remove landed worktrees, delete landed branches
agt worktree restore ID [--path P]          # recreate the worktree and its uncommitted files
agt worktree backups                        # list backups
agt worktree backups show ID
agt worktree backups rm ID [--yes]          # delete the backup's ref and directory
```

`list` prints per worktree its `path`, `branch` (or `detached`), `head`, `locked`, counts of `modified` tracked, `untracked` and `ignored` (not disposable) files, `ahead` and `behind` the default branch, and `merged` (HEAD is an ancestor of it) (a worktree on a branch with no commit yet has `unborn: true` and `null` for those three), and `agents`: the live Claude Code sessions whose working directory is inside it, each with `pid`, `name`, `status` and `session` (the sessions `agt ps` lists). The default branch is the one `refs/remotes/origin/HEAD` names, else `main`, else `master`. `--all` scans the roots in `$XDG_CONFIG_HOME/agt/config.toml` (a repository it cannot read becomes an entry of `warnings` and the rest are still listed, as `cleanup --all` does); globs and `~` are allowed, and the default is the layout `agt cleanup` assumes:

```toml
[worktree]
roots = ["~/Projects/*/*", "~/projects/*/*"]
```

`remove` refuses the main checkout, a locked worktree, a path that is not a linked worktree, a worktree that holds the current directory, a worktree where a live Claude Code session other than the calling one runs (`~/.claude*/sessions/<pid>.json` with a `cwd` inside it, a running pid and a matching `procStart`; a record whose pid is an ancestor of agt or the `CLAUDE_PID` in agt's environment is the calling session and is ignored), a worktree where any running process other than agt and its ancestors has its current directory (`lsof -a -d cwd -Fpn` on macOS, `/proc/*/cwd` on Linux; the refusal names the pid and command; the calling session's own processes, meaning every descendant of an ancestor of agt that has a live session record or of `CLAUDE_PID`, never block, and are listed under `warnings` as pid and command so you can stop task servers; `cleanup` lists them in the repository's `warnings`), and a worktree whose removal would delete work that nothing else holds. The rule is that `remove` deletes only what the backup holds or another clone holds; anything else refuses, naming the repository and the reason. `git worktree remove --force` deletes every submodule clone under the worktree's git directory (`.git/worktrees/<id>/modules/`, initialized or not, whatever `git submodule status` says) and every `.git` under the worktree, so each of those repositories must hold nothing unique: the stash refuses; every other ref except the remote-tracking ones (branches, tags, notes, anything else) and its HEAD must lead only to commits that a remote-tracking branch of that repository or the main checkout's clone of the same submodule has (decided by commits, in one `rev-list` per repository, however many tags there are), and a tag whose commits nothing else has is still held when `git ls-remote --tags` (10 s limit, or `AGT_LS_REMOTE_TIMEOUT` seconds; no prompt of any kind: `GIT_TERMINAL_PROMPT=0`, askpass helpers disabled, ssh with `BatchMode=yes` and `ConnectTimeout=5` added to your own ssh command, git in its own process group that is killed with ssh at the deadline; the `agt cleanup` report and `agt worktree cleanup --offline` never ask, so such a worktree is simply not listed) shows the remote advertising that tag at the same object; offline or on failure it refuses, naming the tag, it must have no linked worktrees of its own, no uncommitted or untracked files (whatever `status.showUntrackedFiles` says; only an untracked `.DS_Store`, `Thumbs.db` or `desktop.ini` is let through, as at the top level), no ignored files that are not disposable (the same rules as the top level: `.env` counts, `node_modules` and build output do not), and no rebase, merge, cherry-pick or bisect in progress. A submodule that is not at the commit the worktree records refuses too. The worktree itself refuses with a `refs/worktree/*` or `refs/bisect/*` ref, with a rebase, merge, cherry-pick, revert or bisect in progress, with a path that has both a staged and an unstaged change (the backup keeps only the working file), and with files in a submodule directory that is not initialized (git lists none of them; the directories are the gitlinks of the index, in the worktree and in each submodule, whatever `.gitmodules` says; only `.DS_Store`, `Thumbs.db` and `desktop.ini` there never refuse, since nothing there is ignored and a `build/` may hold real work). A nested git repository that no `.gitmodules` registers (a directory with its own `.git` at any depth, also inside submodules and inside ignored or disposable directories) always refuses, naming each path: a backup would keep only a gitlink, so move or push it first. Commits that only a reflog holds are not protected. A leftover clone under the worktree's git directory is checked in its own work tree only while that path still belongs to it. The backup also keeps the worktree's own `info/sparse-checkout` and `config.worktree`, and `restore` puts them back and, when the saved config turns the sparse checkout on, reapplies it (`sparse_checkout: true` in its output; the ignored files are copied back after the reapply, so one in a directory outside the cone stays; a disabled one, or a backup with no saved files, restores as a full checkout, silently, even from a sparse main checkout); if that fails it restores a full checkout and warns, naming the saved files. `agt cleanup`'s manual report lists none of these worktrees either. With `--yes` it first snapshots, then runs `git worktree remove --force` (which removes the worktree's own admin directory; other worktrees' metadata is never pruned). A failed backup leaves no ref and no backup directory. It never deletes the branch.

`cleanup` finds work that already landed in the default branch and removes it: in the current repository, or with `--all` in every repository under the roots. Without `--yes` it prints the plan (`plan: true`; per repository `remove`, `delete`, `kept` and `warnings`, each candidate with its `verdict`, `evidence` and the exact `action`) and changes nothing except fetching PR heads into the object store. With `--yes` the lists are `removed` (`path`, backup `id` and `ref`), `deleted` (`branch`, `tip`, `verdict`, `evidence`, `restore`: the `git branch <name> <tip>` that brings it back), `kept` (`branch` or `path`, `verdict`, `reason`) (a `closed` row also carries the PR/MR's `head` sha, `title` and `closed_at`, read from the forge lookup) and `warnings`. `--only <branch-or-path>` (repeatable, with or without `--yes`) limits the pass to the named rows: a name is a local branch or a worktree path, a worktree row and its branch go together, and unnamed rows are never judged or touched. Each named row is judged exactly as the plan judges it, so it is acted on only while it still qualifies; one that does not is listed under the repository's `skipped` (the `kept` row, with its reason, in place of `kept`), and a name that matches no row (protected, unknown or gone) is listed in the top-level `skipped` with a reason. It never touches the default branch, `main`, `master` or `develop` by name, the main checkout's branch, the current branch or the worktree holding the current directory, and it never deletes a remote branch, pushes, closes a PR, resets or discards files.

A branch gets a verdict, strongest evidence first. `merged`: its tip is an ancestor of the local default branch; it is deleted with `git branch -d`, so git can still refuse. Otherwise, unless `--offline` or `origin` is neither GitHub nor GitLab, the repository's PRs/MRs for that source branch are listed (GitHub GraphQL, GitLab REST; every state, every page) through `gh`/`glab`. An open PR gives `open` (kept, even when an older PR merged); a PR closed without merging gives `closed` (kept, with its number and URL). A merged PR counts only when it merged into the default branch, its source branch is the local branch, it is from the same repository (not a fork), its head commit is local (else `refs/pull/<n>/head` or `refs/merge-requests/<iid>/head` is fetched from `origin` and must equal the head SHA) and the local tip is the PR head or an ancestor of it: `squash-merged`, deleted with `git branch -D` after re-reading the tip, and the tip is reported. A tip past or beside the merged head is `diverged` (kept, `diverged from merged PR #<n>`). A failed forge call, a head SHA mismatch or anything unexpected is `unknown` (kept), and no evidence is `unmerged` (kept). The forge host is found from `origin` as `agt review` finds it, without its `hosts.json`.

A linked worktree whose branch is `merged` or `squash-merged`, or whose detached HEAD is in the default branch, is removed first, exactly as `remove --yes` does (snapshot backup, then `git worktree remove --force` once), and then its branch is deleted. It is kept when it is locked, missing, holds a submodule or a nested git repository `remove` would refuse, has a live Claude Code session in it (as `remove`), or any process with its current directory inside it (`lsof -a -d cwd -Fpn` on macOS, `/proc/*/cwd` on Linux; when that cannot be read the worktree is kept and the reason says so). It is kept as `recent` when anything in it changed in the last 3 days (its HEAD commit time, the mtime of its git dir's `HEAD`, `index` or `logs/HEAD`, or of any tracked or untracked non-ignored file), so a worktree just made by `create` survives, unless `--recent` is passed. It is also kept as `dirty` when it has modified tracked files or untracked files that are not disposable, unless `--dirty` is passed; the snapshot preserves them either way. A kept worktree keeps its branch.

The snapshot is a commit of the whole working tree (tracked changes plus untracked, non-ignored files) with the worktree's HEAD as parent, built in a temporary index (where a tracked file marked skip-worktree or assume-unchanged that exists on disk is taken as it is, so its edits are kept and counted; a file missing from a sparse checkout keeps its index entry, and files outside the sparse patterns are kept too; git older than 2.35 cannot add those, so there `remove` refuses such a worktree) and pinned at `refs/agt/backups/<id>`, so the commits outlive the branch. Ignored files that are not disposable (`.env*`, credentials, notes) are copied with their modes. Disposable paths are skipped: `node_modules/`, `.venv/`, `vendor/`, `target/`, `dist/`, `build/`, `coverage/`, regenerable caches (`__pycache__/`, `.pytest_cache/`, `.mypy_cache/`, `.ruff_cache/`, `.next/`, `.gradle/`, `.turbo/`, `.parcel-cache/`, `.tox/`), `*.log`, `*.tsbuildinfo`, `.DS_Store`, `Thumbs.db`, `desktop.ini`, editor swap files and `.cache/agents/scratch/`.

A backup lives in `$XDG_STATE_HOME/agents/backups/<repo>/<YYYYMMDD-HHMM>-worktree-<name>/` as `backup.json` (id, repository, worktree path, branch, head, snapshot commit, ref, created, copied and skipped paths) and `files/`. The id is the directory name. The `<name>` is the worktree's directory name with every run of characters a git ref refuses (space, `~`, `^`, `:`, `..`, a `.lock` ending) replaced by `-`, and a `-2`, `-3`, ... suffix keeps the id unique across the backups of every repository and across the backup refs. `remove` refuses a worktree on a branch with no commit yet (`git worktree add --orphan`): a backup needs a commit to restore on. Backups are never pruned automatically.

`restore` recreates the worktree at its original path (or `--path`) and refuses an existing path. It checks out the original branch when it exists and is not checked out elsewhere and detaches at the snapshot's parent when it is checked out elsewhere or its tip moved off the snapshot's parent (the warning names the tip it has now, so newer commits never show up as reverted changes), and recreates the branch at the snapshot's parent when it is gone. The working tree then matches the snapshot, with modified, deleted and untracked files left unstaged, and the copied ignored files return. At the original path `claude --continue` finds the old session.

`agt worktree create <branch> [--base COMMIT-ISH] [--path P] [--prepare] [--port]` puts a branch in
a worktree at `<main checkout>/.worktrees/<branch>` (slashes make subdirectories) or at `--path`,
and prints JSON. The main checkout is the first entry of `git worktree list`; when it is bare,
`--path` is required. A branch that a worktree already has checked out returns that worktree
(`reused: true`); another existing branch gets a worktree without moving. A branch that exists only as `origin/<branch>` (no local branch, no `--base`) gets a local branch tracking it, checked out in the worktree (`tracking` names the upstream). When neither a local branch nor `origin/<branch>` exists, `create` first asks `origin` (`git ls-remote --exit-code`, whose exit code 2 means no such branch, so git's message language does not matter) and then fetches that one branch (`git fetch origin +refs/heads/<branch>:refs/remotes/origin/<branch>`), so a branch pushed after the last fetch is found; if `origin` has no such branch the new branch starts as below, and any other fetch failure (offline, authentication) is an error naming the branch that suggests `--base`, instead of a silent new branch. A new branch starts at
`--base`, or at the newest default base: for each remote, its live default
(`git ls-remote --symref <remote> HEAD`) is fetched and its commit recorded at once, and the local
default branch (`origin/HEAD`'s, else `main`, else `master`) joins them. The candidate that
contains all the others wins; when they diverge, origin's fetched default is used and
`base.diverged` is true. `base.candidates` lists every candidate with its commit. No existing
branch or remote-tracking branch moves, and a remote that cannot be queried or fetched is an
error naming it. A `.gitmodules` file triggers `git submodule update --init --recursive`.

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
copy-on-write (`cp -cR` on macOS, `cp --reflink=auto -R` on Linux) instead of the node installs,
then runs `npm run prepare --if-present` when there is a `package-lock.json`; the other installs
(`uv`, `poetry`, `cargo`, `go`) still run and are reported, so `ok` is false when any step fails.
It reports `deps_from: "clone"`.

`agt worktree port [PATH] [--env]` gives a worktree a stable TCP port for its dev server. The
first call picks a free port between 10000 and 32767 (below the OS ephemeral ranges) and records
it in `$XDG_STATE_HOME/agents/worktrees/ports.json` under a lock, keyed by the worktree's absolute
path; later calls return it. A port recorded for another worktree is never handed out, and
entries whose path no longer exists are dropped on every write. `--env` prints `PORT=<n>`.

## Wait

`agt wait [--max S] [--every S] -- CHECK [ARGS...]` reruns `CHECK`, output discarded, every
`--every` (5) seconds until it succeeds, for at most `--max` (270) seconds. It replaces the
`agent-wait` script. Agents run it in the background so they wake within the prompt cache's five
minutes to look at a long job. Exit status: 0 the check passed (`agt wait: done after Ns`), 3
`--max` passed first, 2 usage.

## Issues

`agt issue` keeps GitHub and GitLab issues on your machine. Reads (`list`, `show`,
`search`, `status`, `find`) use the local copy; when a configured tracker's last
sync is older than five minutes they run `sync` for it first, and when the host
cannot be reached they read what is there (see [Auto-sync](#auto-sync-of-issues)).
`sync` is the only command that reads a host, and it stays read-only on it: every
write is queued in the local copy and goes out only on `push`, and only for
trackers with `push = true`. Nothing deletes an issue or a comment or edits
someone else's comment.

Trackers are listed in `$XDG_CONFIG_HOME/agt/config.toml` (`~/.config` when unset).
A missing file means no trackers; an unknown key or bad value is an error that
names the file and the key.

```toml
[[issues.tracker]]
forge = "github"            # or "gitlab"
host = "github.com"
repo = "owner/repo"         # GitLab: the full project path
scope = "involved"          # involved (default) | assigned | all
since_days = 90             # first sync only; later syncs list from the watermark
push = false                # writes stay local until this is true

[sync]
auto = true                 # false keeps every read local
max_age_secs = 300          # a read syncs when the last sync is older than this
```

`involved` is what you created, are assigned or (GitHub only) are mentioned in; GitLab
has no mentions filter, so there it is created or assigned. Credentials come from
`gh` and `glab`; the config holds none. Issues are stored per tracker under
`$XDG_STATE_HOME/agents/issues/<tracker>/<number>.json`, where the tracker is
`gh-<host>-<owner>-<repo>` or `gl-<host>-<group>-<project>`. A tracker is named by
that key or by `owner/repo` (`--tracker`, global to `agt issue`); an issue by
`owner/repo#12`, `#12`, `12` or `local-xxxxxx` (not yet created on the host).

### Auto-sync of issues

Before a read, each tracker in scope (`--tracker` narrows it) whose `synced_at` is
older than `max_age_secs`, or that never synced, syncs like `agt issue sync`. With
no tracker configured nothing happens and the output does not change. A failure
(`gh`/`glab` missing, not logged in, network, rate limit, or the calls together
taking more than 15 seconds, which kills the tool) prints one line,
`agt: sync skipped (<reason>); showing local data`, to stderr and reads the local
copy; stdout and the exit status are the local read's. A successful auto-sync
prints nothing. A failed sync is written to `.auto-sync.json` in the tracker's
directory and that tracker is not tried again for `max_age_secs` (no call, no
line); a success or an explicit `sync` clears it. A config file that cannot be
read prints the skip line with its error and the read goes on locally. A tracker
being synced by another auto-sync or pushed is skipped the same way instead of waited
for. `--offline` (global to `agt issue`),
`AGT_OFFLINE=1` and `auto = false` keep reads local. `agt issue sync` has no time
cap and keeps its errors and exit codes.

| Command | What it does |
| --- | --- |
| `sync` | Lists what changed since the last watermark (five minutes of overlap), fetches those issues with their comments and the PRs/MRs the host links, merges them in. Skips an issue whose `updated_at` is unchanged. Reports `listed`, `new`, `updated`, `unchanged`, `claimed`, `conflicts` per tracker. |
| `list [--state open\|closed\|all] [--label L] [--assignee A] [--author A] [--pending]` | Summaries; `pending` is whether anything is queued for the issue. |
| `show <key>` | The issue, its comments, links, notes, what is queued (`queued`), conflicts and errors. |
| `search <text>` | Case-insensitive match over title, body, comments and notes; `matched` says where. |
| `status` | Per tracker: counts, issues with queued changes, conflicts, errors, `push` and the last sync. |
| `find --pr <n\|url>` / `find --branch <name>` / `--title <text>` | Issues linked to a PR/MR; issues named by a number in the branch (`12-fix`, `feat/12-x`, `issue-12`, `gh-12`) or a PR/MR branch the host reported or `link --branch` recorded; issues a `#12`, `owner/repo#12` or issue URL in the title names. `via` says which. |
| `link <key> --pr\|--issue\|--review\|--task\|--branch <value>` | A local link, never sent. |
| `note <key> <text>` | A private note, never sent. |

Queued writes (`<text>` is read from stdin when it is `-`):

| Command | Queues |
| --- | --- |
| `comment <key> <text>` | A comment. |
| `close <key> [--reason completed\|not-planned]` / `reopen <key>` | A state change. A reason is GitHub only. |
| `assign <key> <login>...` / `unassign ...` | Assignees. GitLab takes one assignee here. |
| `label add\|remove <key> <label>...` | Labels. |
| `link-pr <key> --pr <n\|url>` | A comment `Linked: <url>` and a local link. Sent once. GitHub shows the mention on the PR's timeline, not the issue's; a later `sync` links the PR to the issue only when the PR says `Fixes #n`. GitLab lists the MR as related. |
| `create --title T [--body B] [--label L]... [--assignee A]...` | A new issue under a `local-` key. With several trackers pass `--tracker`. |
| `edit <key> [--title T] [--body B]` | A new title or body. |

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
reason. Tests that check local behavior run with `AGT_OFFLINE=1` (`Env::offline`) so reads do not call the fake forge. Tests use temporary
`HOME`, `XDG_STATE_HOME`, `XDG_CONFIG_HOME` and git repositories with
`GIT_CONFIG_GLOBAL`/`GIT_CONFIG_NOSYSTEM`. See `AGENTS.md` for the checks.
