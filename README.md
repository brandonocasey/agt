# agent-review

`agent-review` keeps PR/MR-style code review threads on your machine, one store
per repository. It records review runs and passes, each reviewer's model,
effort, harness and harness version, the findings reviewers kept or killed, and
line-anchored threads with replies and resolutions. It works without a PR or
MR, so Claude Code, Codex, Cursor or a person can pick up where a review left
off. When a GitHub PR or GitLab MR exists, `sync` imports all of its threads
and comments.

## Install

```sh
mise run install
```

This runs `cargo install --locked --path .` and puts `agent-review` in
`~/.cargo/bin`. Git 2.31 or newer must be on `PATH`. `sync` also needs an
authenticated `gh` (GitHub) or `glab` (GitLab).

## Output and errors

Every command prints JSON on stdout, except `list --format prompt`, which prints
plain text. On failure a command prints `agent-review: <message>` on stderr and
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
- `--target <key>` uses a stored target by its key. For a branch target whose
  branch is not in this repository, the head is the synced PR/MR head; with no
  PR/MR it is an error, never the current `HEAD`. With a `gh-<n>` or `gl-<iid>`
  target, `sync` and `publish` use that PR/MR without `--pr`, also on a
  detached `HEAD`.

On the default branch itself, the root commit names the target and passes cover
only uncommitted work; use `--range` to review commits there.

## Commands

Start a run, then passes inside it. A `full` pass opens a new cycle (`c1p0`,
`c2p0`); `fix-check` and `reveal` passes continue the current cycle (`c1p1`,
`c1p2`).

```sh
agent-review run start
# {"run": "rv-7k2m", "target": "branch-feat%2Flogin-1a2b..."}

agent-review pass start --run rv-7k2m --kind full --reviewers - <<'EOF'
[{"model": "claude-opus-5-5", "effort": "xhigh", "harness": "claude-code",
  "harness_version": "2.1.296", "starting_context": "fresh"}]
EOF
```

