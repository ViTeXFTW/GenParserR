// SPDX-License-Identifier: GPL-3.0-or-later
//! Equivalence fuzzing for the incremental reparse: random documents, random
//! edit sequences, asserting after every edit that the spliced parse is
//! byte-for-byte and node-for-node identical to a fresh full parse.
//!
//! Deterministic (fixed LCG seeds) so failures are reproducible; on failure
//! the seed, round, and edit are printed.

use genparser_syntax::parser::FixedOpeners;
use genparser_syntax::{parse, reparse, Edit, Strategy};

/// Minimal deterministic LCG (same constants as MMIX).
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

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

fn openers() -> FixedOpeners {
    FixedOpeners::new(["Object", "Weapon", "Draw", "Body", "ConditionState"])
}

/// Generate a random INI-shaped document: blocks, nested scopes, fields,
/// comments, blank lines, stray `End`s, unterminated scopes, CRLF mix.
fn gen_doc(rng: &mut Rng) -> String {
    let newline = if rng.below(4) == 0 { "\r\n" } else { "\n" };
    let mut out = String::new();
    for _ in 0..rng.below(12) + 1 {
        match rng.below(10) {
            0 => out.push_str(newline), // blank line
            1 => {
                out.push_str("; comment text");
                out.push_str(newline);
            }
            2 => {
                // stray End at file scope
                out.push_str("End");
                out.push_str(newline);
            }
            _ => {
                let kw = rng.pick(&["Object", "Weapon"]);
                out.push_str(kw);
                out.push(' ');
                out.push_str(rng.pick(&["Alpha", "Bravo", "Charlie"]));
                out.push_str(newline);
                for _ in 0..rng.below(4) {
                    match rng.below(4) {
                        0 => {
                            // nested scope, possibly left open
                            out.push_str("  Draw = W3DModelDraw Tag");
                            out.push_str(newline);
                            out.push_str("    Model = TANK");
                            out.push_str(newline);
                            if rng.below(8) != 0 {
                                out.push_str("  End");
                                out.push_str(newline);
                            }
                        }
                        1 => {
                            out.push_str("  Name = \"quoted value\"");
                            out.push_str(newline);
                        }
                        _ => {
                            out.push_str("  ClipSize = 30 ; trailing");
                            out.push_str(newline);
                        }
                    }
                }
                if rng.below(8) != 0 {
                    out.push_str("End");
                    out.push_str(newline);
                }
            }
        }
    }
    out
}

/// Pick a random edit, biased toward the structure-sensitive cases: deleting
/// or inserting `End` lines, block headers, newlines, and boundary touches.
fn gen_edit(rng: &mut Rng, text: &str) -> (usize, usize, String) {
    // Only ASCII is generated, so any byte offset is a char boundary.
    let len = text.len();
    let start = rng.below(len + 1);
    match rng.below(8) {
        0 => {
            // insert a structural line at a random spot
            let ins = rng.pick(&[
                "End\n",
                "Weapon Inserted\n",
                "Object X\n  ClipSize = 1\nEnd\n",
                "\n",
                "  Draw = W3DModelDraw T\n",
                "; note\n",
            ]);
            (start, start, ins.to_string())
        }
        1 => {
            // delete up to 8 bytes (can erase an `End` or a newline)
            let end = (start + rng.below(8) + 1).min(len);
            (start, end, String::new())
        }
        2 => {
            // delete a whole random span (can erase several lines)
            let end = (start + rng.below(40)).min(len);
            (start, end, String::new())
        }
        3 => {
            // replace a span with a partial line (no trailing newline)
            let end = (start + rng.below(12)).min(len);
            (
                start,
                end,
                rng.pick(&["End", "Weapon W", "  X = 1", "\r"]).to_string(),
            )
        }
        _ => {
            // small in-place typing
            let end = (start + rng.below(3)).min(len);
            (
                start,
                end,
                rng.pick(&["5", "ab", " = ", "End", "\n"]).to_string(),
            )
        }
    }
}

#[test]
fn random_edit_sequences_match_full_parse() {
    let oracle = openers();
    let mut spliced = 0usize;
    let mut full = 0usize;

    for seed in 0..40u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) + 1);
        let mut text = gen_doc(&mut rng);
        let mut old = parse(&text, &oracle);

        for round in 0..60 {
            let (start, old_end, ins) = gen_edit(&mut rng, &text);
            let new_text = format!("{}{}{}", &text[..start], ins, &text[old_end..]);
            let edit = Edit {
                start,
                old_end,
                new_len: ins.len(),
            };
            let (inc, strategy) = reparse(&old, &text, &new_text, edit, &oracle);
            match strategy {
                Strategy::Spliced => spliced += 1,
                Strategy::Full => full += 1,
            }

            let reference = parse(&new_text, &oracle);
            let ctx = format!(
                "seed {seed} round {round} edit {edit:?} insert {ins:?}\nold text: {text:?}\nnew text: {new_text:?}"
            );
            assert_eq!(
                inc.syntax().text().to_string(),
                new_text,
                "text round-trip\n{ctx}"
            );
            assert_eq!(
                format!("{:#?}", inc.syntax()),
                format!("{:#?}", reference.syntax()),
                "tree structure\n{ctx}"
            );
            assert_eq!(inc.errors, reference.errors, "errors\n{ctx}");

            text = new_text;
            old = inc; // chain: next edit applies to the spliced parse
        }
    }

    // The whole point is splicing; make sure the fast path actually dominates.
    println!("spliced {spliced}, full-parse fallbacks {full}");
    assert!(
        spliced > full,
        "splice fallback dominates: {spliced} spliced vs {full} full"
    );
}
