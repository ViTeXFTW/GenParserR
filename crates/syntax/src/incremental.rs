//! Incremental reparse: block-granularity reparse-and-splice.
//!
//! At file scope every line belongs to exactly one top-level child of ROOT (a
//! BLOCK, a FIELD directive, a stray-`End` ERROR node, or a trivia token), and
//! a top-level child's parse depends only on its own text: the scope stack is
//! empty at every top-level boundary, the lexer is line-local (no token spans
//! a newline), and the [`OpenerOracle`] sees only the enclosing-scope keyword
//! chain, which is child-internal. So reparsing the edited region as a
//! fragment and splicing its children into the old ROOT is *exactly*
//! equivalent to a full reparse — provided the region is widened to line
//! boundaries and the fragment does not end with an open scope that would
//! swallow the text after it (in which case we fall back to a full parse).
//!
//! Splicing keeps the green nodes of untouched top-level children
//! pointer-identical, which downstream per-block analysis caches key on.

use rowan::{GreenNode, GreenNodeData, GreenToken, GreenTokenData, NodeOrToken};

use crate::kind::SyntaxKind;
use crate::parser::{parse, OpenerOracle, Parse, SyntaxError, SyntaxErrorKind};

/// A single text edit: bytes `[start, old_end)` of the old text were replaced
/// by `new_len` bytes (occupying `[start, start + new_len)` of the new text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edit {
    pub start: usize,
    pub old_end: usize,
    pub new_len: usize,
}

/// How [`reparse`] produced its result, for observability of the fallback rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// The edited region was reparsed and spliced into the old tree.
    Spliced,
    /// The whole document was reparsed from scratch.
    Full,
}

/// Reparse after an edit. `old` must be the parse of `old_text`, and
/// `new_text` must be `old_text` with `edit` applied; the result is always
/// identical to `parse(new_text, oracle)` (equivalence is fuzz-tested).
pub fn reparse(
    old: &Parse,
    old_text: &str,
    new_text: &str,
    edit: Edit,
    oracle: &dyn OpenerOracle,
) -> (Parse, Strategy) {
    match try_splice(old, old_text, new_text, edit, oracle) {
        Some(p) => (p, Strategy::Spliced),
        None => (parse(new_text, oracle), Strategy::Full),
    }
}

