# Phase 0 baseline — benchmarks + corpus smoke pass

Date: 2026-06-10. Machine: Windows 10, release builds. Corpus: 135 INI files
from `TheSuperHackers/GeneralsGamePatch2` `GeneralsZH/Data/INI`
(fetched by `scripts/fetch-corpus.ps1`).

## Per-keystroke budget

**≤ 10 ms p95 for parse + diagnose + publish** on the largest real file.

## Synthetic benchmarks (`cargo bench`)

| pass | 1k lines | 10k lines | 50k lines |
|---|---|---|---|
| tokenize | 37 µs | 0.56 ms | 4.8 ms |
| parse (CST build) | 0.36 ms | 3.7 ms | 22 ms |
| diagnose | 0.25 ms | 2.5 ms | 12.9 ms |
| semantic_tokens (full) | 0.91 ms | 9.4 ms | 50 ms |
| definitions_in | 12 µs | 0.12 ms | 0.57 ms |
| keystroke (parse+diagnose) | 0.62 ms | 6.3 ms | 37 ms |

rowan green-tree construction dominates parse (~6–8× the lexer);
`semantic_tokens` is the single most expensive pass at every size.

## Corpus reality check

Real files exceed the 50k synthetic tier: `ParticleSystem.ini` is **61,719
lines** (37 ms parse+diagnose measured), `Object/CivilianBuilding.ini` 38,773,
five more Object INIs around 19–34k. Corpus percentiles: parse p50 183 µs /
p95 7.3 ms / max 31.3 ms; diagnose p50 54 µs / p95 1.7 ms / max 5.8 ms.

Zero panics across the corpus. Zero `#`-preprocessor lines (engine dialect
confirmed; no flattening needed). All 135 files decode as UTF-8 — the
Windows-1252 risk applies to third-party mod files, not this corpus; keep the
lossy-decode path anyway.

## Diagnostic histogram (schema/oracle backlog input)

| code | count | reading |
|---|---|---|
| unknown-block | 18,348 | schema has 5 of the engine's top-level block types; biggest: ParticleSystem, DialogEvent (Eva), AudioEvent, MappedImage, CommandButton, CommandSet, FXList |
| unknown-field | 13,846 | Object block missing many fields (Geometry*, EditorSorting, Shadow, …) |
| unknown-module | 6,883 | module long tail (FXListDie, SlowDeathBehavior, FlammableUpdate, DestroyDie, PhysicsBehavior, …) |
| syntax | 945 | oracle-gap cascades, concentrated in FXList.ini (420) and ObjectCreationList.ini (322) |
| unresolved-reference | 238 | cross-file refs; re-triage after schema growth |

The 4,815 "unknown block type `Behavior`" hits are the predicted cascade: a
sub-block keyword the oracle doesn't open causes its `End` to close the
enclosing `Object` early, spilling the rest of the block to file scope.
Phase 2 triages these per-keyword.

## Go/no-go for Phase 3 (incremental reparse)

**GO — all three parts.** A 60k-line file costs ~37 ms per keystroke today
(3.7× budget), and `semanticTokens/full` adds up to ~50 ms per repaint:

1. Block-granularity reparse-and-splice (top-level BLOCK boundary).
2. Per-block analysis caching (diagnostics, semantic tokens, definitions).
3. `semanticTokens/range` so repaint cost tracks the viewport, not the file.

Files ≤ ~10k lines are comfortably inside budget already, so the splice
fallback to full reparse is acceptable whenever an edit is not containable.

## Reproduce

```sh
cargo bench -p genparser-syntax --bench parse
cargo bench -p genparser-analysis --bench analyze
pwsh scripts/fetch-corpus.ps1
cargo test --release -p genparser-analysis --test corpus -- --ignored --nocapture
```
