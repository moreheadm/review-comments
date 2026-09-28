local cli = require("rv.cli")

local M = {}
local namespace = vim.api.nvim_create_namespace("rv-review-comments")

local function first_line(text)
  text = tostring(text or "")
  local line = text:match("^([^\r\n]*)") or ""
  if line == "" then return "(empty comment)" end
  return line
end

local function mark(bufnr, line, end_line, label, sidebar)
  if type(line) ~= "number" or line < 1 then return false end
  local count = vim.api.nvim_buf_line_count(bufnr)
  if line > count then return false end
  end_line = math.max(line, math.min(tonumber(end_line) or line, count))
  local opts = {
    virt_text_pos = "eol",
    hl_mode = "combine",
    priority = 150,
    sign_text = "●",
    sign_hl_group = "DiagnosticInfo",
  }
  if not sidebar then opts.virt_text = { { "  ▸ " .. label, "DiagnosticInfo" } } end
  opts.end_row = end_line
  opts.hl_group = "Visual"
  opts.hl_eol = false
  local ok = pcall(vim.api.nvim_buf_set_extmark, bufnr, namespace, line - 1, 0, opts)
  return ok
end

local function show(root, branch, at, path, executable)
  local args = { "-b", branch }
  if at then
    vim.list_extend(args, { "--at", at })
  end
  if path then
    vim.list_extend(args, { "--path", path })
  end
  return cli.rv("show", args, { cwd = root, executable = executable })
end

function M.show(root, branch, at, path, executable)
  local result, err = show(root, branch, at, path, executable)
  if not result then return nil, err end
  return result
end

function M.render_buffer(bufnr, root, branch, at, path, drafts, executable, allow_absent, sidebar)
  local result, err, failure = show(root, branch, at, path, executable)
  if not result then
    if allow_absent and failure and failure.error and failure.error.code == "branch_not_found" then
      err = nil -- An explicitly armed branch has no saved threads yet.
    end
    result = { threads = {} }
  end
  vim.api.nvim_buf_clear_namespace(bufnr, namespace, 0, -1)

  local rendered = 0
  for _, thread in ipairs(result.threads or {}) do
    local mapped = thread.mapped
    local anchor = thread.anchor
    local target_path = mapped and mapped.path or anchor and anchor.path
    local start_line = mapped and mapped.start_line or anchor and anchor.start_line
    local end_line = mapped and mapped.end_line or anchor and anchor.end_line
    if target_path == path and start_line and mapped and mapped.status ~= "deleted"
      and mapped.status ~= "file_deleted" and mapped.status ~= "binary" then
      local text = thread.deleted and "[deleted comment]" or first_line(thread.body)
      local function append_replies(replies)
        for _, reply in ipairs(replies or {}) do
          text = text .. "  ↳ "
            .. (reply.deleted and "[deleted reply]" or first_line(reply.body))
          append_replies(reply.replies)
        end
      end
      append_replies(thread.replies)
      if mark(bufnr, start_line, end_line, text, sidebar) then rendered = rendered + 1 end
    end
  end

  for _, action in ipairs(drafts or {}) do
    local scope = action._context
    if scope and scope.repo == root and scope.branch == branch
      and action.type == "comment" and action.commit == at and action.anchor
      and action.anchor.path == path then
      local label = "draft: " .. first_line(action.body)
      if mark(bufnr, action.anchor.start_line, action.anchor.end_line, label, sidebar) then
        rendered = rendered + 1
      end
    end
  end
  return { result = result, rendered = rendered, error = err }
end

function M.namespace()
  return namespace
end

return M
