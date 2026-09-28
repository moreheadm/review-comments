# rv Neovim integration

This is a Neovim >= 0.10 plugin for the `rv` CLI. Put this directory on
`runtimepath`, install `rv` on `PATH`, and configure it with:

```lua
require("rv").setup({
  branch = "agent-task-42", -- optional; selects, never creates
  autosave_on_comment = false,
  commit_on_close = false, -- optional: commit review when closing a nonempty composer
  display = "sidebar", -- default; use "inline" for end-of-line comment text
})
```

The plugin loader also calls `setup()` with defaults. A configured or selected
branch is always repository-scoped. If no branch was supplied, select one with
`:RvBranch NAME`; this only accepts an existing review branch. Run
`:RvBranch` without a name to list available branches. To start a new
branch, explicitly arm creation with `:RvBranchCreate NAME`, then add an action
and save it. The branch is created only by a successful `:RvCommit`. Selecting
or creating a branch never silently redirects or discards drafts.

## Commands

- `:RvBranch` — list available review branches in this repository (the selected
  branch is marked with `*`). `:RvBranch NAME` selects an existing branch.
- `:RvBranchCreate NAME` — explicitly select a name that does not yet exist.
- `:[range]RvComment` — draft a comment on the current line/range. Select lines
  visually and run `:RvComment` (Neovim supplies `'<,'>`), or use e.g.
  `:2,5RvComment` for a multiline anchor. A Markdown scratch buffer opens;
  `<C-s>` (or `:write` with default settings) adds the draft and `<C-c>`
  cancels. With `commit_on_close = true`, closing the composer (`:close` or
  `:q`) commits the review (including other pending drafts) when the body is
  nonempty; `<C-c>` still cancels. In that mode use `<C-s>` rather than `:write`. The body can contain multiple lines; Lua
  callers may pass `body` directly.
- `:RvReply ACTION_ID` — draft a reply to a saved or current-draft comment/reply;
  it uses the same Markdown composer (or Lua `body`).
- `:RvDelete ACTION_ID` — draft a logical deletion of a comment or reply.
- `:RvEdit ACTION_ID` — reopen an unsaved comment/reply draft to edit its body.
  Normally just move to the draft in the sidebar (or `:RvDrafts`/`:RvShow`)
  and press `<CR>` instead; UUIDs are not shown in plugin windows. Saving
  preserves its ID and line range. Saved comments and delete drafts cannot
  be edited this way.
- `:RvCommit` — explicitly pass all in-memory drafts as JSONL to
  `rv commit --json`. Failures keep every draft for retry.
- `:RvShow` — show saved review threads and drafts for the selected branch.
- `:RvDrafts` — list unsaved in-memory drafts.
- `:RvSidebarOpen`, `:RvSidebarClose`, `:RvSidebarToggle` — explicitly control
  the sidebar. Closing it keeps it closed across file navigation and comments;
  opening it again refreshes the current file. `:RvShow`, `:RvDrafts`, and
  `:RvBranch` explicitly open a listing in the sidebar.
- `:RvRefresh` — reload mapped comments and extmarks in the current buffer.

Saved line comments render as extmarks after `rv show -b NAME --at FULL_OID
--path PATH --json`. Branch tip and `--at` are always explicit full commit IDs;
comments on rewrites are placed by `rv`'s mapping result. Comment and draft
anchors have a sign and highlighted line range, including while the composer
is open. By default the sidebar opens alongside the source buffer when a
branch is selected. It places each mapped comment or draft beside its source
line, with body continuation lines underneath, and follows source scrolling.
`:RvShow` shows the whole branch as a separate listing; `:RvDrafts` lists all
unsaved actions. The sidebar uses one stable buffer name when switching files
or listings. Closing a comment composer returns to the source window without
closing it; saving a comment does not reopen a sidebar you closed explicitly.
Use `display = "inline"` to keep end-of-line first-line previews instead.

## Buffer safety and Diffview support

For ordinary file buffers the plugin requires a named normal buffer in a jj or
Git repository. It pins a jj buffer to a fresh `jj log -r @ --no-graph -T
'commit_id ++ "\n"'` snapshot (plain Git falls back to full `HEAD`), reads that
file from the pinned tree, and compares the exact bytes reconstructed from the
buffer before accepting an anchor. A modified source buffer is rejected by
default. With `autosave_on_comment = true`, the plugin writes only that source
buffer before snapshotting and comparing it; it never runs `rv commit`
automatically.