fn try_splice(
    old: &Parse,
    old_text: &str,
    new_text: &str,
    edit: Edit,
    oracle: &dyn OpenerOracle,
) -> Option<Parse> {
    // The edit must describe `old_text -> new_text` consistently.
    if edit.start > edit.old_end || edit.old_end > old_text.len() {
        return None;
    }
    if old_text.len() - (edit.old_end - edit.start) + edit.new_len != new_text.len() {
        return None;
    }

    // Byte spans of the old ROOT's children (contiguous and gap-free).
    let root = old.green();
    let mut spans = Vec::new();
    let mut offset = 0usize;
    for child in root.children() {
        let len = match child {
            NodeOrToken::Node(n) => u32::from(n.text_len()) as usize,
            NodeOrToken::Token(t) => u32::from(t.text_len()) as usize,
        };
        spans.push((offset, offset + len));
        offset += len;
    }
    if spans.is_empty() {
        return None; // empty document; a full parse is trivial anyway
    }
    let n = spans.len();

    // Children touching the edit (inclusive at boundaries, so an insertion
    // between two children involves both), widened one sibling each side.
    let mut lo = spans.iter().position(|&(_, end)| end >= edit.start)?;
    let mut hi = spans.iter().rposition(|&(start, _)| start <= edit.old_end)?;
    if lo > hi {
        return None;
    }
    lo = lo.saturating_sub(1);
    hi = (hi + 1).min(n - 1);

    // Extend to line boundaries: blank-line trivia is emitted as individual
    // ROOT tokens, so child boundaries are token boundaries, not necessarily
    // line boundaries. Requiring a literal `\n` before the region and at its
    // end also rules out a `\r` + `\n` joining across a splice seam.
    let bytes = old_text.as_bytes();
    while lo > 0 && bytes[spans[lo].0 - 1] != b'\n' {
        lo -= 1;
    }
    while hi + 1 < n && bytes[spans[hi].1 - 1] != b'\n' {
        hi += 1;
    }

    let prefix_len = spans[lo].0;
    let old_suffix_start = spans[hi].1;
    let suffix_len = old_text.len() - old_suffix_start;
    if edit.start < prefix_len || edit.old_end > old_suffix_start {
        return None;
    }
    let region = new_text.get(prefix_len..new_text.len().checked_sub(suffix_len)?)?;
    if suffix_len > 0 && !region.is_empty() && !region.ends_with('\n') {
        return None; // the edit removed the region's trailing newline
    }

    let fragment = parse(region, oracle);
    // An open scope at the fragment's end would swallow the suffix in a full
    // parse, so the splice would diverge — unless there is no suffix.
    if suffix_len > 0
        && fragment
            .errors
            .iter()
            .any(|e| e.kind == SyntaxErrorKind::UnterminatedBlock)
    {
        return None;
    }

    // Splice: prefix children ++ fragment children ++ suffix children.
    fn own(
        child: NodeOrToken<&GreenNodeData, &GreenTokenData>,
    ) -> NodeOrToken<GreenNode, GreenToken> {
        match child {
            NodeOrToken::Node(node) => NodeOrToken::Node(node.to_owned()),
            NodeOrToken::Token(tok) => NodeOrToken::Token(tok.to_owned()),
        }
    }
    let children: Vec<NodeOrToken<GreenNode, GreenToken>> = root
        .children()
        .take(lo)
        .map(own)
        .chain(fragment.green().children().map(own))
        .chain(root.children().skip(hi + 1).map(own))
        .collect();
    let green = GreenNode::new(SyntaxKind::ROOT.into(), children);

    // Errors: keep the prefix's, rebase the fragment's, shift the suffix's.
    // This preserves full-parse order: stray-`End` errors appear in document
    // order, and unterminated-block errors (possible only at the very end —
    // in the suffix, or in the fragment when there is no suffix) come last.
    let delta = new_text.len() as i64 - old_text.len() as i64;
    let shift = |e: &SyntaxError, by: i64| SyntaxError {
        message: e.message.clone(),
        start: (e.start as i64 + by) as usize,
        end: (e.end as i64 + by) as usize,
        kind: e.kind,
    };
    let mut errors: Vec<SyntaxError> = old
        .errors
        .iter()
        .filter(|e| e.end <= prefix_len)
        .cloned()
        .collect();
    errors.extend(fragment.errors.iter().map(|e| shift(e, prefix_len as i64)));
    errors.extend(
        old.errors
            .iter()
            .filter(|e| e.start >= old_suffix_start)
            .map(|e| shift(e, delta)),
    );

    Some(Parse::from_parts(green, errors))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::FixedOpeners;

    fn openers() -> FixedOpeners {
        FixedOpeners::new(["Object", "Weapon", "Draw", "Body", "ConditionState"])
    }

    /// Apply `edit` (replacing `[start, old_end)` with `insert`), reparse
    /// incrementally, and assert exact equivalence with a fresh full parse.
    /// Returns the strategy used.
    fn check(old_text: &str, start: usize, old_end: usize, insert: &str) -> Strategy {
        let oracle = openers();
        let old = parse(old_text, &oracle);
        let new_text = format!("{}{}{}", &old_text[..start], insert, &old_text[old_end..]);
        let edit = Edit {
            start,
            old_end,
            new_len: insert.len(),
        };
        let (inc, strategy) = reparse(&old, old_text, &new_text, edit, &oracle);
        let full = parse(&new_text, &oracle);
        assert_eq!(
            inc.syntax().text().to_string(),
            new_text,
            "spliced tree must reproduce the new text"
        );
        assert_eq!(
            format!("{:#?}", inc.syntax()),
            format!("{:#?}", full.syntax()),
            "spliced structure must equal a full parse"
        );
        assert_eq!(inc.errors, full.errors, "errors must equal a full parse");
        strategy
    }

    const THREE_BLOCKS: &str = "\
Weapon AK47
  PrimaryDamage = 50.0
End
Object Tank
  Body = ActiveBody Tag01
    MaxHealth = 100
  End
End
Weapon Pistol
  ClipSize = 8
End
";

    #[test]
    fn edit_inside_middle_block_splices() {
        let start = THREE_BLOCKS.find("100").unwrap();
        let s = check(THREE_BLOCKS, start, start + 3, "250");
        assert_eq!(s, Strategy::Spliced);
    }

    #[test]
    fn insert_block_between_blocks_splices() {
        let at = THREE_BLOCKS.find("Object Tank").unwrap();
        let s = check(THREE_BLOCKS, at, at, "Weapon New\n  ClipSize = 1\nEnd\n");
        assert_eq!(s, Strategy::Spliced);
    }

    #[test]
    fn deleting_an_end_falls_back_or_matches() {
        // Deleting the Object's inner `End` leaves the fragment open; with a
        // suffix present this must fall back to a full parse — and match it.
        let at = THREE_BLOCKS.find("  End").unwrap();
        check(THREE_BLOCKS, at, at + 6, "");
    }

    #[test]
    fn edit_inside_trailing_unterminated_block_splices() {
        let src = "Weapon AK47\n  PrimaryDamage = 50.0\nEnd\nObject Tank\n  MaxHealth = 100\n";
        let start = src.find("100").unwrap();
        let s = check(src, start, start + 3, "1");
        assert_eq!(s, Strategy::Spliced);
    }

    #[test]
    fn stray_end_errors_shift_across_an_edit() {
        let src = "End\nWeapon AK47\n  ClipSize = 8\nEnd\nEnd\n";
        let start = src.find('8').unwrap();
        check(src, start, start + 1, "30");
    }

    #[test]
    fn crlf_edits_splice() {
        let src = "Weapon AK47\r\n  ClipSize = 8\r\nEnd\r\nWeapon B\r\nEnd\r\n";
        let start = src.find('8').unwrap();
        let s = check(src, start, start + 1, "12");
        assert_eq!(s, Strategy::Spliced);
    }

    #[test]
    fn deleting_a_trailing_newline_stays_equivalent() {
        // Joins the End line with the next block header; must not splice a
        // half line.
        let at = THREE_BLOCKS.find("End\nObject").unwrap();
        check(THREE_BLOCKS, at + 3, at + 4, "");
    }

    #[test]
    fn append_at_eof_splices() {
        let s = check(
            THREE_BLOCKS,
            THREE_BLOCKS.len(),
            THREE_BLOCKS.len(),
            "Weapon Tail\nEnd\n",
        );
        assert_eq!(s, Strategy::Spliced);
    }

    #[test]
    fn full_replacement_matches() {
        check(THREE_BLOCKS, 0, THREE_BLOCKS.len(), "Weapon Only\nEnd\n");
    }

    #[test]
    fn edit_in_file_scope_trivia_matches() {
        let src = "; header\n\nWeapon AK47\nEnd\n";
        let at = src.find("header").unwrap();
        check(src, at, at + 6, "banner");
    }
}
