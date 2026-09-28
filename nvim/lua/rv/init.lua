local cli = require("rv.cli")
local context = require("rv.context")
local render = require("rv.render")

local M = {}
local state = {
  config = { command = "rv", autosave_on_comment = false, commit_on_close = false, display = "sidebar" },
  branch = nil,
  drafts = {},
  composers = 0,
  setup = false,
  sidebar_enabled = true,
}
local rendered_buffers = {}
local command_group
local sidebar_buf, sidebar_win, sidebar_source_win, sidebar_rows
local pending_ns = vim.api.nvim_create_namespace("rv-pending-comment")

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
  if composer.pending_buf and vim.api.nvim_buf_is_valid(composer.pending_buf) then
    vim.api.nvim_buf_del_extmark(composer.pending_buf, pending_ns, composer.pending_mark)
  end
  state.composers = math.max(0, state.composers - 1)
end

local function dismiss_composer(composer)
  close_composer(composer)
  -- Close only the composer split, never delete the active source window's
  -- buffer: nvim_buf_delete on a displayed buffer can replace/close the wrong
  -- window when autocommands have opened a sidebar in the meantime.
  if composer.win and vim.api.nvim_win_is_valid(composer.win) then
    if vim.api.nvim_get_current_win() == composer.win and composer.origin
      and vim.api.nvim_win_is_valid(composer.origin) then
      vim.api.nvim_set_current_win(composer.origin)
    end
    pcall(vim.api.nvim_win_close, composer.win, true)
  end
  if composer.buf and vim.api.nvim_buf_is_valid(composer.buf) then
    pcall(vim.api.nvim_buf_delete, composer.buf, { force = true })
  end
end

local function finalize_composer(composer, body, from_wipe)
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
  local added, err
  if composer.edit_id then
    local original = state.drafts[composer.edit_index]
    if not original or original.id ~= composer.edit_id then
      return nil, "Draft is no longer available for editing"
    end
    original.body = body
    added = original
    if original._context and vim.api.nvim_buf_is_valid(original._context.buffer) then
      M.refresh_buffer(original._context.buffer, false)
    end
  else
    added, err = add_draft(action, composer.context, branch)
  end
  if not added then return nil, err end
  if from_wipe then
    close_composer(composer)
  else
    dismiss_composer(composer)
  end
  local function finish()
    if composer.commit_on_close then
      -- BufWipeout runs before Neovim returns focus to the source split.
      if #state.drafts > 0 then
        local committed, commit_err = M.save()
        if not committed then
          notify("commit on close failed; drafts retained: " .. tostring(commit_err), vim.log.levels.ERROR)
        end
      end
    else
      notify("draft added (not saved; use :RvCommit)")
    end
    if vim.api.nvim_buf_is_valid(composer.context.buffer) then
      M.refresh_buffer(composer.context.buffer, false)
    end
  end
  if from_wipe then vim.schedule(finish) else finish() end
  return added
end

