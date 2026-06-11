# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`genparser` is an IDE-agnostic LSP language server for the INI scripting format
of *Command & Conquer Generals: Zero Hour*. The block/field/module schema is a
**hand-written `crates/schema/schema.json`**, modeled directly on the engine's
own C++ `FieldParse` tables (in `GeneralsCode/`) — so diagnostics/completions
track what the game actually parses. The committed `schema.json` is the single
shipped artifact; the server has no runtime dependency on the C++ source.

The schema starts small and is grown by hand: when you add a block/field/module,
find its `FieldParse` table in `GeneralsCode/` and translate each entry to a
`ValueType` (parse-fn → type mapping is the same judgement the old extractor
made — e.g. `INI::parseReal` → `real`, `INI::parseBool` → `bool`,
`INI::parseIndexList` over a name array → an `enum` value set). When a field's
parse function has no clean type, use `Unknown { parse_fn }` so it is treated
leniently.

## Commands

```sh
cargo build --release            # all crates; binary -> target/release/genparser-lsp
cargo test                       # all unit + integration tests
cargo test -p genparser-analysis --test spec         # spec-first behavior tests
cargo test -p genparser-schema   # schema round-trip + embedded-schema validity

# run a single test by name substring:
cargo test -p genparser-analysis opens_scope

# end-to-end (drives the real binary over stdio):
cargo build -p genparser-server
python crates/server/tests/e2e.py target/debug/genparser-lsp.exe   # .exe on Windows

# real-corpus gate (fetch once, then run): zero panics, zero syntax /
# unknown-block diagnostics over the full ZH game data; the printed warning
# histogram is the schema-coverage backlog.
pwsh scripts/fetch-corpus.ps1            # or scripts/fetch-corpus.sh
cargo test --release -p genparser-analysis --test corpus -- --ignored --nocapture

cargo bench                              # parse/analysis benchmarks (criterion)
```

Editing the schema is a manual workflow: hand-edit `crates/schema/schema.json`,
then run `cargo test -p genparser-schema` (validates the embedded JSON parses
and contains the core blocks) and the spec test below (catches new false
positives and pins intended behavior). Cross-check new entries against the
relevant `FieldParse` table in `GeneralsCode/`. Run the corpus gate after any
schema or oracle change — a single missing sub-block keyword shows up there as
`syntax` / `unknown-block` noise.

### Spec-first behavior tests

The spec tests in `crates/analysis/tests/spec/` **hand-author** the desired
outcome so a behavior can be pinned *before* the feature that produces it exists
(TDD) — rather than snapshotting current output. Driven by
`crates/analysis/tests/spec.rs`:

```sh
cargo test -p genparser-analysis --test spec
```

Each case is a pair: `Foo.ini` (real INI, plus `$1`/`$2` cursor markers at
completion points — stripped before analysis) and `Foo.spec.toml` listing
expectations. `[[diag]]` asserts one diagnostic (`severity`, `code`, and `on` =
the token its span must cover, with optional `nth`); `[[complete]]` asserts a
completion set at a marker (`includes` / `excludes`, or exact `equals`); the
top-level `no_errors` asserts zero error-severity diagnostics. Token searches
ignore `;`-comment text, and each spec resolves references against a
`WorkspaceIndex` built from its own file.

Mark an assertion `xfail = true` when it specifies behavior not yet built: the
suite tolerates it failing today **but fails if it ever passes**, so dropping
the flag is forced once the feature lands. Keep `cargo test` green by adding the
spec first (xfail), then implementing until you can remove the flag.

## Architecture

A one-directional pipeline across four crates (workspace `members` in
`Cargo.toml`):

```
schema.json ──embedded──▶ schema ──▶ analysis ──▶ server
(hand-written  (compile-time   (serde   (diagnostics,   (tower-lsp,
 from C++)      include_str!)   model)   completion,     stdio)
                                         semantic, nav)
        syntax (lexer + rowan CST) ───────────────────────▶ used by analysis
```

- **`schema`** — serde data model + the embedded, hand-written `schema.json`
  (`EMBEDDED_SCHEMA_JSON`, loaded via `embedded()`). Central type is
  `ValueType` (Bool/Int/Real/Enum/BitFlags/Reference/… and the catch-all
  `Unknown { parse_fn }`). `Schema::index()` builds name→item lookup maps.
  **To add coverage, edit `schema.json` against the engine `FieldParse` tables.**
- **`syntax`** — `logos` lexer (engine-faithful: `;` comments, `"`-quoted
  strings, `" \n\r\t="` separators so `Key=Value` ≡ `Key = Value`) + a `rowan`
  lossless CST with error recovery. The parser is **line-oriented**: a line
  either sets a field or opens an `End`-terminated scope. Whether a line opens a
  scope is delegated to the `OpenerOracle` trait (decoupling grammar from
  schema). `incremental.rs` adds **block-splice incremental reparse**
  (`reparse(old, old_text, new_text, edit, oracle)`): the edited top-level
  children are reparsed as a fragment and spliced, unchanged blocks keep
  pointer-identical green nodes, and it falls back to a full parse when the
  fragment ends with an open scope while text follows (`Strategy::Full`).
  Equivalence with a full parse is fuzz-tested (`tests/incremental_fuzz.rs`
  and `analysis/tests/incremental.rs`); any lexer/parser change must keep
  those suites green.
