# Neovim development harness

Test the locally built `zerosyntax-lsp` in Neovim without packaging or
installing anything. This is the fastest loop for **server-only** iteration
(diagnostics, completion, hover, go-to-definition); use the VS Code F5 workflow
in [`docs/vscode-development.md`](../../docs/vscode-development.md) when you also
need to exercise extension-side behaviour (W3D previews, commands, settings UI).

## Quick, isolated run

Build the server, then launch Neovim with the throwaway config in
[`dev-init.lua`](./dev-init.lua) — it loads no plugins and no user config:

```sh
cargo build -p zerosyntax-server
nvim -u editors/nvim/dev-init.lua path/to/some.ini
```

Open an `.ini` file and the server attaches automatically. Useful checks:

- `:checkhealth vim.lsp` or `:LspInfo` — confirm the `zerosyntax` client attached
- `:lua vim.diagnostic.open_float()` — inspect diagnostics under the cursor
- `K` / `gd` / `<C-x><C-o>` — hover, go-to-definition, completion

After changing server code, rebuild and reload:

```sh
cargo build -p zerosyntax-server
```

then `:LspRestart` in the running Neovim (or reopen the file).

## Use it inside your normal config

To wire the local build into an existing setup, copy the `vim.filetype.add`
and `vim.lsp.start` block from `dev-init.lua` and point `exe` at your build:

- debug build: `target/debug/zerosyntax-lsp[.exe]`
- release build: `target/release/zerosyntax-lsp[.exe]`

Set `RUST_LOG=zerosyntax_lsp=debug` in the environment before launching Neovim
to get the server's structured trace on stderr (surfaced through
`:LspLog`). See [`docs/language-server.md`](../../docs/language-server.md) for
logging details and the full list of `init_options`.