local function open_composer(action, draft_context, branch, title, edit_index)
  local composer = {
    action = action,
    context = draft_context,
    repo = draft_context.repo,
    branch = branch.name,
    closed = false,
    commit_on_close = state.config.commit_on_close,
    origin = vim.api.nvim_get_current_win(),
    edit_index = edit_index,
    edit_id = edit_index and action.id or nil,
  }
  if action.anchor and vim.api.nvim_buf_is_valid(draft_context.buffer) and not edit_index then
    local anchor = action.anchor
    composer.pending_buf = draft_context.buffer
    composer.pending_mark = vim.api.nvim_buf_set_extmark(draft_context.buffer, pending_ns,
      anchor.start_line - 1, 0, {
        end_row = anchor.end_line, hl_group = "Visual", sign_text = "✎",
        sign_hl_group = "DiagnosticWarn", priority = 160,
      })
  end
  state.composers = state.composers + 1
  vim.cmd("botright new")
  local buf = current_buffer()
  composer.buf = buf
  composer.win = vim.api.nvim_get_current_win()
  vim.api.nvim_create_autocmd("BufWipeout", {
    buffer = buf,
    once = true,
    callback = function()
      if composer.closed then return end
      if composer.commit_on_close then
        local ok, lines = pcall(vim.api.nvim_buf_get_lines, buf, 0, -1, false)
        local body = ok and table.concat(lines, "\n") or ""
        if body:gsub("%s", "") ~= "" then
          local saved, err = finalize_composer(composer, body, true)
          if not saved then notify(err, vim.log.levels.ERROR) end
        end
      end
      close_composer(composer)
    end,
  })
  vim.api.nvim_buf_set_name(buf, ("rv://%s/%s/%d"):format(title, action.id, buf))
  -- nofile buffers can close normally while modified; acwrite would block
  -- :close with E37 before the wipe callback gets a chance to save.
  vim.bo[buf].buftype = composer.commit_on_close and "nofile" or "acwrite"
  vim.bo[buf].bufhidden = "wipe"
  vim.bo[buf].swapfile = false
  vim.bo[buf].filetype = "markdown"
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, vim.split(action.body or "", "\n", { plain = true }))
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
    if composer.commit_on_close then
      local body = table.concat(vim.api.nvim_buf_get_lines(buf, 0, -1, false), "\n")
      local saved, err = finalize_composer(composer, body)
      if not saved then notify(err, vim.log.levels.ERROR) end
    else
      vim.cmd("write")
    end
  end, { buffer = buf, silent = true, desc = "Add rv draft" })
  vim.keymap.set("n", "<C-c>", function()
    dismiss_composer(composer)
  end, { buffer = buf, silent = true, desc = "Cancel rv draft" })
  notify(("%s in Markdown scratch buffer; <C-s> %s, <C-c> cancels"):format(title,
    composer.commit_on_close and "commits the review (as does closing)" or "saves the draft"))
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

function M.edit_draft(id)
  if type(id) ~= "string" or id == "" then return nil, "A draft ID is required" end
  for index, action in ipairs(state.drafts) do
    if action.id == id then
      if action.type == "delete" then return nil, "Delete drafts have no body to edit" end
      local scope = action._context
      local branch = state.branch
      if not branch or branch.repo ~= scope.repo or branch.name ~= scope.branch then
        return nil, "Select the draft's original repository and branch first"
      end
      return open_composer(vim.deepcopy(action), {
        repo = scope.repo, commit = scope.commit, path = scope.path, buffer = scope.buffer,
      }, branch, "edit", index)
    end
  end
  return nil, "No unsaved draft with ID " .. id
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
  M.refresh_buffer(current_buffer(), false)
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

local function open_listing(lines, title, edit_rows)
  local buf
  if state.config.display == "sidebar" then
    if not sidebar_win or not vim.api.nvim_win_is_valid(sidebar_win)
      or vim.api.nvim_win_get_tabpage(sidebar_win) ~= vim.api.nvim_get_current_tabpage() then
      local source_win = vim.api.nvim_get_current_win()
      sidebar_buf = nil
      vim.cmd("botright vsplit")
      sidebar_win = vim.api.nvim_get_current_win()
      vim.api.nvim_win_set_width(sidebar_win, 42)
      vim.wo[sidebar_win].wrap = false
      vim.wo[sidebar_win].number = false
      vim.wo[sidebar_win].relativenumber = false
      vim.wo[sidebar_win].foldenable = false
      buf = vim.api.nvim_create_buf(false, true)
      vim.api.nvim_win_set_buf(sidebar_win, buf)
      vim.api.nvim_set_current_win(source_win)
    else
      buf = sidebar_buf
      if not buf or not vim.api.nvim_buf_is_valid(buf) then
        buf = vim.api.nvim_create_buf(false, true)
        vim.api.nvim_win_set_buf(sidebar_win, buf)
      elseif vim.api.nvim_win_get_buf(sidebar_win) ~= buf then
        vim.api.nvim_win_set_buf(sidebar_win, buf)
      end
    end
    sidebar_buf = buf
  else
    vim.cmd("botright new")
    buf = current_buffer()
  end
  -- Keep a stable, unique name: renaming a loaded sidebar to a listing name
  -- collides with an older sidebar buffer when returning to a file/tab.
  if vim.api.nvim_buf_get_name(buf) == "" then
    vim.api.nvim_buf_set_name(buf, "rv://sidebar/" .. buf)
  end
  if title ~= "review" then sidebar_rows = nil end
  vim.bo[buf].buftype = "nofile"
  vim.bo[buf].bufhidden = "wipe"
  vim.bo[buf].swapfile = false
  vim.bo[buf].filetype = "markdown"
  vim.bo[buf].modifiable = true
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, #lines > 0 and lines or { "(no review activity)" })
  vim.bo[buf].modifiable = false
  vim.keymap.set("n", "<CR>", function()
    local id = edit_rows and edit_rows[vim.api.nvim_win_get_cursor(0)[1]]
    if id then
      local opened, err = M.edit_draft(id)
      if not opened then notify(err, vim.log.levels.ERROR) end
    end
  end, { buffer = buf, silent = true, desc = "Edit review draft on this line" })
  return buf
