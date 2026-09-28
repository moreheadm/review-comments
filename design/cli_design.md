# rv: CLI Design

Sep 28, 2026 · @Max

## Summary

`rv` is a Rust command-line tool for recording and reading code reviews stored as review branches, in the format defined by Review Commits: Git-Native Storage for Local Code Review. It is low-level plumbing, used both by people and by other programs: every command prints plain text by default and stable JSON with `--json`, and no command is interactive.

Reviews are written elsewhere, by an agent, an editor or a script, as JSONL files. `rv commit` validates one and records it on a named review branch. `rv show` lists a branch's comment threads and can place each comment in newer code, by running Git's own diff and shifting line numbers through its hunks.

`rv` does all of its repository work through the `git` command-line tool, and uses `jj` only to resolve jj revisions. Version 1 supports plain Git repositories and jj repositories colocated with Git.

## Goals and non-goals

`rv` is built to be called by programs, and to never guess what its caller meant.

| Goal | How rv meets it |
| --- | --- |
| Usable from programs | `--json` on every command, structured errors, stable exit codes, no prompts or pagers |
| Faithful to the storage spec | Every commit rv writes passes `rv check` and `git fsck --strict` |
| Explicit over implicit | The caller always names the branch, the reviewed commits, and the commit to place comments in |
| Line mapping matches Git | Hunks come from `git diff` itself, not from a reimplementation |
| One way into the repository | Every read and write goes through `git` commands, the same tool that computes diffs |
| Reusable from Rust | All logic lives in the `rv-core` library crate; the `rv` binary is a thin layer over it |

Out of scope for version 1: composing or editing reviews interactively, rich rendering beyond plain text, pushing or syncing review branches, jj repositories not colocated with Git, syntax-aware or similarity-ranked mapping, and new action types such as `relocate` or `resolve`.

## Conventions

Every command follows the same rules for output, errors, branches, revisions and identity.

### Output

By default, commands print plain text for people. With `--json`, which every command accepts, a command instead prints exactly one JSON object to stdout. Each JSON object has an `"rv": 1` field giving the schema version, which an incompatible change would bump.

The JSON is the stable contract. Text output may change between versions, so programs calling `rv` should always pass `--json`. Both forms are produced from the same result value, so they always carry the same information. Text is colored only when stdout is a terminal and `NO_COLOR` isn't set, and no command pages its output or prompts for input.

### Errors and exit codes

On failure, `rv` exits with a non-zero code and describes the error on stderr: as `error[<code>]: <message>` in text mode, or as one JSON object with `--json`. Exit codes are the same in both modes.

```json
{"rv":1,"error":{"code":"commit_not_reviewed","message":"line 2 comments on 9c3a7e2f, which is not a reviewed commit","details":{"line":2,"commit":"9c3a7e2f0b5d41c8e6a2f9d0b7c3e5a1f8d4b062"}}}
```

