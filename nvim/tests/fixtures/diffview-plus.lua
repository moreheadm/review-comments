-- Shapes read from dlyongemallo/diffview-plus.nvim (shallow clone HEAD 5152bad).
-- The fork retains DiffView.cur_entry and the actual cur_layout's two-pane
-- a/b Window IDs; FileEntry.layout contains the two FileEntry panes. History
-- entries use the same selected FileEntry representation.
local function file(bufnr, symbol, revision, path, nulled)
  return {
    bufnr = bufnr,
    symbol = symbol,
    path = path,
    nulled = nulled or false,
    rev = { type = 2, commit = revision },
    active = true,
    loaded = true,
  }
end

return function(root, old_buf, new_buf, old_revision, new_revision, old_win, new_win)
  local old = file(old_buf, "a", old_revision, "src/example.lua")
  local new = file(new_buf, "b", new_revision, "src/example.lua")
  local old_window, new_window = { id = old_win, file = old }, { id = new_win, file = new }
  local layout = {
    windows = { old_window, new_window },
    a = old_window,
    b = new_window,
  }
  return {
    adapter = { ctx = { toplevel = root } },
    tabpage = vim.api.nvim_win_get_tabpage(new_win),
    cur_layout = layout,
    cur_entry = {
      path = new.path,
      layout = {
        windows = { old_window, new_window },
        a = old_window,
        b = new_window,
      },
    },
  }, old, new
end