end

local function format_thread(thread, depth, lines)
  local indent = string.rep("  ", depth)
  local anchor = thread.mapped and thread.mapped.start_line and thread.mapped or thread.anchor
  local location = anchor and (" %s:%s-%s @ %s"):format(
    anchor.path or "?", tostring(anchor.start_line or "?"), tostring(anchor.end_line or "?"),
    tostring(anchor.commit or "?")
  ) or (" @ " .. tostring(thread.anchor and thread.anchor.commit or thread.commit or "commit"))
  table.insert(lines, indent .. "- " .. tostring(thread.author and thread.author.name or "reviewer") .. location)
  for _, body_line in ipairs(vim.split(tostring(thread.deleted and "[deleted]" or thread.body or "(no body)"), "\n", { plain = true })) do
    table.insert(lines, indent .. "  " .. body_line)
  end
  for _, reply in ipairs(thread.replies or {}) do format_thread(reply, depth + 1, lines) end
end

function M.show()
  state.sidebar_enabled = true
  local root, root_err = selected_root()
  if not root then return nil, root_err end
  local branch, err = branch_for(root)
  if not branch then return nil, err end
  local result, show_err = render.show(root, branch.name, nil, nil, state.config.command)
  if not result then return nil, show_err end
  local lines = { ("# Review branch %s (%s)"):format(branch.name, tostring(result.tip or "no tip")), "" }
  for _, thread in ipairs(result.threads or {}) do format_thread(thread, 0, lines) end
  local edit_rows = {}
  for _, action in ipairs(state.drafts) do
    local scope = action._context or {}
    if scope.repo == root and scope.branch == branch.name then
      table.insert(lines, ("- [draft %s] %s:%s-%s"):format(action.type,
        tostring(action.anchor and action.anchor.path or ""),
        tostring(action.anchor and action.anchor.start_line or ""),
        tostring(action.anchor and action.anchor.end_line or "")))
      if action.type ~= "delete" then edit_rows[#lines] = action.id end
      for _, body_line in ipairs(vim.split(action.body or "", "\n", { plain = true })) do
        table.insert(lines, "  " .. body_line)
      end
    end
  end
  open_listing(lines, "show", edit_rows)
  return result
end

function M.branches_view()
  state.sidebar_enabled = true
  local root, err = selected_root()
  if not root then return nil, err end
  local names, list_err = branch_names(root)
  if not names then return nil, list_err end
  local lines = { "# Review branches", "", "Select with :RvBranch NAME", "" }
  for _, name in ipairs(names) do
    table.insert(lines, (state.branch and state.branch.repo == root and state.branch.name == name
      and "* " or "- ") .. name)
  end
  if #names == 0 then table.insert(lines, "(no branches; use :RvBranchCreate NAME)") end
  open_listing(lines, "branches")
  return names
end

function M.drafts_view()
  state.sidebar_enabled = true
  local lines = { "# Unsaved rv drafts", "" }
  local edit_rows = {}
  for _, action in ipairs(state.drafts) do
    local scope = action._context or {}
    table.insert(lines, ("- %s on %s/%s"):format(
      action.type, tostring(scope.repo or "?"), tostring(scope.branch or "?")
    ))
    if action.type ~= "delete" then edit_rows[#lines] = action.id end
    if action.body then table.insert(lines, "  " .. action.body:gsub("\n", "\n  ")) end
  end
  open_listing(lines, "drafts", edit_rows)
  return true
end

local function sync_sidebar()
  if not sidebar_rows or not sidebar_source_win or not sidebar_win
    or not vim.api.nvim_win_is_valid(sidebar_source_win) or not vim.api.nvim_win_is_valid(sidebar_win)
    or vim.api.nvim_win_get_tabpage(sidebar_source_win) ~= vim.api.nvim_win_get_tabpage(sidebar_win) then
    return
  end
  local view = vim.api.nvim_win_call(sidebar_source_win, vim.fn.winsaveview)
  local row = sidebar_rows[view.topline]
  if row then
    vim.api.nvim_win_call(sidebar_win, function()
      if vim.fn.winsaveview().topline ~= row then
        vim.fn.winrestview({ topline = row, lnum = row, col = 0 })
      end
    end)
  end
end

local function file_sidebar(bufnr, target, branch, threads, drafts)
  local count = vim.api.nvim_buf_line_count(bufnr)
  local comments, draft_rows = {}, {}
  local function put(line, entries, id)
    if type(line) ~= "number" or line < 1 or line > count then return end
    comments[line] = comments[line] or {}
    if id then
      draft_rows[line] = draft_rows[line] or {}
      draft_rows[line][#comments[line] + 1] = id
    end
    vim.list_extend(comments[line], entries)
  end
  local function body_lines(body, prefix)
    local lines = vim.split(tostring(body or "(no body)"), "\n", { plain = true })
    for i, line in ipairs(lines) do lines[i] = (i == 1 and prefix or "    ") .. line end
    return lines
  end
  local function reply_lines(replies, entries, depth)
    for _, reply in ipairs(replies or {}) do
      vim.list_extend(entries, body_lines(reply.deleted and "[deleted reply]" or reply.body,
        string.rep("  ", depth) .. "↳ "))
      reply_lines(reply.replies, entries, depth + 1)
    end
  end
  for _, thread in ipairs(threads or {}) do
    local mapped = thread.mapped
    if mapped and mapped.path == target.path and mapped.status ~= "deleted"
      and mapped.status ~= "file_deleted" and mapped.status ~= "binary" then
      local entries = body_lines(thread.deleted and "[deleted comment]" or thread.body, "● ")
      reply_lines(thread.replies, entries, 1)
      put(mapped.start_line, entries)
    end
  end
  for _, action in ipairs(drafts) do
    local scope = action._context or {}
    if action.type == "comment" and action.anchor and scope.repo == target.repo
      and scope.branch == branch.name and action.commit == target.commit
      and action.anchor.path == target.path then
      put(action.anchor.start_line, body_lines(action.body, "✎ [draft] "), action.id)
    end
  end
  local lines, rows, edit_rows = {}, {}, {}
  for line = 1, count do
    rows[line] = #lines + 1
    local entries = comments[line]
    if entries then
      for i, entry in ipairs(entries) do
        lines[#lines + 1] = entry
        if draft_rows[line] then edit_rows[#lines] = draft_rows[line][i] end
      end
    else
      lines[#lines + 1] = ""
    end
  end
  open_listing(lines, "review", edit_rows)
  sidebar_rows = rows
  local win = vim.fn.bufwinid(bufnr)
  if win ~= -1 then sidebar_source_win = win end
  sync_sidebar()
end

function M.close_sidebar()
  state.sidebar_enabled = false
  sidebar_rows = nil
  if sidebar_win and vim.api.nvim_win_is_valid(sidebar_win) then
    local win = sidebar_win
    if vim.api.nvim_get_current_win() == win and sidebar_source_win
      and vim.api.nvim_win_is_valid(sidebar_source_win) then
      vim.api.nvim_set_current_win(sidebar_source_win)
    end
    sidebar_win = nil
    sidebar_buf = nil
    vim.api.nvim_win_close(win, true)
  end
  return true
end

function M.open_sidebar()
  if state.config.display ~= "sidebar" then return nil, "Sidebar requires display = 'sidebar'" end
  state.sidebar_enabled = true
  local bufnr = current_buffer()
  if sidebar_win and vim.api.nvim_win_is_valid(sidebar_win)
    and bufnr == vim.api.nvim_win_get_buf(sidebar_win) then
    if not sidebar_source_win or not vim.api.nvim_win_is_valid(sidebar_source_win) then
      return nil, "Open a source file first"
    end
    bufnr = vim.api.nvim_win_get_buf(sidebar_source_win)
  end
  if not state.branch then return M.branches_view() end
  local result, err
  if current_buffer() ~= bufnr and sidebar_source_win and vim.api.nvim_win_is_valid(sidebar_source_win) then
    result, err = vim.api.nvim_win_call(sidebar_source_win, function()
      return M.refresh_buffer(bufnr, true)
    end)
  else
    result, err = M.refresh_buffer(bufnr, true)
  end
  if not result then return nil, err or "Open a reviewable source file first" end
  return true
end

function M.toggle_sidebar()
  if state.sidebar_enabled and sidebar_win and vim.api.nvim_win_is_valid(sidebar_win) then
    return M.close_sidebar()
  end
  return M.open_sidebar()
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
    branch.create_armed, state.config.display == "sidebar"
  )
  if not outcome then
    vim.api.nvim_buf_clear_namespace(bufnr, render.namespace(), 0, -1)
    if report_errors then notify(err, vim.log.levels.ERROR) end
    return nil, err
  end
  rendered_buffers[bufnr] = true
  local active_name = vim.api.nvim_buf_get_name(current_buffer())
  if state.config.display == "sidebar" and state.sidebar_enabled and (current_buffer() == bufnr
    or (sidebar_win and vim.api.nvim_win_is_valid(sidebar_win)
      and vim.fn.bufwinid(bufnr) ~= -1 and active_name:match("^rv://(comment|edit|reply)/"))) then
    file_sidebar(bufnr, target, branch, outcome.result.threads, state.drafts)
  end
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
    if cmd.args == "" then
      command_call(M.branches_view)
    else
      command_call(M.select_branch, cmd.args)
    end
  end, { nargs = "?", desc = "List branches or select an existing rv review branch" })
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
  vim.api.nvim_create_user_command("RvEdit", function(cmd)
    command_call(M.edit_draft, cmd.args)
  end, { nargs = 1, desc = "Edit an unsaved comment or reply by ID" })
  vim.api.nvim_create_user_command("RvCommit", function()
    command_call(M.save)
  end, { nargs = 0, desc = "Explicitly save all rv drafts" })
  vim.api.nvim_create_user_command("RvShow", function()
    command_call(M.show)
  end, { nargs = 0, desc = "Show saved review threads" })
  vim.api.nvim_create_user_command("RvDrafts", function()
    command_call(M.drafts_view)
  end, { nargs = 0, desc = "List in-memory review drafts" })
  vim.api.nvim_create_user_command("RvSidebarOpen", function()
    command_call(M.open_sidebar)
  end, { nargs = 0, desc = "Open the review sidebar" })
  vim.api.nvim_create_user_command("RvSidebarClose", function()
    command_call(M.close_sidebar)
  end, { nargs = 0, desc = "Close the review sidebar" })
  vim.api.nvim_create_user_command("RvSidebarToggle", function()
    command_call(M.toggle_sidebar)
  end, { nargs = 0, desc = "Toggle the review sidebar" })
  vim.api.nvim_create_user_command("RvRefresh", function()
    command_call(M.refresh)
  end, { nargs = 0, desc = "Refresh review extmarks in the current buffer" })
end

local function install_autocmds()
  command_group = vim.api.nvim_create_augroup("rv_review_comments", { clear = true })
  vim.api.nvim_create_autocmd("WinClosed", {
    group = command_group,
    callback = function(event)
      if tonumber(event.match) == sidebar_win then
        sidebar_win, sidebar_buf, sidebar_rows = nil, nil, nil
        state.sidebar_enabled = false
      end
    end,
  })
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
  vim.api.nvim_create_autocmd({ "CursorMoved", "WinScrolled" }, {
    group = command_group,
    callback = function() sync_sidebar() end,
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
  if type(new_config.commit_on_close) ~= "boolean" then
    error("rv.setup: commit_on_close must be boolean")
  end
  if new_config.display ~= "sidebar" and new_config.display ~= "inline" then
    error("rv.setup: display must be sidebar or inline")
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
