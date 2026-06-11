# Phase 3 — Incremental reparse & incremental analysis

Date: 2026-06-11. Machine/baseline as in `phase0-baseline.md`.

## What shipped

1. **Block-splice incremental reparse** (`crates/syntax/src/incremental.rs`,
   exposed as `Analyzer::reparse`). On an edit, the affected top-level
   children of ROOT (widened one sibling each side, then extended to line
   boundaries) are reparsed as a fragment and spliced into the old green
   tree; everything else is reused pointer-identically. Falls back to a full
   parse when the fragment would end with an open scope while text follows
   it (e.g. the user deleted an `End`), or when a guard fails. Parser errors
   are kept/rebased/shifted per region, preserving full-parse order.

   *Why this is exact:* the scope stack is empty at every top-level child
   boundary, the lexer is line-local (no token spans a newline — comments,
   strings, and `\r\n` all stop at EOL), and the `OpenerOracle` only sees the
   enclosing-scope keyword chain, which is child-internal. The line-boundary
   guards (`prefix` ends `\n`, region ends `\n` when a suffix exists) also
   rule out `\r`+`\n` joining across a splice seam.

2. **Per-block diagnostics cache** (`diagnostics::DiagnosticsCache`,
   `diagnose_with_cache`). Keyed on green-node pointer identity (each entry
   holds the `GreenNode` alive, so a key can never be reused by a new
   allocation); spans stored block-relative and rebased on hit. Invalidated
   by `WorkspaceIndex::generation()`, which bumps only when a file's
   definition *names* change — ordinary keystrokes keep the cache warm.

3. **`semanticTokens/range`** (`semantic::semantic_tokens_range` + server
   handler, capability `range: true`): block-granular viewport tokens, so a
   repaint no longer costs an O(file) pass.

4. **Server keystroke path** (`backend.rs`): `did_change` applies each delta
   to the rope and reparses incrementally in lockstep (`DocumentState` keeps
   `text: Arc<str>` matching the cached parse); `refresh` only runs
   diagnostics (cached) and publishes.

## Equivalence guarantees (the load-bearing tests)

- `syntax/tests/incremental_fuzz.rs` — 2,400 chained random edits over
  random structure-heavy documents: spliced text, tree dump, and error list
  all exactly equal a fresh full parse (97% splice rate).
- `analysis/tests/incremental.rs` — same property with the real schema
  oracle over every spec INI (always-on) and over all 135 corpus files
  (`--ignored`; 1,080 edits, 96% spliced), additionally asserting
  `diagnose` *and* `diagnose_with_cache` equality after every edit.
- `server/tests/e2e.py` — wire-level: incremental deltas (incl. an
  `End`-deletion fallback case) produce diagnostics identical to a
  full-text baseline; `semanticTokens/range` over the whole document equals
  `full`.

## Numbers (criterion, release)

| Path | Pre-Phase-3 (full) | Phase 3 (incremental) | Speedup |
|---|---|---|---|
| keystroke, synthetic 10k lines | 6.7 ms | 60 µs | ~110× |
| keystroke, synthetic 50k lines | 35.1 ms | 283 µs | ~125× |
| keystroke, ParticleSystem.ini (61,719 lines) | 44.0 ms | **147 µs** | ~300× |
| semantic tokens, ParticleSystem.ini | ~50 ms (full) | 173 µs (range, viewport) | — |

The bench measures reparse + diagnose; the server additionally pays one
rope-to-string copy per change (~hundreds of µs at this size). Total
keystroke cost lands well under the **10 ms p95 budget** from Phase 0 —
budget met with ~30× headroom on the largest real file.

Observability: `reparse` returns a `Strategy` (`Spliced`/`Full`);
`DiagnosticsCache::stats()` reports cumulative hits/misses.

## Verdict

Phase 3 exit criteria met: keystroke path far under budget on the largest
corpus file, equivalence fuzzing green at three layers, fallback rate
observable (~3–4% across fuzz suites). `semanticTokens/full` stays available
unchanged; clients that know `range` get viewport-priced repaints.
