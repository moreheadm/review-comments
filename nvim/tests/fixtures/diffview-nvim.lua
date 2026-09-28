-- Shapes read from sindrets/diffview.nvim (shallow clone HEAD 4516612).
-- Diff2 exposes view.cur_layout.windows with a/b Window IDs, while the
-- FileEntry layout has the same a/b FileEntry panes. A displayed side is
-- identified by the actual current Window, not by the first matching bufnr.
local function file(bufnr, symbol, revision, path, nulled)
  return {
    bufnr = bufnr,
    symbol = symbol,
    path = path,
    nulled = nulled or false,
    rev = { type = 2, commit = revision },
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
