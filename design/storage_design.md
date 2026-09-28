# Review Commits: Git-Native Storage for Local Code Review

Sep 27, 2026 · @Max

## Summary

Review data lives inside the repository as ordinary Git commits under `refs/reviews/`, and each review commit lists every commit it reviews as a parent. That one choice keeps reviewed code reachable through rewrites and garbage collection, and makes review history walkable with plain Git.

The primary use is local review of an AI coding agent's work. A human or another agent comments on the agent's commits; the agent reads the comments, replies, and revises. Everything happens in one local repository, with no server and no multi-user sync.

The format is append-only. Each review is a JSONL file of actions (comments, line comments, replies, deletions), and later review commits only add files. Every review is filed on a named review branch, with no default, and a branch's newest review commit holds its complete review state.

The core idea comes from the native backend of [Flirt](https://blog.buenzli.dev/flirt-native-backend/), an unreleased review tool whose review ref points at a commit that lists every reviewed submission head as a parent.

## Goals and non-goals

The format optimizes for one writer at a time in one local repository, with data that humans and agents can both read using plain Git.

| Goal | How the format meets it |
| --- | --- |
| Reviewed code never disappears | Reviewed commits are parents of review commits, so review branches keep them reachable |
| History is plain Git | Review commits form a DAG readable with `git log` and `git cat-file` |
| Writes never conflict | Files are only ever added, never edited, and every action has a unique ID |
| Current state is cheap to read | The newest review commit's tree contains every review |
| Agents parse it trivially | One JSON object per line, three action types |

Out of scope for version 1: syncing reviews between people or machines, resolving semantic conflicts between concurrent writers, permissions, and thread resolution, verdicts, edits or comments on deleted lines. The format leaves room for these; see Open questions.

## Terminology

| Term | Meaning |
| --- | --- |
| Reviewed commit | A commit whose code is under review, usually one the agent wrote |
| Review action | One atomic event with a unique ID: a comment, a reply or a deletion |
| Review | The actions one author made in one sitting, stored as one JSONL file |
| Review commit | A Git commit that adds any number of reviews to a branch; its tree holds all reviews so far |
| Review parent | A review commit that is a parent of another review commit |
| Review branch | A named line of review history: the ref `refs/reviews/<branch>` and the review commits it reaches |
| Review DAG | The graph of review commits linked by their review parents |

## Overview

&#91;embedded content: review DAG · 3 review commits, 5 code commits\]

R1 reviews the agent's first two commits, R2 only adds the agent's replies, and R3 reviews the rewritten B2. Because every reviewed commit is a parent, the branch's ref keeps A1 and B1 alive after the agent rewrote them.

## Review branches

Every review is written to a named review branch: a ref `refs/reviews/<branch>` pointing at the branch's newest review commit. There is no default branch, so tools must require a branch name when creating a review and never choose one implicitly. The ref also keeps the branch's review commits safe from garbage collection and gives tools a fixed place to find the newest one.

Branch names follow Git's ref-name rules (`git check-ref-format`); one branch per agent task or per code branch is a natural fit. A branch starts with a root review commit, or by pointing a new ref at an existing review commit to fork that history. Branches are independent: each has its own DAG and tree, and one branch's reviews are invisible from another unless a review commit merges them by listing both tips as review parents.

A branch's ref is only ever moved with a compare-and-swap, `git update-ref <ref> <new> <old>`. A human and an agent can write at the same moment even in one local repository; the loser rebuilds its commit on the new tip, which can't conflict because it only adds a file. Git does not push or fetch `refs/reviews/*` by default, so review data stays local unless pushed explicitly.

## Review commit object

A review commit is an ordinary Git commit with one extra header, `review-parents`. Its parents are its review parents plus the commits it reviews, and the header's presence is what marks it as a review commit.

### Headers

| Header | Required | Value |
| --- | --- | --- |
| `tree` | yes | The review tree (see Tree layout) |
| `parent` | one per parent | One line per review parent and per reviewed commit, in any order (see Parents) |
| `author`, `committer` | yes | Whoever wrote the review, human or agent |
| `review-parents` | yes, always | Space-separated object IDs of the review parents; empty on a root review commit |

Extra headers come after `committer`, as Git's commit format requires. Object IDs are full hex in the repository's hash: 40 characters for SHA-1, 64 for SHA-256.

On a root review commit, `review-parents` is present with an empty value, written as the key, one space and a line feed. Git 2.43 accepts this, and `git fsck --strict` passes. Without the space Git still accepts it, but dulwich, a Python Git library, rejects the commit as malformed.

Readers must tell an absent header (not a review commit) from an empty one (a root review commit). A helper that returns an empty string for a missing header would misclassify every commit.

The format has no version header; its absence means format 1. An incompatible future format would add one, such as `review-format 2`.

### Parents

A review commit's parents are its review parents plus its reviewed commits, in any order. The `review-parents` header says which parents are review parents; every other parent is a reviewed commit. Tools walking the review DAG follow `review-parents`, not `parent`.

The reviewed commits record which commits the review covers, and keep their code reachable. For now they are decoupled from the comments' `commit` fields: usually each comment's commit is a reviewed commit and vice versa, but the spec doesn't require it. If a comment's commit isn't a reviewed commit anywhere on the branch, nothing on the branch keeps its code alive, so writers should normally include it.

There is normally one review parent, the previous tip of the branch. Two or more appear only when a commit joins divergent tips or merges two branches.

### Message

The message is free text; writers should start it with `Review <review-id>`. Tools never parse it.

### Example

R3 from the worked example, with one review parent (R2) and one reviewed commit (B2):

```
$ git cat-file -p refs/reviews/config-loader
tree 3b1f0c9e5a7d2e4f6b8c0a1d3e5f7b9c2d4e6f80
parent 8d2a41f7e0c5b93a6d1f4e8c2b7a0d5f9e3c6b18
parent c4e90b12a7d3f58e2c6b0a9d4f1e7c3b5a8d6f24
author Dana <dana@example.com> 1790587800 +0000
committer Dana <dana@example.com> 1790587800 +0000
review-parents 8d2a41f7e0c5b93a6d1f4e8c2b7a0d5f9e3c6b18

Review 01a0e3f0-6a10-7b95-9d24-1e3f5a7c9b02
```

## Tree layout

A review commit's tree holds every review made so far: all files from its review parents' trees, plus the files it adds. Reading the newest tree is enough to reconstruct a branch's full review state.

```
<review tree>
└── reviews/
    ├── 01a0e21d-d100-7a3c-8b21-4f6e0c9d2a17.jsonl    (R1)
    ├── 01a0e25b-1c40-7d58-b6e2-8a0c4f1e3d69.jsonl    (R2)
    └── 01a0e3f0-6a10-7b95-9d24-1e3f5a7c9b02.jsonl    (R3)
```

Each file is named `<review-id>.jsonl`, where the review ID is a UUIDv7 (RFC 9562). A UUIDv7 starts with a millisecond timestamp, so Git's sorted listing of `reviews/` is also chronological. Files are regular blobs with mode `100644`; subdirectories inside `reviews/` are reserved.

A review commit may add any number of review files, including none. Unchanged files are the same blobs in every commit, so a commit costs one blob per added file, plus one `reviews/` tree and one root tree. Readers ignore top-level entries other than `reviews/`, which leaves room for later additions.

## Review files and actions

A review file is UTF-8 JSONL: one JSON object per line, each object one review action, every line ending with a line feed and no blank lines. A file's actions come from one author in one sitting, in the order they were made.

### Fields on every action

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `id` | string, UUIDv7 | yes | Unique across the whole review DAG; other actions refer to it |
| `type` | string | yes | `comment`, `reply` or `delete` |
| `author` | object `{name, email?}` | yes | Who made the action |
| `created_at` | string, RFC 3339 in UTC | yes | When it was made, e.g. `2026-09-27T14:05:00Z` |

Readers ignore fields they don't recognize. An action with an unknown `type` is kept but not interpreted, so newer writers don't break older readers.

### comment: top-level or line range

A comment starts a thread about a reviewed commit. Without `anchor` it is a top-level comment on the commit as a whole; with `anchor` it is attached to a range of lines.

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `commit` | string, object ID | yes | The commit the comment is about; normally also one of the review commit's reviewed commits (see Parents) |
| `anchor.path` | string | with anchor | File path in `commit`'s tree, `/`-separated, relative to the repository root |
| `anchor.start_line` | integer ≥ 1 | with anchor | First line, 1-based |
| `anchor.end_line` | integer ≥ `start_line` | with anchor | Last line, inclusive |
| `body` | string, Markdown | yes | The comment text |

Line numbers refer to the file as it exists in `commit`, the new side of that commit's diff. Deleted lines can't be anchored in version 1.

```json
{"id":"01a0e21d-d4f0-7e12-9a0b-3c5d7e9f1b24","type":"comment","author":{"name":"Dana","email":"dana@example.com"},"created_at":"2026-09-27T14:05:00Z","commit":"9c3a7e2f0b5d41c8e6a2f9d0b7c3e5a1f8d4b062","body":"Server startup should fail loudly when the config file is missing."}
{"id":"01a0e21d-e2a8-72f4-a1c3-5e7f9b0d2c46","type":"comment","author":{"name":"Dana","email":"dana@example.com"},"created_at":"2026-09-27T14:06:10Z","commit":"5d1e8a0c3b7f42e19a6d0c8b3e5f7a2d4c6b8e01","anchor":{"path":"src/config.rs","start_line":12,"end_line":18},"body":"This hand-rolled parser misses quoted values. Use the `toml` crate?"}
```

### reply

A reply answers a comment or another reply, so threads form a tree.

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `in_reply_to` | string, action ID | yes | The `comment` or `reply` being answered |
| `body` | string, Markdown | yes | The reply text |

A reply has no `commit`; it belongs to its thread's anchor. Its target may sit in the same file or in any earlier review.

```json
{"id":"01a0e25b-1d02-7c4a-8f13-6b2d0e5a7c91","type":"reply","author":{"name":"coding-agent"},"created_at":"2026-09-27T15:12:40Z","in_reply_to":"01a0e21d-e2a8-72f4-a1c3-5e7f9b0d2c46","body":"Switched to the `toml` crate in 2b7f0e4c."}
```

### delete

A delete hides a comment or reply.

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `target` | string, action ID | yes | The `comment` or `reply` to hide |

Deletion is logical. The target's line stays in its file and in Git history; readers show it as deleted, and its replies stay visible under a placeholder. Deleting the same target twice has the same effect as once, and targeting a `delete` is invalid; to restore a comment, write a new one.

Because nothing is ever erased, purging something posted by mistake (a secret, say) requires rewriting the review branch's history.

```json
{"id":"01a0e3f0-6b3c-7f61-a8e5-2d4c6b8a0e13","type":"delete","author":{"name":"Dana","email":"dana@example.com"},"created_at":"2026-09-28T09:30:00Z","target":"01a0e21d-d4f0-7e12-9a0b-3c5d7e9f1b24"}
```

## Invariants and validation

Writers must uphold all nine rules below. Readers should check them but degrade gracefully: skip an invalid action, surface a dangling reply as an orphan, and never fail the whole load over one bad line.

| Rule | Requirement |
| --- | --- |
| V1 | The commit carries a `review-parents` header, empty only on a root review commit |
| V2 | Every ID in `review-parents` is also a `parent` |
| V3 | Every review parent is itself a valid review commit |
| V4 | No parent appears twice. Reviewed commits aren't tied to the comments' `commit` values (see Parents) |
| V5 | `reviews/` contains every entry of every review parent's `reviews/`, with identical blob IDs: files are never changed or removed |
| V6 | Each added file is named `<uuidv7>.jsonl` and that name exists in no review parent |
| V7 | Every line parses as a JSON object with the common fields, and every `id` is unique across the whole tree |
| V8 | Each `in_reply_to` and `target` names a `comment` or `reply` in the same tree |
| V9 | Each `anchor.path` exists in `commit`'s tree, and `start_line` ≤ `end_line` ≤ the file's line count |

V5 is what makes merging trivial: joining two tips is a union of their `reviews/` entries, and a name clash can only mean a bug.

## Worked example

Three review commits on the review branch `refs/reviews/config-loader` cover a full loop: Dana reviews the agent's work, the agent rewrites its commits and replies, and Dana reviews again. This is the scenario in the Overview diagram.

The agent starts with two commits on `main`: A1 (`5d1e8a0c`, adds a config loader) and B1 (`9c3a7e2f`, uses it in the server). After R1, the agent rewrites them with jj into A2 (`2b7f0e4c`) and B2 (`c4e90b12`).

| Review commit | Author | Actions added | Review parents | Reviewed commits |
| --- | --- | --- | --- | --- |
| R1 `e71b3d9a` | Dana | Top-level comment on B1; line comment on A1 `src/config.rs:12-18` | none | B1, A1 |
| R2 `8d2a41f7` | coding-agent | Reply to the line comment | R1 | none |
| R3 `f2c8a06d` (tip) | Dana | Delete of the top-level comment; line comment on B2 `src/server.rs:40-44` | R2 | B2 |

R1 is a root review commit, so its `review-parents` header is present but empty (the key followed by one space) and all of its parents are reviewed commits:

```
$ git cat-file -p e71b3d9a4c2f60e8b5d1a7c3f9e2b4d6a8c0f135
tree 6a0d2f4b8c1e3a5d7f9b0c2e4a6d8f1b3c5e7a92
parent 9c3a7e2f0b5d41c8e6a2f9d0b7c3e5a1f8d4b062
parent 5d1e8a0c3b7f42e19a6d0c8b3e5f7a2d4c6b8e01
author Dana <dana@example.com> 1790518000 +0000
committer Dana <dana@example.com> 1790518000 +0000
review-parents 

Review 01a0e21d-d100-7a3c-8b21-4f6e0c9d2a17
```

R1's file holds the two comments shown under Review files and actions, and R2's file holds the one reply shown there. R3's commit object is the example under Review commit object; its file adds:

```json
{"id":"01a0e3f0-6b3c-7f61-a8e5-2d4c6b8a0e13","type":"delete","author":{"name":"Dana","email":"dana@example.com"},"created_at":"2026-09-28T09:30:00Z","target":"01a0e21d-d4f0-7e12-9a0b-3c5d7e9f1b24"}
{"id":"01a0e3f0-7c88-7a2d-b0f4-3e5d7c9a1b35","type":"comment","author":{"name":"Dana","email":"dana@example.com"},"created_at":"2026-09-28T09:31:20Z","commit":"c4e90b12a7d3f58e2c6b0a9d4f1e7c3b5a8d6f24","anchor":{"path":"src/server.rs","start_line":40,"end_line":44},"body":"Log the resolved config path at startup."}
```

Rendering R3's tree as threads, with deleted comments marked, gives:

```
B1 9c3a7e2f · whole commit
  [deleted]                (no replies, so a reader may hide it)
A1 5d1e8a0c · src/config.rs:12-18
  Dana: This hand-rolled parser misses quoted values. Use the `toml` crate?
    coding-agent: Switched to the `toml` crate in 2b7f0e4c.
B2 c4e90b12 · src/server.rs:40-44
  Dana: Log the resolved config path at startup.
```

## Working with jj and rewritten commits

Comments pin exact commit IDs, so they keep pointing at the code that was reviewed even after jj rewrites it. The format stores nothing jj-specific; the conveniences below come from what jj already writes into Git.

**Reviewed commits survive rewrites.** When the agent rewrites B1 into B2, jj hides B1, but the review branch's ref still reaches it. Git never garbage-collects it, so the anchor stays readable until the branch is deleted.

**Old comments map to the current version of a change.** jj records each commit's change ID in a `change-id` commit header. A reader can take a comment's `commit`, read that header, and find the change's current commit with `jj log -r 'change_id(<id>)'`. Then `jj interdiff --from <old> --to <current>` shows what the agent changed in that change since the comment. Commits written by plain Git lack the header, so this mapping is best effort.

**Review commits stay out of the way.** jj imports bookmarks, remote bookmarks and tags, not arbitrary refs, so review commits don't appear in `jj log`. Plain `git log --all` does show them as merge commits; `git log --exclude='refs/reviews/*' --all` hides them.

Line numbers in an anchor always refer to the reviewed commit, never to the current code. Translating an old anchor to current lines is a reader's job; see Open questions.

## Design rationale

Each decision trades a little redundancy for simpler reads and writes that can't conflict.

| Decision | Why |
| --- | --- |
| Reviewed commits are parents | Reachability: a branch's ref keeps reviewed code alive through rewrites and GC, and pushing the branch would carry that code along |
| `review-parents` repeats part of the parent list | Git parents alone can't tell review history from reviewed code without opening every parent; the header makes it one read |
| `review-parents` always present, empty on roots | Marks review commits without a second header; versioning waits until a format 2 exists |
| Loose parent rules | Parent order, files per commit and the link between reviewed commits and comments stay unspecified, so stricter rules can be added later without invalidating existing reviews |
| Cumulative tree | State is one tree read, joining tips is a union, and unchanged blobs are shared |
| One file per review, never edited | No write touches an existing path, so Git-level conflicts are impossible and history doubles as an audit log |
| Delete as a tombstone action | Keeps the format append-only; restoring means writing a new comment |
| State derived by set operations | No clocks or ordering rules needed; any merge of tips yields the same state |
| UUIDv7 IDs | Unique without coordination and sortable by time |
| Review branch required, no default | Every review is filed deliberately, so unrelated work never piles into one shared history |

Two alternatives were rejected. Git notes attach data to a commit ID, so notes dangle when commits are rewritten, and jj doesn't carry notes over. A single mutable JSON file makes every concurrent write a merge conflict.

## Open questions and future extensions

The first two extensions below would be the first actions whose effect depends on order, so each needs an ordering rule such as latest `created_at` wins, ties broken by `id`.

- [ ] **Thread resolution:** `resolve` and `reopen` actions targeting a thread root.
- [ ] **Edits:** an `edit` action carrying a new body for a comment or reply.
- [ ] **Comments on deleted lines:** an `anchor.side` of `old`, meaning the file in `commit`'s first parent.
- [ ] **File-level comments:** an `anchor` with `path` but no line range.
- [ ] **Line drift:** translating an old anchor to lines in current code, computed from diffs or recorded by the agent in a `relocate` action.
- [ ] **Comments without a commit:** whether general notes may omit `commit`; version 1 requires it.
- [ ] **Reviewed commits and comments:** whether to require that every comment's `commit` is a reviewed commit, and that every reviewed commit has a comment.
- [ ] **Permissions:** whether only a comment's author may delete it; version 1 lets anyone.
- [ ] **Scale:** sharding `reviews/` by UUID prefix if a branch reaches thousands of reviews.
