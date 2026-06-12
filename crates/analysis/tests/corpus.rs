// SPDX-License-Identifier: GPL-3.0-or-later
//! Real-corpus harness: runs the full analysis pipeline over the actual game
//! INI files (fetched into `corpus/` by `scripts/fetch-corpus.*`).
//!
//! Ignored by default so plain `cargo test` stays corpus-free; run with:
//!
//! ```sh
//! cargo test --release -p genparser-analysis --test corpus -- --ignored --nocapture
//! ```
//!
//! This is a permanent gate (roadmap Phase 2): zero panics, zero `syntax`
//! diagnostics, and zero `unknown-block` diagnostics across the real game
//! data. A `syntax` or `unknown-block` hit on the corpus is an OpenerOracle /
//! schema-structure gap (one missing sub-block keyword cascades into dozens
//! of bogus diagnostics downstream). The printed histogram of the remaining
//! warning codes is the schema-coverage backlog, ordered by frequency.

use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use genparser_analysis::{diagnostics, index, Analyzer, WorkspaceIndex};

fn corpus_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../corpus/GeneralsGamePatch2/GeneralsZH/Data/INI");
    dir.is_dir().then(|| dir.canonicalize().unwrap())
}

/// Extra corpora checked into `corpus/` by hand (e.g. `MapINI/`: map.ini /
/// solo.ini override files shipped with overhaul maps). Optional — only
/// scanned when present.
fn extra_corpus_dirs() -> Vec<PathBuf> {
    ["MapINI"]
        .iter()
        .filter_map(|name| {
            let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../corpus")
                .join(name);
            dir.is_dir().then(|| dir.canonicalize().unwrap())
        })
        .collect()
}

fn ini_files(dir: &PathBuf) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.clone()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("ini")) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Real INIs predate UTF-8; decode leniently rather than skipping files.
fn read_lossy(path: &PathBuf) -> String {
    String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned()
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

#[test]
#[ignore = "requires corpus/ — run scripts/fetch-corpus.ps1 (or .sh) first"]
fn corpus_smoke() {
    let Some(dir) = corpus_dir() else {
        panic!("corpus not found; run scripts/fetch-corpus.ps1 first");
    };
    // (display-relative name, absolute path) across the main corpus and any
    // hand-added extra corpora (map.ini overrides etc.).
    let mut files: Vec<(String, PathBuf)> = ini_files(&dir)
        .into_iter()
        .map(|p| (p.strip_prefix(&dir).unwrap().display().to_string(), p))
        .collect();
    for extra in extra_corpus_dirs() {
        let base = extra.parent().unwrap().to_path_buf();
        files.extend(
            ini_files(&extra)
                .into_iter()
                .map(|p| (p.strip_prefix(&base).unwrap().display().to_string(), p)),
        );
    }
    assert!(!files.is_empty(), "corpus dir exists but holds no .ini files");

    let analyzer = Analyzer::embedded();

    // Pass 1: index definitions across the whole corpus so unresolved-reference
    // diagnostics reflect cross-file reality.
    let mut wsindex = WorkspaceIndex::new();
    let mut panics: Vec<(String, String)> = Vec::new();
    let mut parses = Vec::with_capacity(files.len());
    for (rel, path) in &files {
        let rel = rel.clone();
        let text = read_lossy(path);
        let result = catch_unwind(AssertUnwindSafe(|| analyzer.parse(&text)));
        match result {
            Ok(parse) => {
                wsindex.set_file(&rel, index::definitions_in(&analyzer, &parse, &rel));
                parses.push((rel, text, Some(parse)));
            }
            Err(e) => {
                let msg = e
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| e.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "non-string panic".into());
                panics.push((rel.clone(), format!("parse: {msg}")));
                parses.push((rel, text, None));
            }
        }
    }

    // Pass 2: timed parse + diagnose per file, with a histogram by code.
    let mut parse_times = Vec::new();
    let mut diag_times = Vec::new();
    let mut histogram: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut by_message: BTreeMap<String, usize> = BTreeMap::new();
    let mut syntax_files: BTreeMap<String, usize> = BTreeMap::new();
    let mut slowest: Vec<(Duration, String, usize)> = Vec::new();

    for (rel, text, parse) in &parses {
        if parse.is_none() {
            continue;
        }
        let t0 = Instant::now();
        let parse = analyzer.parse(text);
        let parse_t = t0.elapsed();

        let t1 = Instant::now();
        let diags = catch_unwind(AssertUnwindSafe(|| {
            diagnostics::diagnose(&analyzer, &parse, Some(&wsindex), Some(rel))
        }));
        let diag_t = t1.elapsed();
        let diags = match diags {
            Ok(d) => d,
            Err(_) => {
                panics.push((rel.clone(), "diagnose panicked".into()));
                continue;
            }
        };

        for d in &diags {
            *histogram.entry(d.code).or_default() += 1;
            *by_message.entry(d.message.clone()).or_default() += 1;
            if d.code == "syntax" || d.code == "unknown-block" {
                *syntax_files.entry(rel.clone()).or_default() += 1;
                let line = text[..d.span.start as usize].lines().count();
                let snippet: String = text[d.span.start as usize..]
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect();
                println!("  [{}] {}:{line}: {} | {snippet}", d.code, rel, d.message);
            }
        }
        parse_times.push(parse_t);
        diag_times.push(diag_t);
        slowest.push((parse_t + diag_t, rel.clone(), text.lines().count()));
    }

    parse_times.sort();
    diag_times.sort();
    slowest.sort();
    slowest.reverse();

    println!("== corpus: {} files ==", files.len());
    println!(
        "parse    p50 {:?}  p95 {:?}  max {:?}",
        percentile(&parse_times, 0.50),
        percentile(&parse_times, 0.95),
        parse_times.last().unwrap()
    );
    println!(
        "diagnose p50 {:?}  p95 {:?}  max {:?}",
        percentile(&diag_times, 0.50),
        percentile(&diag_times, 0.95),
        diag_times.last().unwrap()
    );
    println!("\nslowest files (parse+diagnose):");
    for (t, rel, lines) in slowest.iter().take(5) {
        println!("  {t:>10.2?}  {rel} ({lines} lines)");
    }
    println!("\ndiagnostic histogram:");
    for (code, n) in &histogram {
        println!("  {n:7}  {code}");
    }
    println!("\ntop messages:");
    let mut msgs: Vec<_> = by_message.iter().collect();
    msgs.sort_by(|a, b| b.1.cmp(a.1));
    for (msg, n) in msgs.iter().take(25) {
        println!("  {n:7}  {msg}");
    }
    if !syntax_files.is_empty() {
        println!("\nfiles with `syntax` diagnostics (oracle/parser gaps):");
        let mut sf: Vec<_> = syntax_files.iter().collect();
        sf.sort_by(|a, b| b.1.cmp(a.1));
        for (rel, n) in sf.iter().take(20) {
            println!("  {n:7}  {rel}");
        }
    }

    assert!(panics.is_empty(), "panics on corpus files: {panics:?}");
    let syntax = histogram.get("syntax").copied().unwrap_or(0);
    let unknown_block = histogram.get("unknown-block").copied().unwrap_or(0);
    assert_eq!(
        (syntax, unknown_block),
        (0, 0),
        "oracle/schema-structure gap: {syntax} syntax + {unknown_block} \
         unknown-block diagnostics on real game data (see [..] lines above)"
    );
}
