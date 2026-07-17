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

## Command-line diagnostics

Use the `check` subcommand to run the same parser, schema, workspace index, and
diagnostics without an LSP client:

```sh
zerosyntax-lsp check Data/INI
zerosyntax-lsp check map.ini --base-root "C:/Games/Zero Hour"
zerosyntax-lsp check --fail-on warning Data/INI
zerosyntax-lsp check --json --stdin-filename map.ini - < generated.ini
```

The positional targets are `.ini` files, recursively scanned directories, or
one `-` for stdin. At least one target is required. All selected targets are
indexed together before diagnostics run, so references between them resolve.
Overlapping targets are checked once.

`--base-root` is repeatable and accepts directories or `.big` archives
containing base/mod INIs and W3D assets. Base roots participate in reference,
model, and bone checks but do not emit diagnostics themselves. For stdin,
`--stdin-filename` supplies the displayed/indexed name and enables `map.ini` or
`solo.ini` override semantics; it defaults to `<stdin>`.

Human output uses compiler-style records followed by a summary:

```text
Data/INI/Weapon.ini:12:14: error[bad-number]: expected an integer, found `lots`
1 error(s), 0 warning(s), 0 hint(s)
```

`--json` writes a stable array to stdout. Each record has `file`, `range`,
`severity`, `code`, and `message`. Range lines and Unicode-scalar columns are
1-based; the end position is exclusive.

```json
[{"file":"map.ini","range":{"start":{"line":2,"column":14},"end":{"line":2,"column":18}},"severity":"error","code":"bad-number","message":"expected an integer, found `lots`"}]
```

Exit codes are:

- `0`: no diagnostic meets the failure threshold.
- `1`: at least one diagnostic meets it.
- `2`: invalid arguments, inaccessible/unsupported inputs, or an internal
  failure.

The default threshold is `error`. Select `--fail-on warning` or
`--fail-on hint` for stricter CI. Diagnostics are still written to stdout when
the command exits 1; operational errors go to stderr.

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
  "baseIniRoots": [
    "C:/Games/Zero Hour",
    "C:/Mods/MyMod/Data/INI",
    "C:/Mods/MyMod.big"
  ]
}
```

- `format.enable` controls whether the server advertises document formatting.
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
