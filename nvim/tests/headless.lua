local root = vim.env.RV_PLUGIN_ROOT
vim.opt.rtp:prepend(root)
package.path = root .. "/lua/?.lua;" .. root .. "/lua/?/init.lua;" .. package.path

local function check(value, message)
  if not value then error("FAIL: " .. message, 2) end
end

local repo = vim.env.RV_TEST_REPO
local file = repo .. "/src/example.lua"
vim.cmd.edit(vim.fn.fnameescape(file))
local source = vim.api.nvim_get_current_buf()
local OID = vim.env.RV_FAKE_COMMIT_OID
local review_oid = vim.env.RV_FAKE_REVIEW_OID
local fake = vim.env.RV_FAKE_RV
local rv = require("rv")
rv.setup({ command = fake, branch = "review", autosave_on_comment = false })

local commands = {
  "RvBranch", "RvBranchCreate", "RvComment", "RvReply", "RvDelete",
  "RvCommit", "RvShow", "RvDrafts", "RvRefresh",
}
for _, name in ipairs(commands) do
  check(vim.fn.exists(":" .. name) == 2, "command is registered: " .. name)
end

local action, err = rv.comment({ body = "should not silently create", start_line = 1 })
check(action == nil and err:match("RvBranchCreate"), "absent branch requires explicit create selection")
check(rv.create_branch("review"), "explicit branch creation selection")
local first, first_err = rv.comment({ body = "saved via explicit commit", start_line = 1, end_line = 2 })
check(first ~= nil, "normal buffer comment draft: " .. tostring(first_err))
check(first.commit == OID, "comment pins full jj @ snapshot")
check(first.anchor.path == "src/example.lua" and first.anchor.start_line == 1, "repo-relative line anchor")
check(#rv.get_drafts() == 1, "draft is in memory")
check(vim.fn.filereadable(vim.env.RV_FAKE_LOG) == 0, "drafting did not call rv commit")

local marks = vim.api.nvim_buf_get_extmarks(source, require("rv.render").namespace(), 0, -1, {})
check(#marks == 1, "in-memory draft renders before explicitly created branch exists")
vim.cmd.edit(vim.fn.fnameescape(vim.env.RV_GIT_TEST_REPO .. "/src/plain.txt"))
local wrong_repo_save, wrong_repo_err = rv.save()
check(wrong_repo_save == nil and wrong_repo_err:match("differs from draft repository"),
  "save refuses a changed repository and keeps draft scope")
check(#rv.get_drafts() == 1, "repository mismatch retains draft")
vim.cmd.edit(vim.fn.fnameescape(file))

vim.env.RV_FAKE_FAIL_COMMIT = "1"
local failed, fail_err = rv.save()
check(failed == nil and fail_err:match("mock failure"), "failed explicit save is reported")
check(#rv.get_drafts() == 1, "failed save retains its draft")
vim.env.RV_FAKE_FAIL_COMMIT = nil
local committed, commit_err = rv.save()
check(committed ~= nil, "explicit save succeeds after retry: " .. tostring(commit_err))
check(#rv.get_drafts() == 0, "successful save clears drafts")
check(vim.fn.filereadable(vim.env.RV_FAKE_LOG) == 1, "commit call recorded by mock CLI")
local log = table.concat(vim.fn.readfile(vim.env.RV_FAKE_LOG), "\n")
check(log:match("--create"), "only explicit branch-create selection passes --create")
check(log:match("--reviewed\t" .. OID), "commit includes full pinned reviewed ID")
check(log:match('"commit":"' .. OID .. '"'), "JSONL retains full comment commit ID")

local reply_target = "01912345-6789-7abc-8def-000000000099"
local reply, reply_err = rv.reply(reply_target, { body = "a reply", buffer = source })
check(reply ~= nil, "reply to a saved thread: " .. tostring(reply_err))
check(reply.in_reply_to == reply_target, "reply points to exact saved action ID")
local switched, switch_err = rv.select_branch("somewhere-else")
check(switched == nil and switch_err:match("drafts"), "branch switch refuses to strand drafts")
local deleted, delete_err = rv.delete(reply_target)
check(deleted ~= nil, "logical delete draft: " .. tostring(delete_err))
local saved_replies, save_replies_err = rv.save()
check(saved_replies ~= nil, "reply/delete explicitly save: " .. tostring(save_replies_err))

-- The built-in composer is multiline and still only creates an in-memory draft.
local opened, open_err = rv.comment({ start_line = 2, end_line = 2 })
check(opened == true, "comment opens Markdown composer: " .. tostring(open_err))
local composer = vim.api.nvim_get_current_buf()
vim.api.nvim_buf_set_lines(composer, 0, -1, false, { "first paragraph", "", "second paragraph" })
vim.cmd.write()
local drafts = rv.get_drafts()
check(#drafts == 1 and drafts[1].body == "first paragraph\n\nsecond paragraph", "composer preserves multiline Markdown")
check(rv.save(), "composer draft explicit save")

-- Modified normal buffers are rejected by default; opt-in writes the source
-- buffer, snapshots jj @ again, and still does not commit the review.
vim.api.nvim_buf_set_lines(source, 0, -1, false, { "return 'autosaved'", "return 2" })
local rejected, rejected_err = rv.comment({ body = "must not save", start_line = 1 })
check(rejected == nil and rejected_err:match("Save the source buffer"), "modified buffer is not silently saved")
rv.setup({ autosave_on_comment = true })
local before_autosave = first.commit
local log_size_before_autosave = vim.fn.getfsize(vim.env.RV_FAKE_LOG)
local autosaved, autosave_err = rv.comment({ body = "autosave source only", start_line = 1 })
check(autosaved ~= nil, "autosave option writes source and drafts: " .. tostring(autosave_err))
check(not vim.bo[source].modified, "autosave option writes source buffer")
check(autosaved.commit ~= before_autosave, "autosaved comment pins newly snapshotted jj @")
check(#rv.get_drafts() == 1, "autosave option did not commit review")
check(vim.fn.getfsize(vim.env.RV_FAKE_LOG) == log_size_before_autosave,
  "autosave option never invokes rv commit")
check(rv.save(), "autosaved comment is explicitly committed")

-- If the source buffer is stale relative to the repository snapshot, anchoring
-- is refused even when Neovim does not know the file changed on disk.
local stale_before = vim.api.nvim_buf_get_lines(source, 0, -1, false)
vim.fn.writefile({ "external disk change", "return 2" }, file)
local mismatch, mismatch_err = rv.comment({ body = "stale", start_line = 1 })
check(mismatch == nil and mismatch_err:match("do not match"), "buffer/snapshot mismatch is rejected")
vim.fn.writefile(stale_before, file)

-- Actual runtime shapes from both checked-out upstreams. Resolve the pane by
-- current window identity, validate the b-side blob against its commit, and
-- fail closed for ambiguity, stale buffers, and unsupported merge layouts.
local context = require("rv.context")
local right_win = vim.api.nvim_get_current_win()
local source_before_split = vim.api.nvim_win_get_buf(right_win)
vim.cmd("vsplit")
local left_win = vim.api.nvim_get_current_win()
local old_buf = vim.api.nvim_create_buf(false, true)
local new_buf = vim.api.nvim_create_buf(false, true)
vim.api.nvim_buf_set_name(old_buf, "diffview://fixture/old")
vim.api.nvim_buf_set_name(new_buf, "diffview://fixture/new")
for _, bufnr in ipairs({ old_buf, new_buf }) do
  vim.bo[bufnr].fileformat = "unix"
  vim.bo[bufnr].fileencoding = "utf-8"
  vim.bo[bufnr].endofline = true
end
vim.api.nvim_buf_set_lines(old_buf, 0, -1, false, { "return 1", "return 2" })
vim.api.nvim_buf_set_lines(new_buf, 0, -1, false, { "return 1", "return 2" })
vim.api.nvim_win_set_buf(left_win, old_buf)
vim.api.nvim_win_set_buf(right_win, new_buf)
vim.api.nvim_set_current_win(right_win)
for _, fixture in ipairs({ "diffview-nvim", "diffview-plus" }) do
  local make_view = require("tests.fixtures." .. fixture)
  local view, old_file, new_file = make_view(repo, old_buf, new_buf, OID, OID, left_win, right_win)
  local new_context = context._diffview_file_context(new_buf, view)
  check(new_context and new_context.commit == OID, fixture .. " pins displayed b-side commit")
  check(new_context.path == "src/example.lua", fixture .. " uses new-side repository path")
  local old_context, old_err = context._diffview_file_context(old_buf, view)
  check(old_context == false and old_err:match("old%-side"), fixture .. " rejects old side")
  local explicit_old, explicit_old_err = context._diffview_file_context(old_buf, view, left_win)
  check(explicit_old == false and explicit_old_err:match("old%-side"), fixture .. " honors explicit old window")
  check(context._diffview_file_context(new_buf, view, right_win), fixture .. " honors explicit new window")

  -- Even if both panes display the same buffer, the active window decides the
  -- side; searching layout.windows for the first matching bufnr is unsafe.
  vim.api.nvim_win_set_buf(left_win, new_buf)
  old_file.bufnr = new_buf
  vim.api.nvim_set_current_win(left_win)
  local duplicate_old, duplicate_err = context._diffview_file_context(new_buf, view)
  check(duplicate_old == false and duplicate_err:match("old%-side"), fixture .. " uses active pane with duplicate bufnr")
  vim.api.nvim_set_current_win(right_win)
  check(context._diffview_file_context(new_buf, view), fixture .. " identifies active b pane with duplicate bufnr")
  vim.api.nvim_win_set_buf(left_win, old_buf)
  old_file.bufnr = old_buf

  new_file.nulled = true
  local deleted_context, deleted_err = context._diffview_file_context(new_buf, view)
  check(deleted_context == false and deleted_err:match("deleted"), fixture .. " rejects deleted side")
  new_file.nulled = false
  view.cur_entry.status = "D"
  local deleted_file_context, deleted_file_err = context._diffview_file_context(new_buf, view)
  check(deleted_file_context == false and deleted_file_err:match("deleted"), fixture .. " rejects deleted-file status")
  view.cur_entry.status = nil
  new_file.rev.commit = "LOCAL"
  local local_context, local_err = context._diffview_file_context(new_buf, view)
  check(local_context == false and local_err:match("not pinned"), fixture .. " rejects unpinned local side")
  new_file.rev.commit = OID

  vim.api.nvim_buf_set_lines(new_buf, 0, -1, false, { "modified buffer" })
  local stale_diff, stale_diff_err = context._diffview_file_context(new_buf, view)
  check(stale_diff == false and stale_diff_err:match("do not match"), fixture .. " verifies diff buffer content")
  vim.api.nvim_buf_set_lines(new_buf, 0, -1, false, { "return 1", "return 2" })

  local original_windows = view.cur_layout.windows
  view.cur_layout.windows = { original_windows[1], original_windows[2], { id = left_win, file = new_file } }
  view.cur_layout.c = view.cur_layout.windows[3]
  local merge_context, merge_err = context._diffview_file_context(new_buf, view)
  check(merge_context == false and merge_err:match("two%-pane"), fixture .. " rejects merge/multipane layout")
  local missing_internals = { adapter = view.adapter, cur_entry = view.cur_entry }
  local missing_context, missing_err = context._diffview_file_context(new_buf, missing_internals)
  check(missing_context == false and missing_err:match("Diffview"),
    fixture .. " rejects recognized pane when internals are missing")
  local fallback_context, fallback_err = context.for_buffer(new_buf, {})
  check(fallback_context == nil and fallback_err:match("Cannot identify this Diffview pane"),
    fixture .. " does not fall back to normal-buffer context")

  -- A local side can have a normal filesystem buffer name. If it still
  -- occupies an active Diffview pane but no longer matches the File object,
  -- do not reinterpret it as a normal jj@ buffer.
  view.cur_layout.windows = original_windows
  view.cur_layout.c = nil
  local old_lib = package.loaded["diffview.lib"]
  package.loaded["diffview.lib"] = { get_current_view = function() return view end }
  vim.api.nvim_buf_set_name(new_buf, repo .. "/src/diffview-local-side.lua")
  new_file.bufnr = old_buf
  local pane_mismatch, pane_mismatch_err = context.for_buffer(new_buf, {})
  check(pane_mismatch == nil and pane_mismatch_err:match("identity is inconsistent"),
    fixture .. " fails closed instead of normal-buffer fallback")
  new_file.bufnr = new_buf
  local missing_view = { tabpage = vim.api.nvim_get_current_tabpage() }
  package.loaded["diffview.lib"] = { get_current_view = function() return missing_view end }
  local missing_view_result, missing_view_err = context.for_buffer(new_buf, {})
  check(missing_view_result == nil and missing_view_err:match("normal%-buffer fallback is disabled"),
    fixture .. " disables normal fallback when active view internals are missing")
  vim.api.nvim_buf_set_name(new_buf, "diffview://fixture/new")
  package.loaded["diffview.lib"] = old_lib
end
vim.api.nvim_win_set_buf(right_win, source_before_split)
vim.api.nvim_win_close(left_win, true)
vim.api.nvim_set_current_win(right_win)
vim.api.nvim_buf_delete(old_buf, { force = true })
vim.api.nvim_buf_delete(new_buf, { force = true })

-- Plain Git (without jj) is supported too; it pins HEAD and checks the same
-- exact saved-buffer/tree-content invariant.
local git_file = vim.env.RV_GIT_TEST_REPO .. "/src/plain.txt"
vim.cmd.edit(vim.fn.fnameescape(git_file))
local cross_repo, cross_repo_err = rv.comment({ body = "wrong repo", start_line = 1 })
check(cross_repo == nil and cross_repo_err:match("another repository"),
  "selected branch is not silently reused in another repository")
local git_context, git_err = context.for_buffer(vim.api.nvim_get_current_buf(), {})
check(git_context ~= nil, "plain Git normal-buffer context: " .. tostring(git_err))
check(git_context.vcs == "git" and git_context.commit == vim.env.RV_GIT_TEST_OID,
  "plain Git buffer pins full HEAD")
vim.cmd.edit(vim.fn.fnameescape(vim.env.RV_GIT_TEST_REPO .. "/src/empty.txt"))
local empty_context, empty_err = context.for_buffer(vim.api.nvim_get_current_buf(), {})
check(empty_context ~= nil, "empty file snapshot matches Neovim's one-empty-line representation: " .. tostring(empty_err))

print("nvim headless tests passed")
