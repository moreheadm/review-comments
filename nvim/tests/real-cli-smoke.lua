local root = vim.env.RV_PLUGIN_ROOT
vim.opt.rtp:prepend(root)
package.path = root .. "/lua/?.lua;" .. root .. "/lua/?/init.lua;" .. package.path

local function check(value, message)
  if not value then error("FAIL: " .. message, 2) end
end

local repo = vim.env.RV_REAL_REPO
local rvbin = vim.env.RV_REAL_CLI
local file = repo .. "/src/review.lua"
vim.cmd.edit(vim.fn.fnameescape(file))
local buffer = vim.api.nvim_get_current_buf()
local rv = require("rv")
rv.setup({ command = rvbin, branch = "nvim-smoke", autosave_on_comment = true })
check(rv.create_branch("nvim-smoke"), "real CLI explicit create selection")

local first, first_err = rv.comment({ body = "Review the original line", start_line = 2, end_line = 2 })
check(first ~= nil, "real CLI first draft: " .. tostring(first_err))
local initial_commit = first.commit
local draft_marks = vim.api.nvim_buf_get_extmarks(buffer, require("rv.render").namespace(), 0, -1, {})
check(#draft_marks == 1, "draft extmark is visible before the new branch exists")
local refreshed = rv.refresh()
check(refreshed and not refreshed.error, "explicitly armed missing branch is an empty saved review")
local first_commit, first_commit_err = rv.save()
check(first_commit ~= nil, "real CLI first commit: " .. tostring(first_commit_err))
check(first_commit.branch == "nvim-smoke", "review lands on explicit branch")
check(rv.select_branch("nvim-smoke"), "real rv branches recognizes existing branch")

local render = require("rv.render")
local initial = render.show(repo, "nvim-smoke", initial_commit, "src/review.lua", rvbin)
check(initial ~= nil and #initial.threads == 1, "real rv show --at renders saved thread")
check(initial.threads[1].anchor.commit == initial_commit, "JSON keeps full pinned anchor commit")
local initial_marks = vim.api.nvim_buf_get_extmarks(buffer, render.namespace(), 0, -1, {})
check(#initial_marks >= 1, "real saved comment creates an extmark")

-- Insert a line above the old anchor. autosave_on_comment writes only this
-- source buffer; its new jj @ is pinned by the second draft, while rv maps the
-- first saved comment to the shifted line. Verify the review ref itself does
-- not move until the following explicit save.
local cli = require("rv.cli")
local review_ref = "refs/reviews/nvim-smoke"
local tip_before_autosave = cli.capture("git", { "rev-parse", review_ref }, repo)
check(tip_before_autosave ~= nil, "review ref exists before source autosave")
vim.api.nvim_buf_set_lines(buffer, 0, 0, false, { "-- inserted above reviewed line" })
local second, second_err = rv.comment({ body = "Review this new header", start_line = 1, end_line = 1 })
check(second ~= nil, "autosave-on-comment pins and drafts new snapshot: " .. tostring(second_err))
check(not vim.bo[buffer].modified, "autosave writes source buffer")
check(second.commit ~= initial_commit, "new code snapshot gets a distinct full commit ID")
local mapped = render.show(repo, "nvim-smoke", second.commit, "src/review.lua", rvbin)
check(mapped ~= nil and mapped.threads[1].mapped.start_line == 3,
  "rv show --at maps original line after inserted line")
local mapped_marks = vim.api.nvim_buf_get_extmarks(buffer, render.namespace(), 0, -1, {})
local has_line_three = false
for _, extmark in ipairs(mapped_marks) do
  if extmark[2] == 2 and extmark[3] == 0 then has_line_three = true end
end
check(has_line_three, "mapped saved comment extmark uses displayed new-side line")
check(#rv.get_drafts() == 1, "autosave does not commit the review")
local tip_after_autosave = cli.capture("git", { "rev-parse", review_ref }, repo)
check(tip_after_autosave == tip_before_autosave, "autosave leaves review ref unchanged")
check(rv.save(), "explicitly save second real review action")

local target = first.id
local reply, reply_err = rv.reply(target, { body = "Acknowledged", buffer = buffer })
check(reply ~= nil, "real CLI validates and drafts reply: " .. tostring(reply_err))
check(rv.save(), "explicitly save real reply")
local replied = render.show(repo, "nvim-smoke", second.commit, "src/review.lua", rvbin)
check(replied.threads[1].replies[1].body == "Acknowledged", "real show returns saved reply")

local deletion, delete_err = rv.delete(target)
check(deletion ~= nil, "real CLI validates and drafts deletion: " .. tostring(delete_err))
check(rv.save(), "explicitly save real deletion")
local deleted = render.show(repo, "nvim-smoke", second.commit, "src/review.lua", rvbin)
check(deleted.threads[1].deleted == true, "real show exposes logical delete tombstone")
local checked, check_err = require("rv.cli").rv("check", { "-b", "nvim-smoke" }, {
  cwd = repo,
  executable = rvbin,
})
check(checked ~= nil and checked.ok == true, "real rv check validates plugin-written branch: " .. tostring(check_err))

-- Ordinary buffer destruction must cancel a composer exactly once.
check(rv.comment({ start_line = 1 }), "open composer for forced closure")
local composer = vim.api.nvim_get_current_buf()
check(rv.get_state().composers == 1, "composer counted while open")
vim.cmd("bwipeout!")
check(not vim.api.nvim_buf_is_valid(composer), "composer was wiped")
check(rv.get_state().composers == 0, "forced wipe releases composer scope")
check(rv.select_branch("nvim-smoke"), "branch selection works after forced composer wipe")

-- External deletion retains drafts but explicit same-scope re-arming recovers.
local recovery = rv.comment({ body = "retained after ref deletion", start_line = 1 })
check(recovery ~= nil, "create recovery draft")
check(cli.capture("git", { "update-ref", "-d", review_ref }, repo) ~= nil, "delete review ref externally")
local failed, failure = rv.save()
check(not failed and failure:match("RvBranchCreate nvim%-smoke"), "save offers actionable re-arm command")
check(#rv.get_drafts() == 1, "failed save retains recovery draft")
check(not rv.create_branch("different"), "recovery cannot redirect drafts to another branch")
check(rv.create_branch("nvim-smoke"), "explicit same-scope creation may retain drafts")
check(#rv.get_drafts() == 1, "re-arming does not discard drafts")
check(rv.save(), "save succeeds after explicit same-scope re-arm")

print("real rv CLI Neovim smoke passed")
