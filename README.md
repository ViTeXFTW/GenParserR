# genparser — a language server for C&C Generals: Zero Hour INI

`genparser` is an IDE-agnostic [language server](https://microsoft.github.io/language-server-protocol/)
for the INI scripting format used by *Command & Conquer Generals: Zero Hour*.
It helps mappers and modders author `Object`, `Weapon`, `FXList`, and other INI
definitions with **diagnostics**, **completions**, **hover**, **go-to-definition**,
and schema-aware **semantic highlighting**.

What makes it faithful to the game: the block/field schema is **hand-written to
match the engine's own `FieldParse` tables** in the open-sourced C++ source. The
parse function the engine uses for each field determines the field's data type;
the table's userData column determines its enum/bitflag value set; the engine's
dispatch table and `ModuleFactory` enumerate the block and module types. The
schema lives in `crates/schema/schema.json` and is grown by hand against that
source.

## Workspace layout

```
crates/
  schema/      Serde data model for the schema + the embedded, hand-written schema.json
  syntax/      logos lexer + rowan lossless CST (error-recovering)
  analysis/    Diagnostics, completion, semantic tokens, cross-file index
  server/      tower-lsp server (stdio)  ->  binary `genparser-lsp`
editors/
  vscode/      Reference VS Code extension (thin LSP client)
GeneralsCode/  The engine source the schema is modeled on (not part of the crate)
```

## Build

```sh
cargo build --release            # builds all crates incl. the server
cargo test                       # unit + integration tests
```

The language server binary lands at `target/release/genparser-lsp`.

### End-to-end test

```sh
cargo build -p genparser-server
python crates/server/tests/e2e.py target/debug/genparser-lsp     # .exe on Windows
```

This drives the real binary over stdio (initialize → didOpen → diagnostics,
completion, semantic tokens).

### Spec-first behavior tests

`crates/analysis/tests/spec/` holds `.ini` files paired with hand-authored
`*.spec.toml` expectations of the diagnostics and completions they should
produce. Unlike a snapshot, a spec is written by hand, so an outcome can be
pinned *before* the feature that produces it exists:

* `[[diag]]` — assert a diagnostic by `severity`, `code`, and the token its span
  must cover (`on`).
* `[[complete]]` — assert the completion set at a `$N` cursor marker.
* `xfail = true` — for behavior not built yet; the suite tolerates it failing
  today but fails if it ever passes (forcing the flag to be dropped).

Run `cargo test -p genparser-analysis --test spec` to check them. To add a case,
drop a `.ini` (with `$N` markers at completion points) and a sibling
`*.spec.toml`.

## Editing the schema

The committed `crates/schema/schema.json` is embedded into the server at compile
time. It is **hand-written**: to add or fix a block, field, module or value set,
edit the JSON directly and model each entry on the matching `FieldParse` table
in `GeneralsCode/`. The engine's parse function for a field maps to a
`value_type` (e.g. `INI::parseReal` → `real`, `INI::parseBool` → `bool`,
`INI::parseIndexList` over a name array → an `enum` value set); when a parse
function has no clean type, use `Unknown { parse_fn }` so the field is treated
leniently rather than falsely flagged.

After editing, run `cargo test -p genparser-schema` and the spec test to
validate the JSON and catch new false positives.

## Editor setup

The server speaks LSP over stdio, so any LSP-capable editor works.

### VS Code

Use the bundled extension in `editors/vscode/` (see its README). It claims the
`generals-ini` language id for `.ini` files and launches the server.

### Neovim (nvim-lspconfig)

```lua
local configs = require("lspconfig.configs")
local lspconfig = require("lspconfig")
if not configs.genparser then
  configs.genparser = {
    default_config = {
      cmd = { "genparser-lsp" },
      filetypes = { "generals_ini" },     -- map your .ini files to this filetype
      root_dir = lspconfig.util.root_pattern(".git", "*.ini"),
      single_file_support = true,
    },
  }
end
lspconfig.genparser.setup({})
```

### Helix (`languages.toml`)

```toml
[language-server.genparser]
command = "genparser-lsp"

[[language]]
name = "generals-ini"
scope = "source.ini"
file-types = ["ini"]
language-servers = ["genparser"]
```

### Zed / Sublime (LSP) / others

Point the editor's generic LSP integration at the `genparser-lsp` command with
stdio transport and a document selector for your INI files.

## How faithful is it?

* **Lexing** mirrors the engine: `;` line comments, `"`-quoted strings, and
  `" \n\r\t="` token separators (so `Key=Value` and `Key = Value` are identical).
* **Block / field / module catalog** is generated from the engine source, so it
  matches what the game actually parses.
* **Diagnostics** are *stricter than the engine by design* (the project's goal):
  on top of engine-faithful errors (unknown block/field, bad value type, bad
  enum/bitflag member, unterminated block) it adds modder-helpful warnings such
  as unresolved cross-file references and unknown modules.

### Known limitations / future work

* The hand-written schema currently covers a core set of blocks (`Object`,
  `Weapon`, `Armor`, `Locomotor`, `Upgrade`) and a few modules; more are added
  by hand from the engine `FieldParse` tables as needed.
* Some custom sub-object parsers (FX nuggets, OCL nuggets, weapon/armor sets)
  are modeled as lenient `Unknown` / list fields; their inner structure isn't
  validated yet.
* Enum/bitflag value sets that aren't yet enumerated are modeled as `Unknown`
  (member validation is then skipped, never falsely flagged).
* Positions assume effectively-ASCII content (true for vanilla INI); non-BMP
  text columns are not adjusted for UTF-16.

## License

GPL-3.0-or-later, matching the engine source the schema derives from.
