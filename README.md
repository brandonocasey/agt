# agent-review

`agent-review` keeps PR/MR-style code review threads on your machine, one store
per repository. It records review runs and passes, each reviewer's model,
effort, harness and harness version, the findings reviewers kept or killed, and
line-anchored threads with replies and resolutions. It works without a PR or
MR, so Claude Code, Codex, Cursor or a person can pick up where a review left
off.

## Install

```sh
mise run install
```

This runs `cargo install --locked --path .` and puts `agent-review` in
`~/.cargo/bin`. Git 2.31 or newer must be on `PATH`.

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
  told apart.
- `--range <from>..<to>` reviews a range; `--range <rev>` reviews one commit.
  Each gets its own target.
- `--target <key>` uses a stored target by its key.

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

Reviewer ids are `r<n>`, assigned in order unless you pass `"id"`. A reviewer
continued in a later pass keeps its id; give only `"id"` and
`"starting_context"` and the rest is copied from its earlier pass. `--base
<rev>` sets the start of the reviewed delta (default: the merge-base for a full
pass, the previous pass's head otherwise).

```sh
agent-review finding add --run rv-7k2m --pass c1p0 --reviewer r1 < findings.json
agent-review pass end --run rv-7k2m --pass c1p0 --lock
agent-review run end --run rv-7k2m
```

`pass end --lock` freezes the pass for a blind review: `finding add` to it then
fails. `run end` closes the run, records commit patch-ids for every pass whose
reviewed tree now equals `HEAD`'s tree (see below), and prunes old targets of
this repository.

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
  stored reviewer. With no author flags the author is git `user.name`.
- `resolve`: `--resolution` is one of `fixed <sha>`, `not fixed: <reason>`,
  `stale` or `answered`. `--triage fixed|stale` resolves (`stale` implies the
  `stale` resolution; `fixed` needs `--resolution "fixed <sha>"`).
  `--triage valid|question` only records the triage and leaves the thread open;
  `question` marks it held. `--reopen` marks it unresolved.

Reading:

```sh
agent-review list --open
agent-review list --run rv-7k2m --format prompt   # fix-check and reveal input
agent-review show rv-7k2m.c1p0.r1.f1              # thread or killed finding
agent-review show rv-7k2m                          # run
agent-review show rv-7k2m.c1p0                     # pass
agent-review status
agent-review status --head 1a2b3c4
```

`list` prints threads (each with `current`: the anchor mapped to `HEAD`) and
killed findings. `--run` keeps only that run's findings.

`status` never writes: not to the store, not to the git index or object
database. It reports whether the head is reviewed, `reviews_at_head`, the
passes that cover it, each commit's reviewed state, `unreviewed_commits`, open
threads, `outdated_threads`, held questions and runs. Without `--head` on a
branch, `worktree` reports the same for uncommitted work.

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
- A target is deleted 90 days after its last change. `run end` and `prune`
  apply this to the current repository; `prune --all` to every repository.

## Store

Files live under `$XDG_STATE_HOME/agents/reviews/` (default
`~/.local/state/agents/reviews/`), in a directory per repository:
`<host>/<owner>/<repo>` from the `origin` URL, or
`local/<name>-<hash>` without one (the hash is from `git hash-object` of the git
common directory path). Each target is one JSON file, mode 0600, in a 0700
directory. Writes take a per-target lock (`.<key>.lock`), write a temporary
file in the same directory, fsync it and rename it over the target. Symbolic
links are refused.

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
      ]
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
        { "author": { "kind": "agent", "model": "claude-opus-5-5",
                      "effort": "xhigh", "harness": "claude-code",
                      "harness_version": "2.1.296", "run": "rv-7k2m",
                      "pass": "c1p0", "reviewer": "r1" },
          "body": "Retry loop drops the last response when ...",
          "created_at": "2026-10-16T12:20:00Z" },
        { "author": { "kind": "human", "login": "Brandon" },
          "body": "Fixed in 9e8d7c6.", "created_at": "2026-10-16T12:40:00Z" }
      ],
      "created_at": "2026-10-16T12:20:00Z",
      "updated_at": "2026-10-16T12:40:00Z"
    }
  ]
}
```

Anchors carry `commit` when the line numbers refer to a commit; for uncommitted
work `commit` is `null` and `tree` names the reviewed tree (`run end` fills in
`commit` once `HEAD` has that tree). Human thread ids are `h<n>`; finding ids are
`<run>.<pass>.<reviewer>.f<n>`.

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

`AGENT_REVIEW_NOW=<unix seconds>` replaces the current time. It exists only so
tests can check age pruning; do not set it otherwise. Tests use temporary
`HOME`, `XDG_STATE_HOME`, `XDG_CONFIG_HOME` and git repositories with
`GIT_CONFIG_GLOBAL`/`GIT_CONFIG_NOSYSTEM`. See `AGENTS.md` for the checks.
