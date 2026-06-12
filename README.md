# genparser — a language server for C&C Generals: Zero Hour INI

`genparser` is an IDE-agnostic [language server](https://microsoft.github.io/language-server-protocol/)
for the INI scripting format used by *Command & Conquer Generals: Zero Hour*.
It helps mappers and modders author `Object`, `Weapon`, `FXList`, and other INI
definitions with **diagnostics**, **completions**, **hover**, **go-to-definition**,
**find-references**, workspace-wide **rename**, an **outline** (document
symbols + folding), **workspace symbol search**, **formatting**, **quick
fixes**, and schema-aware **semantic highlighting**.

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

### Suppressing diagnostics per file

If a file intentionally trips a diagnostic (e.g. it references definitions
that live outside the workspace), opt that file out of specific codes with a
pragma comment at file scope (outside any block):

```ini
; genparser-disable: unresolved-reference, unreachable-set
```

Codes may be separated by commas and/or spaces, the colon is optional, and
multiple pragma lines accumulate. The code names are exactly what the editor
shows on each diagnostic (e.g. `genparser(unresolved-reference)` in VS Code's
hover and Problems panel). A misspelled code is flagged with an
`unknown-suppression` hint so a typo never silently suppresses nothing.

### Known limitations / future work

The full phase-by-phase plan and status lives in
[`docs/roadmap.md`](docs/roadmap.md). Current state in brief:

* All 63 top-level block types and all 223 engine-registered modules are
  modeled; running the analyzer over the complete Zero Hour game data yields
  **21 diagnostics, all genuine** (dead references, condition-less
  WeaponSets, and unreachable upgrade-conditioned sets in the shipped
  INIs) — zero unknown-block / unknown-module / unknown-field noise.
* Value validators cover Bool/Int/Real/Percent/Color/Coord/Duration, enums,
  bitflags, references, and typed token lists (`Armor = <DamageType>
  <percent>`). Module field tables are extracted from the engine's
  `buildFieldParse` chains; fields that name other definitions (images, FX
  lists, OCLs, audio events, weapons, upgrades, …) are reference-typed, so
  they complete from the workspace index and warn when unresolved.
* **Coverage: 3,656 fields across the 63 blocks + 223 modules; 86% carry a
  concrete value type, validated/completed against 31 engine-extracted value
  sets** (ObjectStatus, ModelCondition, death/damage/veterancy flags, KindOf,
  Locomotor appearance, …). The remaining 14% use multi-token engine parse
  functions not yet modeled and stay `Unknown` (validation is then skipped,
  never falsely flagged).
* Editing is incremental end-to-end (block-splice reparse + per-block
  diagnostics cache; a keystroke in a 60k-line file costs ~150 µs), and
  positions honor the negotiated LSP encoding (UTF-8 or UTF-16).
* The full LSP surface: outline/folding/workspace-symbol, find-references and
  rename (index-backed, case-insensitive like the engine), semantic tokens
  (full + range + delta), an indent formatter, quick fixes (insert missing
  `End`, did-you-mean for enum members), and a block-local dead-code warning
  (`unreachable-set`) that found 8 genuinely dead WeaponSets/ArmorSets in the
  shipped game data. Corpus total: **21 diagnostics, all genuine**.
* Workspace-wide *unused-definition* hints were measured and deliberately not
  shipped: maps, `.wnd` files, and engine code reference INI entities the
  index can't see (97% of ParticleSystems would false-flag). See the roadmap.

## License

GPL-3.0-or-later, matching the engine source the schema derives from.
