// SPDX-License-Identifier: GPL-3.0-or-later
//! Incremental-reparse equivalence with the *real* schema oracle: random edit
//! sequences over real INI inputs, asserting the spliced parse and its
//! diagnostics are identical to a from-scratch parse.
//!
//! The always-on test runs over the spec corpus in `tests/spec/`; the
//! `#[ignore]`d test sweeps the full game-data corpus (fetch with
//! `scripts/fetch-corpus.ps1`, run via `--ignored`).

use std::path::PathBuf;

use genparser_analysis::diagnostics::{self, DiagnosticsCache};
use genparser_analysis::Analyzer;
use genparser_syntax::{Edit, Strategy};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Largest char boundary `<= i` (corpus files contain lossy-decoded non-ASCII).
fn floor_boundary(text: &str, mut i: usize) -> usize {
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn random_edit(rng: &mut Rng, text: &str) -> (usize, usize, &'static str) {
    const INSERTS: &[&str] = &[
        "",
        "",
        "5",
        "End\n",
        "Weapon Inserted\nEnd\n",
        "  ClipSize = 1\n",
        "\n",
        "; note",
        "Behavior = AutoHealBehavior ModuleTag_Z\n",
    ];
    let start = floor_boundary(text, rng.below(text.len() + 1));
    let end = floor_boundary(text, (start + rng.below(60)).min(text.len()));
    let (start, end) = (start.min(end), start.max(end));
    (start, end, INSERTS[rng.below(INSERTS.len())])
}

/// Drive `rounds` random edits over `text`, asserting equivalence after each.
/// Returns (spliced, full) counts.
fn check_edit_sequence(
    analyzer: &Analyzer,
    label: &str,
    text: &str,
    seed: u64,
    rounds: usize,
) -> (usize, usize) {
    let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) + 1);
    let mut text = text.to_string();
    let mut old = analyzer.parse(&text);
    let mut cache = DiagnosticsCache::new();
    let (mut spliced, mut full) = (0, 0);

    for round in 0..rounds {
        let (start, old_end, ins) = random_edit(&mut rng, &text);
        let new_text = format!("{}{}{}", &text[..start], ins, &text[old_end..]);
        let edit = Edit {
            start,
            old_end,
            new_len: ins.len(),
        };
        let (inc, strategy) = analyzer.reparse(&old, &text, &new_text, edit);
        match strategy {
            Strategy::Spliced => spliced += 1,
            Strategy::Full => full += 1,
        }

        let reference = analyzer.parse(&new_text);
        let ctx = format!("{label}: seed {seed} round {round} edit {edit:?} insert {ins:?}");
        assert_eq!(inc.syntax().text().to_string(), new_text, "text\n{ctx}");
        assert_eq!(
            format!("{:#?}", inc.syntax()),
            format!("{:#?}", reference.syntax()),
            "structure\n{ctx}"
        );
        assert_eq!(inc.errors, reference.errors, "errors\n{ctx}");
        let fresh = diagnostics::diagnose(analyzer, &reference, None);
        assert_eq!(
            diagnostics::diagnose(analyzer, &inc, None),
            fresh,
            "diagnostics\n{ctx}"
        );
        // The per-block cache, fed only spliced trees, must agree exactly.
        assert_eq!(
            diagnostics::diagnose_with_cache(analyzer, &inc, None, &mut cache),
            fresh,
            "cached diagnostics\n{ctx}"
        );

        text = new_text;
        old = inc;
    }
    (spliced, full)
}

fn spec_inis() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/spec");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "ini") {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            out.push((name, std::fs::read_to_string(path).unwrap()));
        }
    }
    out.sort();
    out
}

#[test]
fn spec_corpus_edit_sequences_match_full_parse() {
    let analyzer = Analyzer::embedded();
    let (mut spliced, mut full) = (0, 0);
    for (name, text) in spec_inis() {
        for seed in 0..6 {
            let (s, f) = check_edit_sequence(&analyzer, &name, &text, seed, 30);
            spliced += s;
            full += f;
        }
    }
    println!("spliced {spliced}, full-parse fallbacks {full}");
    assert!(spliced > full);
}

#[test]
#[ignore = "requires corpus/ — run scripts/fetch-corpus.ps1 (or .sh) first"]
fn corpus_edit_sequences_match_full_parse() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../corpus/GeneralsGamePatch2/GeneralsZH/Data/INI");
    assert!(dir.is_dir(), "corpus not found; run scripts/fetch-corpus.ps1");

    let analyzer = Analyzer::embedded();
    let (mut spliced, mut full) = (0, 0);
    let mut stack = vec![dir];
    let mut files = Vec::new();
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("ini")) {
                files.push(p);
            }
        }
    }
    files.sort();
    for (i, path) in files.iter().enumerate() {
        let text = String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned();
        let label = path.display().to_string();
        let (s, f) = check_edit_sequence(&analyzer, &label, &text, i as u64, 8);
        spliced += s;
        full += f;
    }
    println!("spliced {spliced}, full-parse fallbacks {full}");
    assert!(spliced > full);
}
