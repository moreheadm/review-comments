local M = {}

local function command_path(command)
  if command:find("/", 1, true) then
    return command
  end
  local path = vim.fn.exepath(command)
  if path == "" then
    return nil
  end
  return path
end

function M.run(command, args, opts)
  opts = opts or {}
  local executable = command_path(command)
  if not executable then
    return nil, ("Executable not found: %s"):format(command)
  end

  local argv = { executable }
  vim.list_extend(argv, args or {})
  local started, system = pcall(vim.system, argv, {
    cwd = opts.cwd,
    stdin = opts.stdin,
    text = false,
  })
  if not started then
    return nil, tostring(system)
  end
  local waited, process = pcall(function()
    return system:wait()
  end)
  if not waited then
    return nil, tostring(process)
  end
  if process.code ~= 0 then
    local err = process.stderr or ""
    local decoded
    if err ~= "" then
      local parsed, value = pcall(vim.json.decode, err)
      if parsed then decoded = value end
    end
    local message = decoded and decoded.error and decoded.error.message
    return nil, message or vim.trim(err) ~= "" and vim.trim(err)
      or ("%s exited with status %d"):format(command, process.code), decoded
  end

  local out = process.stdout or ""
  if opts.json then
    local parsed, decoded = pcall(vim.json.decode, out)
    if not parsed or not decoded then
      return nil, ("Invalid JSON from %s: %s"):format(command, tostring(decoded or "empty output"))
    end
    if decoded.rv ~= 1 then
      return nil, ("Unsupported rv JSON schema from %s"):format(command), decoded
    end
    return decoded
  end
  return out
end

function M.rv(command, args, opts)
  opts = vim.tbl_extend("force", opts or {}, { json = true })
  local argv = { command }
  vim.list_extend(argv, args or {})
  argv[#argv + 1] = "--json"
  return M.run(opts.executable or "rv", argv, opts)
end

function M.capture(command, args, cwd)
  return M.run(command, args, { cwd = cwd })
end

return M
