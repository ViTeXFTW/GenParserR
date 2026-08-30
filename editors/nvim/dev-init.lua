-- Isolated Neovim harness for the locally built ZeroSyntax language server.
--
-- Build first, then launch Neovim with this config (no plugins, no global
-- config), so you can test the server against a real .ini file:
--
--     cargo build -p zerosyntax-server
--     nvim -u editors/nvim/dev-init.lua path/to/some.ini
--
-- Re-test after a server change: rebuild, then `:LspRestart` (or reopen the
-- file). Requires Neovim 0.8+ (uses vim.lsp.start / vim.fs).

local script = debug.getinfo(1, "S").source:sub(2)
local nvim_dir = vim.fn.fnamemodify(script, ":p:h")
local repo_root = vim.fn.fnamemodify(nvim_dir, ":h:h")
local is_win = vim.fn.has("win32") == 1
local exe = repo_root .. "/target/debug/zerosyntax-lsp" .. (is_win and ".exe" or "")

if vim.fn.filereadable(exe) == 0 then
  vim.schedule(function()
    vim.notify(
      "zerosyntax-lsp not found at " .. exe .. "\nRun: cargo build -p zerosyntax-server",
      vim.log.levels.ERROR
    )
  end)
end

-- The server expects the Generals/Zero Hour INI language, so map .ini to it.
vim.filetype.add({ extension = { ini = "generals-ini" } })

vim.api.nvim_create_autocmd("FileType", {
  pattern = "generals-ini",
  callback = function(args)
    local fname = vim.api.nvim_buf_get_name(args.buf)
    vim.lsp.start({
      name = "zerosyntax",
      cmd = { exe }, -- bare invocation speaks LSP over stdio
      root_dir = vim.fs.dirname(fname),
      -- Mirrors the VS Code extension's initializationOptions; see
      -- docs/language-server.md for the full list.
      init_options = {
        format = { enable = false },
        -- baseIniRoots = { "C:/Games/Command and Conquer Generals Zero Hour" },
        analysis = {
          modelMemberStrictness = "compatible",
          mapOrderingDiagnostics = true,
          debounceMs = 250,
        },
      },
    })
  end,
})