Diffview comments are accepted only from the actual current window of a
supported two-pane `a`/`b` comparison, with the `b` pane identified as the
new side and its file revision a full commit ID. The anchor uses that pane's
commit and repository-relative path—not jj `@` and not the selected branch
head. Before accepting the comment, the plugin reads the file blob from that
commit and compares it to the displayed buffer. Old-side panes, deleted
files/lines, modified/stale pane contents, inline and three-/four-way merge
layouts, local/uncommitted new sides, and unrecognized layouts are refused.
This means a Diffview+ `pin_local` working-tree pane cannot be commented on
until the new side is a committed revision.

Neither upstream exposes a public API for discovering the active file pane's
revision. The adapter therefore reads their shared, guarded runtime objects:
`diffview.lib.get_current_view()`, the actual
`view.cur_layout.windows`/`a`/`b` window IDs, `view.cur_entry.layout`, each
active `Window.file`'s `bufnr/symbol/path/rev.commit`, and
`view.adapter.ctx.toplevel`. It resolves the pane using the current window (or
a unique visible window for an explicitly supplied buffer), never the first
matching buffer in the layout. Missing or changed internals and ambiguous
buffer/window identity fail closed instead of falling back to a guessed
commit. The adapter fixtures in `tests/fixtures/`
record these shapes from read-only shallow clones of
[sindrets/diffview.nvim](https://github.com/sindrets/diffview.nvim) at
`4516612` and
[dlyongemallo/diffview-plus.nvim](https://github.com/dlyongemallo/diffview-plus.nvim)
at `5152bad`.

Drafts are scoped to their repository and selected branch. Branch switching is
refused while any draft or composer is active; save also verifies that the
selected repository/branch still matches the draft scope. Comment, reply and
delete actions receive IDs from `rv id`; only a successful explicit `:RvCommit`
clears the drafts. By default closing or wiping a composer cancels it without
blocking later branch selection. With `commit_on_close = true`, closing a
nonempty composer adds its draft and calls `rv commit` for all pending drafts.
A failed commit retains the drafts for retry via `:RvCommit`. Explicit `<C-c>`
always cancels without committing. If the selected branch is deleted externally,
drafts are retained: `:RvBranchCreate SAME_NAME` explicitly re-arms creation for
the same repository and branch. This does not restore deleted review history;
replies to lost targets cannot be saved until that history is restored.

## Lua API

```lua
local rv = require("rv")
rv.setup({ branch = "agent-task-42", autosave_on_comment = false, commit_on_close = false })

-- With body, these return the created action; without body, comment/reply opens
-- the Markdown composer. Failures return nil, message.
local action, err = rv.comment({ start_line = 12, end_line = 14, body = "Please simplify this." })
local reply = rv.reply("<comment-or-reply-id>", { body = "I will change this." })
local deletion = rv.delete("<comment-or-reply-id>")
local commit_result, save_error = rv.save()
```

`comment` accepts `buffer`, `start_line`, `end_line`, `body`, `autosave_on_comment`,
and `anchor = false` (top-level comment). Other public helpers are
`edit_draft(id)`, `branches_view()`, `select_branch(name)`, `create_branch(name)`,
`open_sidebar()`, `close_sidebar()`, `toggle_sidebar()`, `show()`, `refresh()`,
`get_drafts()`, and `get_state()`. `create_branch` has the same explicit-arming
semantics as `:RvBranchCreate`. The `command` setup option may point to a
non-default `rv` executable (also used by the headless tests).
`commit_on_close` applies to newly opened composers; it commits the review
when a nonempty composer is closed or saved with `<C-s>`, but does not
save source files.

## Tests

Run `nvim/tests/run-headless.sh` from the project root. It creates temporary
jj-colocated and plain-Git repositories, puts a mock `rv` CLI in the test
configuration, and checks snapshots/buffer matching, in-memory drafts,
multiline composition, explicit branch creation and commit, failure retention,
replies/deletions, extmarks, and both upstream Diffview adapter fixtures.

With the real CLI built, run `RV_CLI=/path/to/rv nvim/tests/real-cli-smoke.sh`.
That smoke test creates a throwaway colocated jj/Git repository, writes and
checks a review branch with the plugin, verifies `show --at` line mapping,
replies/deletions, `rv check`, and that autosaving source changes remains
separate from explicit review commits.