- **`analysis`** — IDE-agnostic logic over a shared `Analyzer` (owns the
  schema + lookup maps). Modules: `diagnostics`, `completion`, `semantic`
  (tokens), `nav` (hover/go-to-def), `index` (`WorkspaceIndex` cross-file
  reference table), `model` (resolves the schema scope at a CST position).
  All positions are **byte offsets**; the server converts to LSP line/char.
  Per-block diagnostics are cached (`diagnose_with_cache` +
  `DiagnosticsCache`): keyed on green-node pointer identity with
  block-relative spans, invalidated by `WorkspaceIndex::generation()` (bumps
  only when a file's definition *names* change). Consequence: a root child's
  diagnostics must never depend on sibling blocks. `semantic_tokens_range`
  serves viewport-only tokens.
- **`server`** — `tower-lsp` over stdio. `backend.rs` holds open docs as
  `DocumentState`s (`ropey` rope + matching `text: Arc<str>` + cached parse +
  version + diagnostics cache; INCREMENTAL sync — `did_change` applies each
  delta to the rope and keeps the parse in lockstep via `Analyzer::reparse`,
  so a keystroke costs the edited block, not the file; `refresh` only runs
  cached diagnostics and publishes; read-only requests reuse the cached
  parse), the shared `Analyzer`, and the `WorkspaceIndex`. `convert.rs` does
  byte-offset ↔ LSP position mapping in the position encoding negotiated at
  `initialize` (UTF-8 when the client offers it, else the UTF-16 baseline).
  Advertises `semanticTokens` `full` **and** `range`. Phase-3 numbers
  (`docs/phase3-incremental.md`): keystroke on the 61k-line
  ParticleSystem.ini ≈ 147 µs vs 44 ms full reparse.

### Two concepts worth understanding before editing

**The `OpenerOracle` (the scope-opening decision).** `analysis::SchemaOpeners`
implements it context-sensitively: a nested line opens an `End`-terminated scope
only if its head keyword is a child of the *enclosing* scope — a module slot or
a `sub_blocks` entry declared in `schema.json` (e.g. `FXList` → `ParticleSystem`
nugget, `Object` → `Prerequisites`; sub-blocks nest and are keyed by the
enclosing head keyword) — or one of the global `CURATED_SUBBLOCKS`
(ConditionState, ArmorSet, decal templates, …) in `crates/analysis/src/lib.rs`.
This context-sensitivity is what stops a field whose first token equals a block
keyword (e.g. `Armor = TankArmor` inside an `ArmorSet`, or `ParticleSystem =
BlackTrail` inside a `CreateDebris` nugget) from being misparsed as a new
block. Prefer declaring sub-blocks in the schema (context-keyed); use
`CURATED_SUBBLOCKS` only for keywords that appear inside module scopes, where
the oracle sees the slot keyword (`Behavior`/`Draw`) as the enclosing head.
Blocks with `"terminated": false` (BenchProfile, ReallyLowMHz, LODPreset) are
single-line directives: the parser emits them as file-scope fields, not
blocks. Sub-blocks may declare `fields` of their own (e.g. `WeaponSet`,
`FXList`'s `Tracer` nugget): `model::scope_schema` resolves a nameless
`MODULE` node against the enclosing scope's `sub_blocks`, so those fields
validate and complete like block fields. Multi-token engine parse functions
are typed `token_list` (e.g. `Armor = <DamageType|Default> <percent>`,
`Weapon = <slot> <weapon>`), validating/completing each token position by
its own element type.

**Diagnostics are intentionally stricter than the engine** (the project's
goal). Two layers in `diagnostics.rs`: engine-faithful *errors* (unknown
block/field, bad value type, bad enum/bitflag member, unterminated block) plus
modder-helpful *warnings/hints* (unknown module, unresolved cross-file
reference). Each diagnostic carries a stable string `code` (e.g.
`unknown-field`, `syntax`) — the spec tests pin these by `code`, so changing a
code or span requires updating the affected `*.spec.toml` files.

### Editor client

`editors/vscode/` is a thin reference LSP client (claims the `generals-ini`
language id). The server speaks stdio LSP, so Neovim/Helix/Zed configs are in
the README. `GeneralsCode/` is the engine source the schema is modeled on (a
reference for hand-authoring `schema.json`) — it is not part of any crate.

## Conventions

- License is **GPL-3.0-or-later** (matches the engine source the schema
  derives from); keep crate headers consistent.
- `schema.json` is hand-written — edit the JSON directly, modeling new entries
  on the engine's `FieldParse` tables in `GeneralsCode/`. Keep `value_type`s
  faithful and fall back to `Unknown { parse_fn }` when a type is unclear.
- Adding/changing diagnostics or schema content: update the affected
  `tests/spec/*.spec.toml` (and drop any `xfail` a change now satisfies), then
  review the diff.
