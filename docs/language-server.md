# Standalone language server

Use `zerosyntax-lsp` with any editor that supports the Language Server Protocol
over stdio.

## Install

1. Download the Windows x64 or Linux x64 server archive from the
   [latest release](https://github.com/ViTeXFTW/ZeroSyntaxV2/releases/latest).
2. Verify the archive against `SHA256SUMS.txt` from the same release.
3. Extract the archive and put `zerosyntax-lsp` (or `zerosyntax-lsp.exe`) on
   your `PATH`, or configure its absolute path in your editor.
4. Register the server for the Generals/Zero Hour `.ini` files in your project.

The server writes protocol messages to stdout, so clients must launch it using
stdio rather than a TCP port.

## Client configuration

Configure your LSP client with:

| Option | Value |
| --- | --- |
| Command | `zerosyntax-lsp` |
| Transport | stdio |
| Files | Zero Hour `.ini` files |
| Workspace root | Your map or mod directory |

The server indexes INI files under each workspace folder. Open the whole project
to enable cross-file completion, definitions, references, rename, and workspace
symbols.

## Initialization options

```json
{
  "format": { "enable": false },
  "analysis": { "modelMemberStrictness": "compatible" },
  "baseIniRoots": [
    "C:/Games/Zero Hour",
    "C:/Mods/MyMod/Data/INI",
    "C:/Mods/MyMod.big"
  ]
}
```

- `format.enable` controls whether the server advertises document formatting.
- `analysis.modelMemberStrictness` is `off`, `compatible` (member exists in any
  applicable model), or `strict` (member exists in every applicable model).
  It defaults to `false`.
- `baseIniRoots` accepts directories and `.big` archives containing base game
  or mod INI files and W3D assets. Those INI definitions are treated as loaded
  before `map.ini` and `solo.ini`.

Restart the language server after changing initialization options.

## Supported LSP features

ZeroSyntax supports incremental document sync, diagnostics, completion, hover,
go to definition, references, rename, semantic tokens, document and workspace
symbols, folding ranges, quick fixes, and optional document formatting.

## Build from source

Install Rust 1.75 or newer, clone the repository, and run:

```sh
cargo build --locked --release -p zerosyntax-server
```

The binary is written to `target/release/zerosyntax-lsp` (`.exe` on Windows).
