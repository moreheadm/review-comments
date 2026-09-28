local cli = require("rv.cli")
local context = require("rv.context")
local render = require("rv.render")

local M = {}
local state = {
  config = { command = "rv", autosave_on_comment = false },
  branch = nil,
  drafts = {},
  composers = 0,
  setup = false,
}
local rendered_buffers = {}
local command_group

local function notify(message, level)
  vim.notify("rv: " .. tostring(message), level or vim.log.levels.INFO)
end

local function current_buffer()
  return vim.api.nvim_get_current_buf()
end

local function current_repo()
  local root, err = context.repository_for_buffer(current_buffer())
  if root then return root end
  local name = vim.api.nvim_buf_get_name(current_buffer())
  if name:match("^rv://") and state.branch and state.branch.repo then
    return state.branch.repo
  end
  return nil, err
end

local function branch_names(root)
  local result, err = cli.rv("branches", {}, { cwd = root, executable = state.config.command })
  if not result then return nil, err end
  local items = result.branches or result.items
  if type(items) ~= "table" then
    return nil, "rv branches returned an unsupported JSON result"
  end
  local names, seen = {}, {}
  for key, value in pairs(items) do
    local name
    if type(value) == "string" then
      name = value
    elseif type(value) == "table" then
      name = value.name or value.branch
      if not name and type(value.ref) == "string" then
        name = value.ref:gsub("^refs/reviews/", "")
      end
    elseif type(key) == "string" and value then
      name = key
    end
    if type(name) == "string" and name ~= "" then
      name = name:gsub("^refs/reviews/", "")
      if not seen[name] then
        seen[name] = true
        names[#names + 1] = name
      end
    end
  end
  table.sort(names)
  return names
end

local function branch_exists(root, name)
  local names, err = branch_names(root)
  if not names then return nil, err end
  for _, candidate in ipairs(names) do
    if candidate == name then return true end
  end
  return false
end

local function branch_for(root)
  if not state.branch then
    return nil, "Select a review branch with :RvBranch or explicitly create one with :RvBranchCreate"
  end
  if state.branch.repo == nil then
    state.branch.repo = root
  elseif state.branch.repo ~= root then
    return nil, "The selected review branch belongs to another repository; select a branch for this repository"
  end
  return state.branch
end

local function guard_scope(root)
  local branch, err = branch_for(root)
  if not branch then return nil, err end
  local exists, exists_err = branch_exists(root, branch.name)
  if exists == nil then return nil, exists_err end
  if not exists and not branch.create_armed then
    return nil, ("Review branch '%s' does not exist; use :RvBranchCreate %s to explicitly create it"):format(
      branch.name,
      branch.name
    )
  end
  if exists and branch.create_armed then branch.create_armed = false end
  return branch, nil, exists
end

local function selected_root()
  local root, err = current_repo()
  if root then return root end
  return nil, err or "Cannot determine the current repository"
end

local function add_draft(action, draft_context, branch)
  if not branch or branch.repo ~= draft_context.repo then
    return nil, "The draft repository/branch context changed; select the original branch and retry"
  end
  action._context = {
    repo = draft_context.repo,
    branch = branch.name,
    commit = draft_context.commit,
    path = draft_context.path,
    buffer = draft_context.buffer,
  }
  state.drafts[#state.drafts + 1] = action
  if vim.api.nvim_buf_is_valid(draft_context.buffer) then
    rendered_buffers[draft_context.buffer] = true
    M.refresh_buffer(draft_context.buffer, false)
  end
  return action
end

local function create_id(root)
  local result, err = cli.rv("id", {}, { cwd = root, executable = state.config.command })
  if not result then return nil, err end
  if type(result.id) ~= "string" or result.id == "" then
    return nil, "rv id returned no action ID"
  end
  return result.id
end

local function line_range(opts, bufnr)
  local line_count = vim.api.nvim_buf_line_count(bufnr)
  local start_line = tonumber(opts.start_line)
  local end_line = tonumber(opts.end_line)
  if not start_line then
    local current = vim.api.nvim_get_current_win()
    local win = opts.winid or (vim.api.nvim_win_get_buf(current) == bufnr and current or vim.fn.bufwinid(bufnr))
    start_line = win and win ~= -1 and vim.api.nvim_win_is_valid(win)
      and vim.api.nvim_win_get_cursor(win)[1] or 1
  end
  if not end_line then end_line = start_line end
  if start_line < 1 or end_line < start_line or end_line > line_count then
    return nil, nil, "Comment range must be within the displayed new-side buffer"
  end
  return start_line, end_line
end

local function close_composer(composer)
  if composer.closed then return end
  composer.closed = true
  state.composers = math.max(0, state.composers - 1)
end

local function finalize_composer(composer, body)
  if composer.closed then return nil, "Composer is already closed" end
  if type(body) ~= "string" or body:gsub("%s", "") == "" then
    return nil, "Comment/reply body cannot be empty"
  end
  local branch = state.branch
  if not branch or branch.repo ~= composer.repo or branch.name ~= composer.branch then
    return nil, "The review branch changed while the composer was open; select its original branch first"
  end
  local action = vim.deepcopy(composer.action)
  action.body = body
  local added, err = add_draft(action, composer.context, branch)
  if not added then return nil, err end
  close_composer(composer)
  if vim.api.nvim_buf_is_valid(composer.buf) then
    pcall(vim.api.nvim_buf_delete, composer.buf, { force = true })
  end
  notify("draft added (not saved; use :RvCommit)")
  return added
end

local function open_composer(action, draft_context, branch, title)
  local composer = {
    action = action,
    context = draft_context,
    repo = draft_context.repo,
    branch = branch.name,
    closed = false,
  }
  state.composers = state.composers + 1
  vim.cmd("botright new")
  local buf = current_buffer()
  composer.buf = buf
  vim.api.nvim_create_autocmd("BufWipeout", {
    buffer = buf,
    once = true,
    callback = function() close_composer(composer) end,
  })
  vim.api.nvim_buf_set_name(buf, ("rv://%s/%s"):format(title, action.id))
  vim.bo[buf].buftype = "acwrite"
  vim.bo[buf].bufhidden = "wipe"
  vim.bo[buf].swapfile = false
  vim.bo[buf].filetype = "markdown"
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, { "" })
  vim.bo[buf].modified = true
  vim.api.nvim_create_autocmd("BufWriteCmd", {
    buffer = buf,
    callback = function()
      local body = table.concat(vim.api.nvim_buf_get_lines(buf, 0, -1, false), "\n")
      local saved, err = finalize_composer(composer, body)
      if not saved then notify(err, vim.log.levels.ERROR) end
    end,
  })
  vim.keymap.set("n", "<C-s>", function()
    vim.cmd("write")
  end, { buffer = buf, silent = true, desc = "Add rv draft" })
  vim.keymap.set("n", "<C-c>", function()
    close_composer(composer)
    pcall(vim.api.nvim_buf_delete, buf, { force = true })
  end, { buffer = buf, silent = true, desc = "Cancel rv draft" })
  notify(("%s in Markdown scratch buffer; <C-s> adds a draft, <C-c> cancels"):format(title))
  return true
end

local function validate_target(root, branch, id)
  for _, action in ipairs(state.drafts) do
    if action.id == id then
      if action.type == "comment" or action.type == "reply" then return action.type end
      return nil, "A delete action cannot be a reply/delete target"
    end
  end
  local result, err = render.show(root, branch.name, nil, nil, state.config.command)
  if not result then return nil, err end
  local found
  local function visit(thread)
    if thread.id == id then found = "comment" end
    for _, reply in ipairs(thread.replies or {}) do
      if reply.id == id then found = "reply" end
      visit(reply)
    end
  end
  for _, thread in ipairs(result.threads or {}) do visit(thread) end
  if not found then return nil, ("No comment or reply with ID %s on branch %s"):format(id, branch.name) end
  return found
end

function M.comment(opts)
  opts = opts or {}
  local bufnr = opts.buffer or current_buffer()
  local draft_context, err = context.for_buffer(bufnr, {
    autosave_on_comment = opts.autosave_on_comment == nil
      and state.config.autosave_on_comment
      or opts.autosave_on_comment,
    winid = opts.winid,
  })
  if not draft_context then return nil, err end
  local branch, branch_err = guard_scope(draft_context.repo)
  if not branch then return nil, branch_err end
  local start_line, end_line
  if opts.anchor ~= false then
    local range_err
    start_line, end_line, range_err = line_range(opts, bufnr)
    if not start_line then return nil, range_err end
  end
  local id, id_err = create_id(draft_context.repo)
  if not id then return nil, id_err end
  local action = { id = id, type = "comment", commit = draft_context.commit }
  if opts.anchor ~= false then
    action.anchor = { path = draft_context.path, start_line = start_line, end_line = end_line }
  end
  if opts.body ~= nil then
    if type(opts.body) ~= "string" or opts.body:gsub("%s", "") == "" then
      return nil, "Comment body cannot be empty"
    end
    action.body = opts.body
    return add_draft(action, draft_context, branch)
  end
  return open_composer(action, draft_context, branch, "comment")
end

function M.reply(target, opts)
  opts = opts or {}
  if type(target) ~= "string" or target == "" then return nil, "Reply requires a comment/reply ID" end
  local root, root_err = current_repo()
  if not root then return nil, root_err end
  local branch, branch_err = guard_scope(root)
  if not branch then return nil, branch_err end
  local target_type, target_err = validate_target(root, branch, target)
  if not target_type then return nil, target_err end
  local id, id_err = create_id(root)
  if not id then return nil, id_err end
  local action = { id = id, type = "reply", in_reply_to = target }
  local draft_context = { repo = root, commit = nil, path = nil, buffer = opts.buffer or current_buffer() }
  if opts.body ~= nil then
    if type(opts.body) ~= "string" or opts.body:gsub("%s", "") == "" then
      return nil, "Reply body cannot be empty"
    end
    action.body = opts.body
    return add_draft(action, draft_context, branch)
  end
  return open_composer(action, draft_context, branch, "reply")
end

function M.delete(target)
  if type(target) ~= "string" or target == "" then return nil, "Delete requires a comment/reply ID" end
  local root, root_err = current_repo()
  if not root then return nil, root_err end
  local branch, branch_err = guard_scope(root)
  if not branch then return nil, branch_err end
  local target_type, target_err = validate_target(root, branch, target)
  if not target_type then return nil, target_err end
  local id, id_err = create_id(root)
  if not id then return nil, id_err end
  return add_draft({ id = id, type = "delete", target = target }, {
    repo = root,
    commit = nil,
    path = nil,
    buffer = current_buffer(),
  }, branch)
end

function M.select_branch(name)
  if type(name) ~= "string" or name == "" then return nil, "A branch name is required" end
  if #state.drafts > 0 or state.composers > 0 then
    return nil, "Save or cancel all drafts/composers before switching branches"
  end
  local root, root_err = selected_root()
  if not root then return nil, root_err end
  local exists, err = branch_exists(root, name)
  if exists == nil then return nil, err end
  if not exists then
    return nil, ("Review branch '%s' does not exist; use create_branch('%s') or :RvBranchCreate %s"):format(
      name, name, name
    )
  end
  state.branch = { repo = root, name = name, create_armed = false }
  M.clear_marks()
  M.refresh_buffer(current_buffer(), false)
  return true
end

function M.create_branch(name)
  if type(name) ~= "string" or name == "" then return nil, "A branch name is required" end
  local root, root_err = selected_root()
  if not root then return nil, root_err end
  local same_scope = state.branch and state.branch.repo == root and state.branch.name == name
  if (#state.drafts > 0 or state.composers > 0) and not same_scope then
    return nil, "Save or cancel all drafts/composers before selecting/creating a branch"
  end
  local exists, err = branch_exists(root, name)
  if exists == nil then return nil, err end
  if exists then return nil, ("Review branch '%s' already exists; use :RvBranch %s"):format(name, name) end
  state.branch = { repo = root, name = name, create_armed = true }
  M.clear_marks()
  notify(("branch '%s' selected for explicit creation; it will be created only by :RvCommit"):format(name))
  return true
end

function M.save()
  if #state.drafts == 0 then return nil, "There are no review drafts to save" end
  local first = state.drafts[1]._context
  local root, branch = first.repo, first.branch
  if not root or not branch then return nil, "Draft has no repository/branch scope" end
  for _, action in ipairs(state.drafts) do
    local scope = action._context
    if not scope or scope.repo ~= root or scope.branch ~= branch then
      return nil, "Drafts from different repositories/branches cannot be committed together"
    end
  end
  if not state.branch or state.branch.repo ~= root or state.branch.name ~= branch then
    return nil, "Selected repository/branch changed since drafting; drafts retained, reselect their original branch"
  end

  local current_root, current_root_err = current_repo()
  if not current_root then
    return nil, ("Cannot verify current repository; drafts retained: %s"):format(tostring(current_root_err))
  end
  if current_root ~= root then
    return nil, "Current repository differs from draft repository; drafts retained"
  end
  local exists, exists_err = branch_exists(root, branch)
  if exists == nil then return nil, exists_err end
  if not exists and not state.branch.create_armed then
    return nil, ("Branch '%s' disappeared; drafts retained. Explicitly arm it with :RvBranchCreate %s"):format(branch, branch)
  end

  local reviewed, seen = {}, {}
  local input = {}
  for _, action in ipairs(state.drafts) do
    local clean = vim.deepcopy(action)
    clean._context = nil
    input[#input + 1] = vim.json.encode(clean)
    if action.type == "comment" and not seen[action.commit] then
      seen[action.commit] = true
      reviewed[#reviewed + 1] = action.commit
    end
  end
  local args = { "-b", branch }
  for _, commit in ipairs(reviewed) do
    vim.list_extend(args, { "--reviewed", commit })
  end
  if not exists then args[#args + 1] = "--create" end
  local result, err = cli.rv("commit", args, {
    cwd = root,
    stdin = table.concat(input, "\n") .. "\n",
    executable = state.config.command,
  })
  if not result then return nil, err end

  state.drafts = {}
  state.branch.create_armed = false
  notify(("review saved to '%s' as %s"):format(branch, tostring(result.commit or "review commit")))
  M.clear_marks()
  M.refresh_buffer(current_buffer(), false)
  return result
end

local function open_listing(lines, title)
  vim.cmd("botright new")
  local buf = current_buffer()
  vim.api.nvim_buf_set_name(buf, "rv://" .. title)
  vim.bo[buf].buftype = "nofile"
  vim.bo[buf].bufhidden = "wipe"
  vim.bo[buf].swapfile = false
  vim.bo[buf].filetype = "markdown"
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, #lines > 0 and lines or { "(no review activity)" })
  vim.bo[buf].modifiable = false
  return buf
end

local function format_thread(thread, depth, lines)
  local indent = string.rep("  ", depth)
  local anchor = thread.anchor
  local location = anchor and (" %s:%s-%s @ %s"):format(
    anchor.path or "?", tostring(anchor.start_line or "?"), tostring(anchor.end_line or "?"),
    tostring(anchor.commit or "?")
  ) or (" @ " .. tostring(thread.anchor and thread.anchor.commit or thread.commit or "commit"))
  table.insert(lines, indent .. "- " .. tostring(thread.author and thread.author.name or "reviewer") .. location)
  table.insert(lines, indent .. "  " .. tostring(thread.deleted and "[deleted]" or thread.body or "(no body)"))
  for _, reply in ipairs(thread.replies or {}) do format_thread(reply, depth + 1, lines) end
end

function M.show()
  local root, root_err = selected_root()
  if not root then return nil, root_err end
  local branch, err = branch_for(root)
  if not branch then return nil, err end
  local result, show_err = render.show(root, branch.name, nil, nil, state.config.command)
  if not result then return nil, show_err end
  local lines = { ("# Review branch %s (%s)"):format(branch.name, tostring(result.tip or "no tip")), "" }
  for _, thread in ipairs(result.threads or {}) do format_thread(thread, 0, lines) end
  for _, action in ipairs(state.drafts) do
    local kind = action.type == "comment" and "comment" or action.type
    table.insert(lines, ("- [draft %s] %s %s"):format(kind, tostring(action.id),
      action.body and action.body:gsub("\n.*", "") or ""))
  end
  open_listing(lines, "show/" .. branch.name)
  return result
end

function M.drafts_view()
  local lines = { "# Unsaved rv drafts", "" }
  for _, action in ipairs(state.drafts) do
    local scope = action._context or {}
    table.insert(lines, ("- %s %s on %s/%s"):format(
      action.type, tostring(action.id), tostring(scope.repo or "?"), tostring(scope.branch or "?")
    ))
    if action.body then table.insert(lines, "  " .. action.body:gsub("\n", "\n  ")) end
  end
  open_listing(lines, "drafts")
  return true
end

function M.clear_marks()
  for bufnr in pairs(rendered_buffers) do
    if vim.api.nvim_buf_is_valid(bufnr) then
      vim.api.nvim_buf_clear_namespace(bufnr, render.namespace(), 0, -1)
    end
  end
  rendered_buffers = {}
end

function M.refresh_buffer(bufnr, report_errors)
  if not bufnr or not vim.api.nvim_buf_is_valid(bufnr) then return nil end
  local diff_context, diff_err = context.from_diffview(bufnr)
  local target
  if diff_context == false then
    vim.api.nvim_buf_clear_namespace(bufnr, render.namespace(), 0, -1)
    if report_errors then notify(diff_err, vim.log.levels.ERROR) end
    return nil, diff_err
  elseif diff_context then
    target = diff_context
  else
    target = context.for_buffer(bufnr, { autosave_on_comment = false })
    if not target then
      vim.api.nvim_buf_clear_namespace(bufnr, render.namespace(), 0, -1)
      return nil
    end
  end

  local branch, branch_err = branch_for(target.repo)
  if not branch then
    vim.api.nvim_buf_clear_namespace(bufnr, render.namespace(), 0, -1)
    if report_errors then notify(branch_err, vim.log.levels.ERROR) end
    return nil, branch_err
  end
  local outcome, err = render.render_buffer(
    bufnr, target.repo, branch.name, target.commit, target.path, state.drafts, state.config.command,
    branch.create_armed
  )
  if not outcome then
    vim.api.nvim_buf_clear_namespace(bufnr, render.namespace(), 0, -1)
    if report_errors then notify(err, vim.log.levels.ERROR) end
    return nil, err
  end
  rendered_buffers[bufnr] = true
  if outcome.error and report_errors then notify(outcome.error, vim.log.levels.ERROR) end
  return outcome
end

function M.refresh()
  return M.refresh_buffer(current_buffer(), true)
end

function M.get_drafts()
  local result = {}
  for i, action in ipairs(state.drafts) do
    result[i] = vim.deepcopy(action)
  end
  return result
end

function M.get_state()
  return {
    branch = state.branch and vim.deepcopy(state.branch) or nil,
    drafts = M.get_drafts(),
    composers = state.composers,
    config = vim.deepcopy(state.config),
  }
end

local function command_call(fn, ...)
  local ok, result, err = pcall(fn, ...)
  if not ok then
    notify(result, vim.log.levels.ERROR)
    return
  end
  if not result then
    notify(err or "operation failed", vim.log.levels.ERROR)
  end
end

local function define_commands()
  vim.api.nvim_create_user_command("RvBranch", function(cmd)
    command_call(M.select_branch, cmd.args)
  end, { nargs = 1, desc = "Select an existing rv review branch" })
  vim.api.nvim_create_user_command("RvBranchCreate", function(cmd)
    command_call(M.create_branch, cmd.args)
  end, { nargs = 1, desc = "Explicitly arm creation of an rv review branch" })
  vim.api.nvim_create_user_command("RvComment", function(cmd)
    local opts = { buffer = current_buffer() }
    if cmd.range > 0 then
      opts.start_line, opts.end_line = cmd.line1, cmd.line2
    end
    command_call(M.comment, opts)
  end, { range = true, nargs = 0, desc = "Draft a comment on the current new-side range" })
  vim.api.nvim_create_user_command("RvReply", function(cmd)
    command_call(M.reply, cmd.args, { buffer = current_buffer() })
  end, { nargs = 1, desc = "Draft a reply to a comment or reply ID" })
  vim.api.nvim_create_user_command("RvDelete", function(cmd)
    command_call(M.delete, cmd.args)
  end, { nargs = 1, desc = "Draft a logical deletion of a comment or reply" })
  vim.api.nvim_create_user_command("RvCommit", function()
    command_call(M.save)
  end, { nargs = 0, desc = "Explicitly save all rv drafts" })
  vim.api.nvim_create_user_command("RvShow", function()
    command_call(M.show)
  end, { nargs = 0, desc = "Show saved review threads" })
  vim.api.nvim_create_user_command("RvDrafts", function()
    command_call(M.drafts_view)
  end, { nargs = 0, desc = "List in-memory review drafts" })
  vim.api.nvim_create_user_command("RvRefresh", function()
    command_call(M.refresh)
  end, { nargs = 0, desc = "Refresh review extmarks in the current buffer" })
end

local function install_autocmds()
  command_group = vim.api.nvim_create_augroup("rv_review_comments", { clear = true })
  vim.api.nvim_create_autocmd({ "BufEnter", "BufWinEnter", "BufWritePost" }, {
    group = command_group,
    callback = function(event)
      if vim.bo[event.buf].buftype == "" then
        vim.schedule(function()
          if vim.api.nvim_buf_is_valid(event.buf) then M.refresh_buffer(event.buf, false) end
        end)
      end
    end,
  })
  for _, pattern in ipairs({
    "DiffviewViewOpened", "DiffviewViewEnter", "DiffviewViewPostLayout",
    "DiffviewDiffBufWinEnter", "DiffviewSelectionChanged",
  }) do
    vim.api.nvim_create_autocmd("User", {
      group = command_group,
      pattern = pattern,
      callback = function()
        vim.schedule(function() M.refresh_buffer(current_buffer(), false) end)
      end,
    })
  end
end

function M.setup(opts)
  opts = opts or {}
  local new_config = vim.tbl_extend("force", state.config, opts)
  if new_config.autosave_on_comment ~= nil and type(new_config.autosave_on_comment) ~= "boolean" then
    error("rv.setup: autosave_on_comment must be boolean")
  end
  if new_config.command ~= nil and type(new_config.command) ~= "string" then
    error("rv.setup: command must be an executable string")
  end
  if opts.branch ~= nil then
    if type(opts.branch) ~= "string" or opts.branch == "" then error("rv.setup: branch must be a name") end
    if #state.drafts > 0 or state.composers > 0 then error("rv.setup: cannot change branch with active drafts") end
    local root = current_repo()
    state.branch = { repo = root, name = opts.branch, create_armed = false }
  end
  state.config = new_config
  if not state.setup then
    define_commands()
    install_autocmds()
    state.setup = true
  end
  if opts.branch then M.refresh_buffer(current_buffer(), false) end
  return M
end

return M