| Exit code | Meaning | Error codes |
| --- | --- | --- |
| 0 | Success | none |
| 1 | Unexpected internal error | `internal` |
| 2 | Bad command-line usage (clap's default code) | `usage` |
| 3 | Invalid review input | `invalid_action`, `duplicate_id`, `commit_not_reviewed`, `bad_anchor`, `unknown_target`, `not_a_review_commit`, `empty_commit` |
| 4 | Something named doesn't exist or is ambiguous | `branch_not_found`, `revision_not_found`, `ambiguous_revision` |
| 5 | Conflict with the branch's current state | `tip_moved`, `would_orphan` |
| 6 | Environment problem | `not_a_repository`, `git_failed`, `jj_failed` |

### Branches

`-b <name>` always means the ref `refs/reviews/<name>`. Every command that works on a branch requires it: there is no default branch and no environment-variable fallback. Names follow Git's ref-name rules (`git check-ref-format`).

### Revisions

`rv` accepts revisions in two places, `rv commit --reviewed` and `rv show --at`, and resolves them the same way in both, always to full commit IDs.

- **In a jj repository** (`jj root` succeeds), `rv` runs `jj log -r <rev> --no-graph --reversed -T 'commit_id ++ "\n"'`. Anything jj accepts works: commit IDs, change IDs, `@-`, and ranges such as `trunk()..@` or `A::C`. A divergent change ID is an error, never a guess.
- **In a plain Git repository**, `rv` runs `git rev-parse` for single revisions and `git rev-list --reverse` for `A..B` ranges.

Revisions are never stored. Review files always contain full commit IDs, chosen by whoever composed the review when the comment was written.

### Identity and time

The author identity comes from `--author "Name <email>"`, then the `RV_AUTHOR` environment variable, then Git's own identity (`git var GIT_AUTHOR_IDENT`). `rv commit` uses it for any action missing an `author`, and as the review commit's author and committer. All timestamps are RFC 3339 in UTC.

## Commands

`rv` has six commands, and `rv commit` is the only one that writes.

| Command | Purpose |
| --- | --- |
| `rv commit -b <branch> [<file> \| -]` | Validate a JSONL review and record it on a branch |
| `rv show -b <branch>` | List the branch's comment threads, optionally placed in another commit or limited to new activity |
| `rv log -b <branch>` | List the branch's review commits, newest first |
| `rv check -b <branch>` | Check every review commit on the branch against the storage spec |
| `rv branches` | List review branches with their tips |
| `rv id` | Print a new UUIDv7, for when a reply must reference a comment in the same review file |

Deleting a branch needs no command of its own: `git update-ref -d refs/reviews/<branch> <tip>` deletes it, and fails if the tip has moved.

## rv commit

`rv commit` records a composed review as a review commit. It fills in missing fields, checks the review against the branch, writes the commit, and moves the branch ref only if nobody else moved it first.

```
rv commit -b <branch> [<file> | -]
    --reviewed <rev>            repeatable; commit IDs, change IDs or revsets
    --review-parent <ref>       repeatable; a review branch name or review commit ID
    --create                    create the branch if it doesn't exist
    --expect-tip <commit|none>  fail unless the branch tip is exactly this
    --force                     allow a commit that orphans the current tip
    --author "Name <email>"
    -m <text>                   extra text for the commit message
```

### Steps

1. **Read the input**, JSONL from the file or stdin. Empty input is allowed only when the commit still records something (see Validation).
2. **Fill in missing fields.** An action without `id` gets a new UUIDv7, without `author` the configured identity, and without `created_at` the current time. All other fields, including ones rv doesn't recognize, are kept in their original order.
3. **Resolve `--reviewed`** to commit IDs (see Revisions), keeping flag order, expanding ranges oldest first, and dropping duplicates. rv never infers reviewed commits from the review file.
4. **Choose the review parents:** the `--review-parent` values if given; otherwise the branch's current tip; for a new branch, none.
5. **Validate** the review against the checks below.
6. **Write the review file and trees.** `git hash-object -w` stores the input as a blob. Two `git mktree` calls then build the `reviews/` tree, holding the review parents' entries plus `<review-id>.jsonl`, and the root tree above it. rv generates the review ID; empty input adds no file.
7. **Write the commit.** Its parents are the review parents, then the reviewed commits. The `review-parents` header lists the review parents, and is written as the key plus one space when there are none. The message is `Review <review-id>`, followed by any `-m` text. `git commit-tree` can't write custom headers, so rv assembles the commit object itself and stores it with `git hash-object -t commit -w`.
8. **Move the ref** with `git update-ref refs/reviews/<branch> <new> <old>`, which fails if the tip changed after step 4 (`<old>` is all zeros for a new branch). If that happens and the caller relied on the default parents without `--expect-tip`, rv starts again from step 4, up to 5 times; adding a file can't conflict. Otherwise it fails with `tip_moved`.

### Branch states

| Branch exists? | `--create` | `--review-parent` | Result |
| --- | --- | --- | --- |
| yes | either | none | Commit on the tip; retried automatically if the tip moves |
| yes | either | given | Commit on the listed parents, which must include the current tip or a review commit descended from it; otherwise `would_orphan`, unless `--force` is given |
| no | no | any | `branch_not_found`, so a typo can't create a branch |
| no | yes | none | Root review commit, which starts the branch |
| no | yes | given | New branch that forks the listed review history |

The orphan check follows `review-parents` headers from the listed parents, so it never walks into code history. To merge branch `other` into `main`, run `rv commit -b main --review-parent main --review-parent other` with empty input.

### Validation

| Check | Error code |
| --- | --- |
| Every line is a JSON object with a known `type` and the fields that type requires | `invalid_action` |
| Every given `id` is a UUIDv7 not already used on the branch or elsewhere in the input | `duplicate_id` |
| Every comment's `commit` is one of the reviewed commits | `commit_not_reviewed` |
| Every anchor's path exists in its `commit`, and 1 ≤ `start_line` ≤ `end_line` ≤ the file's line count | `bad_anchor` |
| Every `in_reply_to` and `target` names a comment or reply on the branch or earlier in the input | `unknown_target` |
| Every review parent is a review commit | `not_a_review_commit` |
| The commit records something: non-empty input, two or more review parents, or at least one reviewed commit | `empty_commit` |
| With `--review-parent`, the current tip stays reachable | `would_orphan` |
| With `--expect-tip`, the current tip matches it | `tip_moved` |

Readers must tolerate unknown action types, but rv refuses to write them, to catch typos. The `commit_not_reviewed` check also catches a race. Suppose a caller displays commit B1, the agent then rewrites it as B2, and the caller passes B1's change ID to `--reviewed`. The change ID now resolves to B2, so comments on B1 fail this check instead of being recorded against code nobody reviewed.

### Output

In text mode, rv prints one summary line:

```
recorded review 01a0e3f0 on config-loader as f2c8a06d: 2 actions, reviewed c4e90b12
```

With `--json`, it prints the details. Commit IDs are shortened here for readability; the JSON always has them in full.

```json
{"rv":1,"branch":"config-loader","commit":"f2c8a06d","previous_tip":"8d2a41f7","review_id":"01a0e3f0-6a10-7b95-9d24-1e3f5a7c9b02","review_parents":["8d2a41f7"],"reviewed":["c4e90b12"],"actions":[{"id":"01a0e3f0-6b3c-7f61-a8e5-2d4c6b8a0e13","type":"delete"},{"id":"01a0e3f0-7c88-7a2d-b0f4-3e5d7c9a1b35","type":"comment"}],"attempts":1}
```

## Reading commands

The reading commands build a branch's state from its tip's tree; `rv log` and `rv show --since` also walk the review history.

### rv show

```
rv show -b <branch> [--at <rev>] [--path <path>] [--since <review-commit>] [--include-deleted]
```

`rv show` lists every comment thread on the branch. Each comment starts a thread, each reply nests under the comment or reply it answers, and siblings are ordered by `created_at`, then `id`.

- `--at <rev>` places each line comment in that commit, adding a `mapped` location (see Line mapping). Without it, comments are shown where they were made.
- `--path <path>` keeps only threads whose original or mapped path is `<path>`, for callers that display one file at a time.
- `--since <review-commit>` keeps only threads with activity after that review commit, and marks the new actions `"new": true`. "After" means review files in the tip's tree that aren't in that commit's tree. To poll for new feedback, pass the `tip` from the previous call.
- Deleted comments and replies keep their place with `"deleted": true` and no body, so callers can show a placeholder; `--include-deleted` adds their bodies back.

Each anchor includes its commit's jj change ID when the commit has jj's `change-id` header, so callers can group comments by change. Top-level comments have only a commit, and are never mapped.

In text mode, each thread is printed under its location:

```
config-loader at f2c8a06d, comments placed in 2b7f0e4c

src/config.rs:14-20 (made on 5d1e8a0c at lines 12-18; exact)
  Dana, 2026-09-27 14:06
    This hand-rolled parser misses quoted values. Use the `toml` crate?
  coding-agent, 2026-09-27 15:12
    Switched to the `toml` crate in 2b7f0e4c.
```

With `--json`, the same result is:

```json
{"rv":1,"branch":"config-loader","tip":"f2c8a06d","at":"2b7f0e4c",
 "threads":[{"id":"01a0e21d-e2a8-72f4-a1c3-5e7f9b0d2c46","author":{"name":"Dana"},"created_at":"2026-09-27T14:06:10Z",
   "body":"This hand-rolled parser misses quoted values. Use the `toml` crate?",
   "anchor":{"commit":"5d1e8a0c","change_id":"kmnpqrstkmnpqrstkmnpqrstkmnpqrst","path":"src/config.rs","start_line":12,"end_line":18},
   "mapped":{"commit":"2b7f0e4c","path":"src/config.rs","start_line":14,"end_line":20,"status":"exact"},
   "replies":[{"id":"01a0e25b-1d02-7c4a-8f13-6b2d0e5a7c91","author":{"name":"coding-agent"},"created_at":"2026-09-27T15:12:40Z",
     "body":"Switched to the `toml` crate in 2b7f0e4c.","replies":[]}]}],
 "orphans":[]}
```

### rv log

`rv log -b <branch>` follows `review-parents` from the tip and lists every review commit, newest first, with its author, time, review parents, reviewed commits, and the reviews and actions it added.

### rv check

`rv check -b <branch>` checks every review commit on the branch against the storage spec's rules. In text mode it lists one violation per line, or reports that the branch is valid; with `--json` it returns `{"rv":1,"ok":false,"violations":[{"rule":"V5","commit":"…","detail":"…"}]}`. It exits 0 even when it finds violations, so callers can tell an invalid branch apart from a check that couldn't run.

### rv branches

`rv branches` lists every ref under `refs/reviews/` with its tip and number of reviews.

### rv id

`rv id` prints a new UUIDv7, or `{"rv":1,"id":"<uuidv7>"}` with `--json`. A composer needs it only when one action must reference another before either is committed, such as a reply to a comment in the same review file; otherwise `rv commit` assigns IDs itself.

## Line mapping

`rv show --at` places a comment in a later commit by diffing the file between the comment's commit and the target with `git diff`, then shifting the comment's line numbers through the hunks. GitHub, GitLab and Gerrit work the same way; version 1 adds no similarity heuristics on top.

### Steps

1. **Find the file in the target commit.** If the comment's path exists there, compare the two versions of that path. If not, look for a rename with `git diff-tree -r -z -M --name-status <comment-commit> <target>`; if nothing was renamed from that path, the status is `file_deleted`.
2. **Skip unchanged files.** If both versions are the same blob, the comment keeps its lines with status `exact`, and no diff runs.
3. **Diff the two blobs** with `git diff --no-color --no-ext-diff --no-textconv --histogram -U0 <old-blob> <new-blob>`. The flags fix the algorithm and turn off external diff tools and text conversion, whatever the user's Git configuration says. `-U0` leaves out context lines, so a hunk contains only lines that actually changed.
4. **Read the hunk headers**, the lines of the form `@@ -a[,b] +c[,d] @@`, where a missing count means 1. When a count is 0, Git gives the line *before* the empty range as the start, so rv adds 1. Each hunk then says plainly: old lines `a` to `a+b-1` became new lines `c` to `c+d-1`.
5. **Map the comment's first and last lines** with the function below, then classify the result.

```
map_line(L, hunks) -> Line(n) | Inside(hunk):
    offset = 0
    for h in hunks, top to bottom:
        if L < h.old_start:                 return Line(L + offset)
        if L < h.old_start + h.old_count:   return Inside(h)
        offset = (h.new_start + h.new_count) - (h.old_start + h.old_count)
    return Line(L + offset)
```

### Classifying a comment's range

A hunk touches a comment's range, lines S to E, if it replaces or deletes any of those lines, or inserts new lines between two of them. For an insertion before old line p, that means S < p ≤ E.

| Status | When | Mapped range |
| --- | --- | --- |
| `exact` | No hunk touches the range | Both ends shifted by the same amount |
| `changed` | A hunk touches the range, and some lines remain | From the mapped first line to the mapped last line |
| `deleted` | Every line in the range was deleted | none |
| `file_deleted` | The file was deleted and not renamed | none |
| `binary` | The file is binary and changed, so Git reports no hunks | none |

When the first or last line falls inside a hunk, it maps to the first or last of the lines that replaced it. A deletion replaces lines with nothing, so a range whose lines were all deleted maps to an empty range, which is the `deleted` status.

For example, `@@ -9,0 +10 @@` inserts one line before old line 10. A comment on line 20 maps `exact` to line 21, and a comment on lines 15 to 18 maps `exact` to 16 to 19. A comment on lines 8 to 12 maps `changed` to 8 to 13, because the new line lands inside it.

### Caching

Blobs never change, so the hunks for a pair of blobs can be cached forever, keyed by the two blob IDs; rename lookups are keyed by (comment commit, target commit, path). The cache lives under `$GIT_DIR/rv/cache/` and can be deleted at any time. Within one call, each pair of blobs is diffed at most once, however many comments it has.

## Rust architecture

&#91;embedded content: rv architecture · CLI, 4 core modules, git and jj\]

The `rv` binary only parses arguments and prints results. All logic lives in the `rv-core` library, which reaches the repository only through the `git` and `jj` command-line tools.

### Crates and modules

| Crate or module | Responsibility |
| --- | --- |
| `rv` (binary) | Parse arguments with clap, and print each result as text or JSON |
| `rv-core` (library) | The modules below, plus the shared `model`, `error` and `output` types |
| `git` | Run Git commands and parse their output; keep one `git cat-file --batch` process open per invocation for reading objects |
| `resolve` | Detect jj, resolve revisions and ranges to commit IDs, and read `change-id` headers |
| `store` | Read a branch's reviews from its tip's tree; write blobs, trees and commits; move refs with compare-and-swap |
| `validate` | The checks `rv commit` runs, and the storage spec's rules for `rv check` |
| `map` | Rename lookup, hunk parsing, line mapping and the hunk cache |

### Git commands

| rv needs to | Git command |
| --- | --- |
| Read a branch tip | `git rev-parse --verify refs/reviews/<branch>` |
| List branches | `git for-each-ref refs/reviews/` |
| Read commits, trees and blobs | `git cat-file --batch`, one process streaming any number of objects |
| Store a review file | `git hash-object -w --stdin` |
| Build trees | `git mktree`, once for `reviews/` and once for the root |
| Store a review commit | `git hash-object -t commit -w --stdin`, since `git commit-tree` can't write custom headers |
| Move a ref safely | `git update-ref <ref> <new> <old>` |
| Find renames | `git diff-tree -r -z -M --name-status` |
| Diff two blobs | `git diff --histogram -U0`, with the flags from Line mapping |
| Get the author identity | `git var GIT_AUTHOR_IDENT` |

A typical `rv commit` starts around ten processes, and `rv show` starts one `cat-file` process plus one diff per changed file. Each costs a few milliseconds.

### Dependencies

| Crate | Used for |
| --- | --- |
| `clap` | Argument parsing, derive API |
| `serde`, `serde_json` | Actions and output; the `preserve_order` feature keeps unknown fields in their original order |
| `uuid` | UUIDv7 generation, `v7` feature |
| `jiff` | RFC 3339 timestamps |
| `thiserror` | Error types that carry their error code and exit code |

### Key types

A sketch of the mapping types; names may change during implementation. `Oid` is a hex object ID.

```rust
/// A normalized hunk: old lines [old_start, old_start + old_count)
/// became new lines [new_start, new_start + new_count).
pub struct Hunk { pub old_start: u32, pub old_count: u32, pub new_start: u32, pub new_count: u32 }

pub enum BlobDiff { Hunks(Vec<Hunk>), Binary }

/// Where hunks come from. Version 1 runs `git diff`.
pub trait DiffEngine {
    fn diff(&self, old: &Oid, new: &Oid) -> Result<BlobDiff, Error>;
}

pub enum MapStatus { Exact, Changed, Deleted, FileDeleted, Binary }

pub struct Mapped {
    pub commit: Oid,
    pub path: Option<String>,
    pub lines: Option<(u32, u32)>,
    pub status: MapStatus,
}
```

`DiffEngine` keeps version 1 faithful to Git while leaving room for speed. If one `git diff` process per file proves too slow, an in-process engine such as imara-diff could implement the trait instead, at the cost of hunks that may differ slightly from Git's.

## Choices the storage spec leaves open

The storage spec leaves three things unspecified, and rv makes one fixed choice for each, so every commit it writes follows the same pattern. Other writers may choose differently, and `rv check` accepts any choice the spec allows.

| Topic | Storage spec | What rv does |
| --- | --- | --- |
| Files per commit | Any number, including none | One file per `rv commit`, or none when the input is empty (a merge, or a record of reviewed commits only) |
| Parent order | Unspecified | Review parents first, then reviewed commits in the order given to `--reviewed`, ranges oldest first |
| Reviewed commits and comments | Not tied together | Every comment's `commit` must be a reviewed commit (`commit_not_reviewed`); reviewed commits without comments are allowed |

The third choice is deliberately stricter than the spec. It keeps every commented-on commit reachable from the branch, and it catches the rewrite race described under Validation.

## Testing

Tests run against throwaway repositories created per test, using the real `git` binary; jj tests are skipped when `jj` isn't installed.

| Area | Test |
| --- | --- |
| Line mapping | Property tests: generate a file and a random edit script, apply it, and check that every line the script left untouched maps `exact` to the position the script predicts |
| Hunk parsing | Table tests for every header shape, including zero counts, missing counts, and "No newline at end of file" markers |
| Git output parsing | Paths with spaces, quotes, newlines and non-ASCII characters survive `-z` and `cat-file --batch` parsing |
| Format compatibility | Every commit rv writes passes `git fsck --strict` and `rv check`; a root commit's header bytes are exactly `review-parents` plus one space |
| Concurrency | Two writers race on one branch; both reviews land, neither is orphaned, and explicit parents fail with `tip_moved` instead of retrying |
| Validation | One failing fixture per error code, checking the code, the exit code and the reported input line |
| Revision resolution | Change IDs, ranges and divergent changes resolved through jj, and the same revisions through plain Git |
| Output | Snapshot tests of each command's JSON and text output, so any change to either is deliberate |

## Open questions and risks

None of these block version 1.

- [ ] **Process cost:** benchmark `rv show --at --path` on a large file with a cold cache. If it's too slow, implement `DiffEngine` in-process.
- [ ] **Mapping uncommitted comments:** `rv map` was cut from version 1. Add it back if a caller needs to place comments that aren't committed yet, such as drafts.
- [ ] **jj repositories not colocated with Git:** rv needs to find the backing Git store before it can support them.
- [ ] **Mid-line anchors:** optional start and end columns in `anchor`, which mapping would carry through unchanged lines.
- [ ] **Explicit relocation:** a `relocate` action, written by whoever moved the code, that takes precedence over the computed mapping.
- [ ] **Smarter mapping for `changed` comments:** CodeMapper-style candidate ranking, behind a flag, if hunk-based mapping proves too coarse.