Reviewer ids are `r<n>`, assigned in order unless you pass `"id"`. Two ids are
reserved: `manual` for the main session's own pass and `fix` for the fixer,
whose replies feed the final record's `Fixed by:` line; give them model, effort,
harness and version like any reviewer. A reviewer continued in a later pass
keeps its id; give only `"id"` and `"starting_context"` and the rest is copied
from its earlier pass. `--base <rev>` sets the start of the reviewed delta
(default: the merge-base for a full pass, the previous pass's head otherwise).

```sh
agent-review finding add --run rv-7k2m --pass c1p0 --reviewer r1 < findings.json
agent-review pass end --run rv-7k2m --pass c1p0 --lock
agent-review finding kill rv-7k2m.c1p0.r1.f2 --reason "The caller checks it."
agent-review run end --run rv-7k2m
agent-review run end --run rv-7k2m --reveal-agree 2 --reveal-disagree 0
```

`pass end --lock` freezes the pass for a blind review: `finding add` to it then
fails. `finding kill` moves a surviving finding to its pass's killed list, also
on a locked pass; a finding that is posted or being posted to the PR/MR is
resolved as `stale` instead. `run end` closes the run, records commit patch-ids for every pass whose
reviewed tree now equals `HEAD`'s tree (see below), records `HEAD` as the run's
`repaired_head` (commits after the last pass up to it are the run's `repair`),
stores the reveal counts for the final record, and prunes old targets of this
repository.

Threads for humans and agents:

```sh
agent-review comment --body "Why 3 retries?" --path src/net.rs --line 42
agent-review comment --body - --type question < note.md   # general comment
agent-review reply h1 --body "Matches the API limit."
agent-review reply rv-7k2m.c1p0.r1.f1 --body "Fixed in 9e8d7c6" \
  --run rv-7k2m --pass c1p0 --reviewer fix --model claude-opus-5-5 --harness claude-code
agent-review resolve h1 --resolution answered
agent-review resolve rv-7k2m.c1p0.r1.f1 --triage fixed --resolution "fixed 9e8d7c6"
agent-review resolve rv-7k2m.c1p0.r1.f2 --triage question     # held, stays open
agent-review resolve rv-7k2m.c1p0.r1.f1 --reopen --resolution "not fixed: still loses data"
```

- `comment` options: `--type bug|question|requirement|nit|comment` (default
  `comment`), `--severity high|medium|low`, `--path`, `--line`, `--old-line`,
  `--side new|old`, `--start-line`, `--commit <rev>`. Line numbers refer to
  `--commit`, else `HEAD`, else the uncommitted work tree when it is dirty.
- Author: `--author <login>` for a human, or agent flags `--model`,
  `--effort`, `--harness`, `--harness-version`, `--run`, `--pass`,
  `--reviewer`. `--reviewer` with `--run` and `--pass` fills the rest from the
  stored reviewer. With no author flags the author is the forge login that
  `sync` last authenticated as for this repository, else git `user.name`,
  which the next `sync` (or `publish`) replaces with the forge login.
- `resolve`: `--resolution` is one of `fixed <sha>`, `not fixed: <reason>`,
  `stale` or `answered`. `--triage fixed|stale` resolves (`stale` implies the
  `stale` resolution; `fixed` needs `--resolution "fixed <sha>"`).
  `--triage valid|question` only records the triage and leaves the thread open;
  `question` marks it held. `--reopen` marks it unresolved.

Reading:

```sh
agent-review list --open
agent-review list --run rv-7k2m --format prompt   # fix-check and reveal input
agent-review list --run rv-7k2m --cycle 2         # one cycle's findings
agent-review show rv-7k2m.c1p0.r1.f1              # thread or killed finding
agent-review show rv-7k2m                          # run
agent-review show rv-7k2m.c1p0                     # pass
agent-review status
agent-review status --head 1a2b3c4
```

`list` prints threads (each with `current`: the anchor mapped to `HEAD`, or
for imported threads the host's path, line and outdated state) and killed
findings. `--open` keeps the threads `status` counts as open. `--run` keeps only
that run's findings, and `--cycle <n>` (with `--run`) only that cycle's threads
and killed findings. `--format prompt` prints the text only of comments written
locally or by the authenticated forge user; every other comment shows as
`(comment by another author, text hidden)`, so reviewers never read other
people's comments.

`status` never writes: not to the store, not to the git index or object
database. It reports whether the head is reviewed, `reviews_at_head`, the
passes that cover it, each commit's reviewed state, `unreviewed_commits`, open
threads, `outdated_threads`, held questions, runs, the linked PR/MR and the
trusted imported review records for the head. Without `--head` on a branch,
`worktree` reports the same for uncommitted work. Open threads are unresolved
threads still on the host, minus other people's general comments and review
bodies, which have no resolved state there; your own general comments stay
open until you resolve them.

Sync with a PR or MR:

```sh
agent-review sync                     # the PR/MR whose head is this branch
agent-review sync --pr 1235
agent-review sync --pr https://gitlab.example.com/group/proj/-/merge_requests/3
```

`sync` is read-only on the forge: it sends only `GET` requests and GraphQL
queries. See [Sync](#sync).

Publish to an own PR or MR:

```sh
agent-review publish --dry-run        # print every payload; writes nothing
agent-review publish
agent-review publish --pr 1235 --not-own
```

`publish` posts what changed locally since the last publish. See
[Publish](#publish).

Cleanup:

```sh
agent-review prune --dry-run
agent-review prune --all
```

## Reviewed, counting and pruning rules

- A pass records its `base..head`, the reviewed tree id, each commit's
  patch-id, the whole range's patch-id (target merge-base to reviewed tree) and
  the head commit's patch-id. For uncommitted work, the tree id comes from a
  copy of the real index in a temporary `GIT_INDEX_FILE` after `git add -A`
  and `git write-tree`, so tracked files that match `.gitignore` stay in the
  tree and the real index is untouched.
- An ended pass covers a head when the head's tree equals the pass tree, the
  range's patch-id equals the pass's whole-range patch-id, or the head commit's
  patch-id equals the pass's head or whole-range patch-id. A commit is
  reviewed when its patch-id, tree or sha matches any ended pass. Squashes,
  fixups and plain rebases stay reviewed; a conflict resolution changes the
  patch and shows as unreviewed. Merge commits are not listed.
- `run end` adds commit patch-ids to passes whose tree now equals `HEAD`'s
  tree, so reviewed uncommitted work stays reviewed after it is committed and
  later rebased.
- `reviews_at_head` counts covering passes that are full reviews, plus fix
  checks that found no major finding (a `bug` of `high` or `medium` severity).
  Parallel reviewers in one pass count once.
- Anchors map through `git diff -U0 -M <anchor commit or tree> HEAD`. A
  new-side line inside a changed hunk, or a deleted file, is outdated; other
  lines shift and follow renames. An old-side anchor is outdated once its file
  changes. Anchors on uncommitted work name a tree that no ref points to;
  `agent-review` never writes refs to pin it, so once `git gc` prunes it (by
  default about two weeks later, unless `run end` has since moved the anchor
  to a commit) the anchor counts as outdated.
- A target is deleted 90 days after its PR/MR closed or merged, or, with no
  closed PR/MR, 90 days after its last change. `sync`, `run end` and `prune`
  apply this to the current repository; `prune --all` to every repository.
- Each trusted `review-record v2` comment whose `head` is the evaluated head
  adds its `reviews-at-head` to `reviews_at_head` and makes the head reviewed.
  Records of this target's own runs are skipped; their passes already count.
- A run's `repair` commits (after its last pass, up to `HEAD` at `run end`)
  show as reviewed with `"match": "repair"` in `status` commits (`"pass"` for
  a pass match), but never add to `reviews_at_head`.

## Sync

- Forge: `github.com` uses `gh`; any other host uses `glab` with
  `GITLAB_HOST=<host>`, so self-hosted GitLab works. The host and repository
  come from `--pr <url>` or the `origin` remote.
- PR/MR lookup: the PR whose head is `<origin owner>:<branch>` (GitHub) or the
  MR whose source branch is the current branch (GitLab), first in `origin`,
  then in the repository `origin` was forked from. Open ones win, then the
  newest. On GitLab, only MRs whose source project is `origin` count, so a
  fork's MR with the same branch name is skipped.
- The target is renamed to `gh-<number>` or `gl-<iid>` under both locks, keeping
  its runs and threads; later commands on the branch find it by reflog birth or
  shared patch-ids. `sync` also adopts patch-ids like `run end` and prunes.
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
  comment, then by a trusted `<!-- agent-review id=<local id> -->` marker that
  `publish` adds to every body. Unmatched threads are imported as
  `gh-t<comment id>` (review thread), `gh-r<review id>` (review body),
  `gh-c<comment id>` (general comment) or `gl-n<note id>`, with `-2`, `-3`
  added when that id is taken; an id is never reused, including a killed
  finding's. A GitLab discussion keeps its thread when a reply turns an
  individual note into a discussion. `publish`'s own review bodies are not
  imported.
- Trust: a marker or record counts only when its comment's author is the
  authenticated user, `editor` is null or that user, and (for a marker) it is
  in the thread's first comment. Untrusted ones import as plain comments.
  Trusted records are stored under `records`; a trusted general comment ending
  in `Re: <thread id>` after `Fixed in <sha>` (or after an `agent-review`
  marker) answers that thread: it is added as a reply and, when the thread is
  not resolvable, resolves it as `fixed <sha>` (a `Fixed in` answer) or
  reopens it (any other answer).
- A draft note with a marker for a local comment marks it `posting` with its
  `draft_id`, so a later publish publishes it; a `draft_id` whose draft is gone
  is cleared.
- Threads and comments the host no longer returns are kept and flagged
  `deleted_remote: true` with `deleted_seen_at`; they are never dropped and
  are not open. The flag clears if they come back.
- The authenticated login is cached in `.account.json` in the repository's
  store directory.

## Publish

- Only on a PR/MR authored by the authenticated login, unless `--not-own`.
  `publish` syncs first, then posts in this order: new threads, sync, replies,
  resolves and reopens, final records, then a last sync to fill in ids.
- Posted: new surviving findings of ended passes (a locked pass's findings wait
  until a later `reveal` pass of the same cycle has ended) and local comment
  threads, new replies, resolve and reopen changes, and the final record of each
  ended run not posted yet. Never posted: killed findings, imported comments,
  and local comments by another human author (listed under `skipped`).
- Every body ends with a signature (agent: `<model> <effort> <harness>
  <run>.<pass>.<reviewer>`, plus ` post-reveal`; human: the login) and
  `<!-- agent-review id=<local id> -->`. Finding threads also carry
  `<!-- review-finding v1 id=<finding id> sha=<reviewed sha> -->`.
- GitHub: in-diff comments go out as one `COMMENT` review per reviewed commit
  (`POST repos/<repo>/pulls/<n>/reviews`; comment ids are read back from the
  review). Anything else is an issue comment, with an out-of-diff finding's
  location on its first line. Replies and resolves use the GraphQL
  `addPullRequestReviewThreadReply`, `resolveReviewThread` and
  `unresolveReviewThread` mutations; a reply to a general comment is a new
  issue comment ending in `Re: <thread id>`.
- GitLab: in-diff comments are draft notes at the latest MR diff, each
  published on its own (never `bulk_publish`). Anything else is a discussion;
  replies are discussion notes; resolves are `PUT .../discussions/<id>`. The
  final record is a plain note, so it never blocks merging.
- In-diff means the line is in a hunk of `git diff -U3 -M` between the PR/MR
  base and the reviewed commit (GitHub) or MR head (GitLab, after mapping the
  line to it); when the commits are not local it is out of diff.
- The final record follows the review skill: a header with cycles, found,
  fixed and open counts, a `| Pass | Reviewed | Starting context | Reviewer |`
  table, `Reveal:` and `Fixed by:` lines when known, and
  `<!-- review-record v2 id=<run> head=<repaired head> reviews-at-head=<N> open=<O> -->`.
- Crash safety: each item is saved as `posting` with an attempt id before its
  write and gets its remote id right after. A rerun syncs first and matches
  `posting` items by their `agent-review` marker before retrying, and
  publishes a GitLab draft that was created but not published.
- `--dry-run` syncs in memory and prints every payload, including the final
  record; it writes nothing to the store, the repository or the forge.

## Store

Files live under `$XDG_STATE_HOME/agents/reviews/` (default
`~/.local/state/agents/reviews/`), in a directory per repository:
`<host>/<owner>/<repo>` from the `origin` URL, or
`local/<name>-<hash>` without one (the hash is from `git hash-object` of the git
common directory path). Each target is one JSON file, mode 0600, in a 0700
directory. Writes take a per-target lock (`.<key>.lock`), write a temporary
file in the same directory, fsync it and rename it over the target. Symbolic
links are refused. After `sync`, the key is `gh-<number>` or `gl-<iid>`.

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
    "author": "brandonocasey", "login": "brandonocasey", "synced_at": "2026-10-16T13:00:00Z"
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
      "record": { "post": "posted", "attempt": "at-x1y2", "head": "9e8d7c...",
                  "remote_id": "2002", "url": "https://github.com/...#issuecomment-2002" }
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
          "deleted_remote": false, "deleted_seen_at": null,
          "remote": { "id": "1001", "url": "https://github.com/...#discussion_r1001",
                      "login": "brandonocasey", "is_me": true, "editor": null,
                      "review_id": "5001" } },
        { "id": "rv-7k2m.c1p0.r1.f1/c2",
          "author": { "kind": "human", "login": "Brandon", "git_name": false },
          "body": "Fixed in 9e8d7c6.", "created_at": "2026-10-16T12:40:00Z",
          "post": "pending", "remote": null, "draft_id": null, "attempt": null,
          "deleted_remote": false, "deleted_seen_at": null }
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
the try; `draft_id` a GitLab draft), and `posted` once the forge has it; `remote` and the target's `review` and `records` are filled by
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
| `start_line` | integer | no | First line of a multi-line range ending at `line` |
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

## Testing

`AGENT_REVIEW_GH_BIN` and `AGENT_REVIEW_GLAB_BIN` replace the `gh` and `glab`
executables; tests point both at `test/fixtures/fake-forge.sh`, which replays
canned responses and fails on any other call. `AGENT_REVIEW_NOW=<unix seconds>` replaces the current time. It exists only so
tests can check age pruning; do not set it otherwise. Tests use temporary
`HOME`, `XDG_STATE_HOME`, `XDG_CONFIG_HOME` and git repositories with
`GIT_CONFIG_GLOBAL`/`GIT_CONFIG_NOSYSTEM`. See `AGENTS.md` for the checks.
