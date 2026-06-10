# Generals Zero Hour INI — VS Code extension

Reference editor client for the `genparser` language server (see the project
root README for the full architecture).
Provides diagnostics, completions, hover, go-to-definition, and schema-aware
semantic highlighting for C&C Generals: Zero Hour `.ini` files.

The server itself is IDE-agnostic; this extension is just a thin client that
launches it and speaks LSP.

## Build

```sh
cd editors/vscode
npm install
npm run compile
```

## Provide the server binary

Build the server from the workspace root and make it discoverable in one of
these ways (checked in order):

1. Set `genparser.server.path` to the absolute path of `genparser-lsp`.
2. Copy the binary to `editors/vscode/server/genparser-lsp[.exe]` (bundled into
   the `.vsix`).
3. Put `genparser-lsp` on your `PATH`.

```sh
cargo build --release -p genparser-server
# option 2:
mkdir -p editors/vscode/server
cp target/release/genparser-lsp* editors/vscode/server/
```

## Run / debug

Open this folder in VS Code and press <kbd>F5</kbd> to launch an Extension
Development Host, then open any `.ini` file recognized as **Generals INI**.

## Package

```sh
npm run package   # produces genparser-vscode-<version>.vsix (needs @vscode/vsce)
```

## Note on `.ini` association

This extension claims the `.ini` extension under the `generals-ini` language id.
If you also edit unrelated `.ini` files, scope it per workspace with
`files.associations`, e.g. only treat files under your mod folder as
`generals-ini`.
