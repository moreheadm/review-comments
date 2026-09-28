local cli = require("rv.cli")

local M = {}
local OID_LEN = { [40] = true, [64] = true }

local function is_oid(value)
  return type(value) == "string"
    and OID_LEN[#value] == true
    and value:match("^[0-9a-fA-F]+$") ~= nil
end

local function trim_output(value)
  return (value or ""):gsub("%s+$", "")
end

local function normalize(path)
  return vim.fs.normalize(path):gsub("\\", "/")
end

local function canonical(path)
  return vim.uv.fs_realpath(path) or normalize(path)
end

local function repository_root(path)
  local cwd = vim.fn.fnamemodify(path, ":h")
  local root = cli.capture("jj", { "root" }, cwd)
  if root then
    root = trim_output(root)
    if root ~= "" then return canonical(root), "jj" end
  end

  root = cli.capture("git", { "rev-parse", "--show-toplevel" }, cwd)
  if not root then
    return nil, nil, "This buffer is not inside a jj or Git repository"
  end
  root = trim_output(root)
  if root == "" then
    return nil, nil, "Git returned an empty repository root"
  end
  return canonical(root), "git"
end

local function relative_path(root, path)
  local absolute = canonical(path)
  local prefix = root:gsub("/+$", "") .. "/"
  if absolute == root then return nil end
  if absolute:sub(1, #prefix) ~= prefix then return nil end
  local relative = absolute:sub(#prefix + 1)
  if relative == "" or relative:match("^%.%./") or relative:find("/../", 1, true) then
    return nil
  end
  return relative
end

local function buffer_bytes(bufnr)
  local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
  local format = vim.bo[bufnr].fileformat
  local newline = format == "dos" and "\r\n" or format == "mac" and "\r" or "\n"
  local text = table.concat(lines, newline)
  if vim.bo[bufnr].endofline then text = text .. newline end
  if vim.bo[bufnr].bomb then text = "\239\187\191" .. text end
  local encoding = vim.bo[bufnr].fileencoding
  if encoding ~= "" and encoding:lower() ~= "utf-8" and encoding:lower() ~= "utf8" then
    local ok, converted = pcall(vim.iconv, text, "utf-8", encoding)
    if not ok or converted == nil then
      return nil, ("Cannot encode buffer as %s"):format(encoding)
    end
    text = converted
  end
  return text
end

local function buffer_matches_snapshot(bufnr, snapshot)
  local content, err = buffer_bytes(bufnr)
  if not content then return nil, err end
  if content == snapshot then return true end
  -- Neovim represents a zero-byte file as one empty line with 'endofline'
  -- set, indistinguishable from a one-newline file using buffer APIs alone.
  local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
  if #lines == 1 and lines[1] == "" and vim.bo[bufnr].endofline then
    if snapshot == "" then return true end
    if vim.bo[bufnr].bomb and snapshot == "\239\187\191" then return true end
  end
  return false
end

local function snapshot_revision(root, vcs)
  local output, err
  if vcs == "jj" then
    output, err = cli.capture("jj", {
      "log", "-r", "@", "--no-graph", "-T", 'commit_id ++ "\\n"',
    }, root)
  else
    output, err = cli.capture("git", { "rev-parse", "--verify", "HEAD^{commit}" }, root)
  end
  if not output then return nil, err end
  local revision = trim_output(output)
  if not is_oid(revision) then
    return nil, ("Could not resolve a full %s commit ID"):format(vcs)
  end
  return revision:lower()
end

local function read_snapshot(root, vcs, revision, path)
  local output, err
  if vcs == "jj" then
    output, err = cli.capture("jj", { "file", "show", "--revision", revision, path }, root)
  else
    output, err = cli.capture("git", { "cat-file", "blob", revision .. ":" .. path }, root)
  end
  if not output then
    return nil, err or "The file does not exist in the pinned snapshot"
  end
  return output
end

local function is_diffview_buffer(bufnr)
  return vim.api.nvim_buf_get_name(bufnr):match("^diffview://") ~= nil
end

local function unsupported_diffview(message)
  return false, message
    or "Cannot identify this Diffview pane/layout (fail closed; placement is disabled rather than guessed)"
end

local function current_window_for_buffer(bufnr, requested_winid)
  local api = vim.api
  if requested_winid then
    if api.nvim_win_is_valid(requested_winid) and api.nvim_win_get_buf(requested_winid) == bufnr then
      return requested_winid
    end
    return nil, "Requested window does not display the supplied Diffview buffer"
  end

  local current = api.nvim_get_current_win()
  if api.nvim_win_get_buf(current) == bufnr then return current end
  local matches = {}
  for _, winid in ipairs(api.nvim_tabpage_list_wins(api.nvim_get_current_tabpage())) do
    if api.nvim_win_get_buf(winid) == bufnr then matches[#matches + 1] = winid end
  end
  if #matches == 1 then return matches[1] end
  if #matches > 1 then return nil, "Diffview buffer is shown in multiple windows; pane identity is ambiguous" end
  return nil, "Diffview buffer is not visible in the current tab"
end

local function pane_count(layout)
  return type(layout) == "table" and type(layout.windows) == "table" and #layout.windows or 0
end

local function diffview_file_context(bufnr, view, requested_winid)
  local recognized = is_diffview_buffer(bufnr)
  if type(view) ~= "table" then
    if recognized then return unsupported_diffview() end
    return nil
  end
  local winid, win_err = current_window_for_buffer(bufnr, requested_winid)
  if not winid then
    if recognized then return unsupported_diffview(win_err) end
    return nil
  end
  local layout = view.cur_layout
  if type(layout) == "table" and type(layout.windows) == "table" then
    for _, win in ipairs(layout.windows) do
      if win and win.id == winid then
        -- Window membership is authoritative even if its File buffer object
        -- is stale or its internals changed; never treat that pane as a normal
        -- source buffer in that state.
        recognized = true
        break
      end
    end
  end

  local entry = view.cur_entry
  local entry_layout = type(entry) == "table" and entry.layout or nil
  if not recognized and type(entry_layout) == "table" and type(entry_layout.windows) == "table" then
    for _, win in ipairs(entry_layout.windows) do
      if win and (win.id == winid or win.file and win.file.bufnr == bufnr) then
        recognized = true
        break
      end
    end
  end
  if not recognized and view.tabpage == vim.api.nvim_win_get_tabpage(winid)
    and (type(entry) ~= "table" or type(layout) ~= "table") then
    return unsupported_diffview("Active Diffview view is missing pane internals; normal-buffer fallback is disabled")
  end
  if not recognized then return nil end

  if type(entry_layout) ~= "table" or type(layout) ~= "table" then
    return unsupported_diffview()
  end

  -- Only the ordinary two-sided a/b comparison is supported. Inline and
  -- three-/four-way merge layouts do not provide the unambiguous review side.
  if pane_count(layout) ~= 2 or pane_count(entry_layout) ~= 2
    or type(layout.a) ~= "table" or type(layout.b) ~= "table"
    or type(entry_layout.a) ~= "table" or type(entry_layout.b) ~= "table"
    or layout.a.id == nil or layout.b.id == nil or layout.a.id == layout.b.id then
    return unsupported_diffview("Unsupported Diffview layout (only a two-pane a/b diff is supported)")
  end

  if view.tabpage and vim.api.nvim_win_get_tabpage(winid) ~= view.tabpage then
    return unsupported_diffview("Window does not belong to the active Diffview tab")
  end

  local side, pane
  if winid == layout.a.id then
    side, pane = "a", layout.a
  elseif winid == layout.b.id then
    side, pane = "b", layout.b
  else
    return unsupported_diffview("Current window is not one of the active two-pane Diffview windows")
  end
  local matched = pane.file
  if type(matched) ~= "table" or matched.bufnr ~= bufnr then
    return unsupported_diffview("Active Diffview window/file identity is inconsistent")
  end
  if matched.symbol and matched.symbol ~= side then
    return unsupported_diffview("Diffview pane symbol conflicts with its active window identity")
  end
  if side ~= "b" then
    return false, "Review comments are only allowed on the Diffview new side (old-side pane refused)"
  end
  if entry.status == "D" or matched.status == "D" or matched.nulled or matched.path == "null" then
    return false, "Cannot comment on a deleted file or empty diff side"
  end
  if matched.binary then return false, "Cannot comment on a binary Diffview buffer" end
  local name = vim.api.nvim_buf_get_name(bufnr)
  if name:match("^diffview://null") then
    return false, "Cannot comment on a deleted file or empty diff side"
  end

  local revision = matched.rev and matched.rev.commit
  if not is_oid(revision) then
    return false, "The displayed new side is not pinned to a commit; open a commit-to-commit diff"
  end
  local path = matched.path or entry.path
  if type(path) ~= "string" or path == "" or path:sub(1, 1) == "/" then
    return false, "Diffview did not provide a repository-relative new-side path"
  end
  for part in path:gmatch("[^/]+") do
    if part == ".." or part == "." then
      return false, "Diffview provided an unsafe new-side path"
    end
  end

  local repo = view.adapter and view.adapter.ctx and view.adapter.ctx.toplevel
  if type(repo) ~= "string" or repo == "" then
    return false, "Diffview did not provide a repository root"
  end
  repo = canonical(repo)
  local snapshot, snapshot_err = cli.capture(
    "git", { "cat-file", "blob", revision .. ":" .. path }, repo
  )
  if not snapshot then
    return false, ("Could not read displayed new-side file from pinned commit: %s"):format(
      tostring(snapshot_err)
    )
  end
  local matches, match_err = buffer_matches_snapshot(bufnr, snapshot)
  if matches == nil then return false, match_err end
  if not matches then
    return false, "Diffview buffer contents do not match the displayed new-side commit snapshot"
  end

  return {
    kind = "diffview",
    repo = repo,
    vcs = "git",
    commit = revision:lower(),
    path = path,
    side = side,
    buffer = bufnr,
    winid = winid,
  }
end

function M.from_diffview(bufnr, view, winid)
  if view == nil then
    local ok, lib = pcall(require, "diffview.lib")
    if ok and type(lib.get_current_view) == "function" then
      local called, current = pcall(lib.get_current_view)
      if called then view = current end
    end
  end
  if not view then
    if is_diffview_buffer(bufnr) then return unsupported_diffview() end
    return nil
  end
  return diffview_file_context(bufnr, view, winid)
end

function M.for_buffer(bufnr, opts)
  opts = opts or {}
  local diff_context, diff_err = M.from_diffview(bufnr, nil, opts.winid)
  if diff_context == false then return nil, diff_err end
  if diff_context then return diff_context end

  local name = vim.api.nvim_buf_get_name(bufnr)
  if vim.bo[bufnr].buftype ~= "" or name == "" then
    return nil, "Review comments require a normal named file buffer or a Diffview new-side buffer"
  end
  local path = canonical(name)
  local root, vcs, root_err = repository_root(path)
  if not root then return nil, root_err end
  local relative = relative_path(root, path)
  if not relative then return nil, "The file must be inside the repository root" end

  if vim.bo[bufnr].modified then
    if not opts.autosave_on_comment then
      return nil, "Save the source buffer before commenting (or enable autosave_on_comment)"
    end
    local ok, write_err = pcall(vim.api.nvim_buf_call, bufnr, function()
      vim.cmd("silent write")
    end)
    if not ok then
      return nil, ("Could not save source buffer: %s"):format(tostring(write_err))
    end
  end

  local revision, revision_err = snapshot_revision(root, vcs)
  if not revision then return nil, revision_err end
  local snapshot, snapshot_err = read_snapshot(root, vcs, revision, relative)
  if not snapshot then
    return nil, ("Could not read %s from pinned snapshot: %s"):format(relative, snapshot_err)
  end
  local matches, match_err = buffer_matches_snapshot(bufnr, snapshot)
  if matches == nil then return nil, match_err end
  if not matches then
    return nil, "Buffer contents do not match the pinned repository snapshot; save/reload the source and retry"
  end

  return {
    kind = "normal",
    repo = root,
    vcs = vcs,
    commit = revision,
    path = relative,
    buffer = bufnr,
  }
end

function M.repository_for_buffer(bufnr)
  local diff_context, diff_err = M.from_diffview(bufnr)
  if diff_context == false then
    local ok, lib = pcall(require, "diffview.lib")
    if ok and type(lib.get_current_view) == "function" then
      local called, view = pcall(lib.get_current_view)
      local root = called and view and view.adapter and view.adapter.ctx and view.adapter.ctx.toplevel
      if type(root) == "string" and root ~= "" then return canonical(root), "git" end
    end
    return nil, diff_err
  end
  if diff_context then return diff_context.repo, diff_context.vcs end

  local name = vim.api.nvim_buf_get_name(bufnr)
  if vim.bo[bufnr].buftype ~= "" or name == "" then
    return nil, "Current buffer is not in a repository context"
  end
  local root, vcs, err = repository_root(canonical(name))
  if not root then return nil, err end
  return root, vcs
end

function M.is_oid(value)
  return is_oid(value)
end

function M._diffview_file_context(bufnr, view, winid)
  return diffview_file_context(bufnr, view, winid)
end

return M
