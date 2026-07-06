//! Formatting: an indentation normalizer over the lossless CST
//! (`textDocument/formatting`). Reindents each line to its scope depth and
//! touches nothing else — token spacing, casing, blank lines, and comment
//! lines (whose intended scope is ambiguous) are left exactly as written.

use zerosyntax_syntax::{Parse, SyntaxKind};

use crate::Span;

/// One whitespace replacement: substitute `new_text` for the text at `span`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FmtEdit {
    pub span: Span,
    pub new_text: String,
}

/// Edits closer than this many unchanged bytes are merged into one edit
/// (carrying the unchanged text between them). A wildly misindented real game
/// file otherwise produces one edit per line — tens of thousands — and the
/// editor-side cost of applying and echoing that many discrete edits dwarfs
/// the formatting itself. Scattered touch-ups stay minimal: a gap this large
/// only closes within a run of consecutive reformatted lines.
const COALESCE_GAP: u32 = 512;

/// Compute the indentation edits to normalize `text`. `indent` is one level's
/// worth of whitespace (e.g. `"  "` or `"\t"`), from the client's formatting
/// options. Edits are per misindented line, then runs of nearby edits are
/// coalesced (see [`COALESCE_GAP`]).
pub fn format_edits(parse: &Parse, text: &str, indent: &str) -> Vec<FmtEdit> {
    // One ordered walk collecting (token end, scope depth) for every token,
    // so the line loop resolves each line's depth with a forward cursor. A
    // per-line `token_at_offset` would rescan the root's children every call,
    // which is quadratic on real game files (tens of thousands of lines over
    // thousands of top-level blocks).
    let depths = token_depths(&parse.syntax());
    let mut cursor = 0usize;
    let mut depth_for = |offset: u32| -> usize {
        while cursor + 1 < depths.len() && depths[cursor].0 <= offset {
            cursor += 1;
        }
        depths.get(cursor).map_or(0, |&(_, d)| d as usize)
    };

    let mut out = Vec::new();
    let mut line_start = 0usize;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let first_sig = content
            .as_bytes()
            .iter()
            .position(|b| *b != b' ' && *b != b'\t');
        if let Some(ws_len) = first_sig {
            // Comment-only lines keep their hand-placed indentation: the CST
            // attaches them to the enclosing node, which can't distinguish a
            // body comment from one annotating the header or the `End`.
            if content.as_bytes()[ws_len] != b';' {
                let depth = depth_for((line_start + ws_len) as u32);
                let desired = indent.repeat(depth);
                if desired != content[..ws_len] {
                    out.push(FmtEdit {
                        span: Span::new(line_start as u32, (line_start + ws_len) as u32),
                        new_text: desired,
                    });
                }
            }
        }
        line_start += line.len();
    }
    coalesce(out, text)
}

/// Merge edits separated by fewer than [`COALESCE_GAP`] unchanged bytes,
/// splicing the untouched text between them into the merged replacement. The
/// applied result is identical; only the edit count (and thus the client-side
/// application cost) changes.
fn coalesce(edits: Vec<FmtEdit>, text: &str) -> Vec<FmtEdit> {
    let mut out: Vec<FmtEdit> = Vec::new();
    for e in edits {
        match out.last_mut() {
            Some(prev) if e.span.start - prev.span.end < COALESCE_GAP => {
                prev.new_text
                    .push_str(&text[prev.span.end as usize..e.span.start as usize]);
                prev.new_text.push_str(&e.new_text);
                prev.span.end = e.span.end;
            }
            _ => out.push(e),
        }
    }
    out
}

/// `(end offset, scope depth)` for every token, in document order. Fields sit
/// one level inside their scope; a scope's own header and `End` tokens (those
/// parented directly by the `BLOCK`/`MODULE` node) sit at the depth of the
/// scope's parent.
fn token_depths(root: &zerosyntax_syntax::SyntaxNode) -> Vec<(u32, u32)> {
    let mut depths = Vec::new();
    let mut depth: u32 = 0;
    let mut parents: Vec<SyntaxKind> = Vec::new();
    for ev in root.preorder_with_tokens() {
        match ev {
            rowan::WalkEvent::Enter(rowan::NodeOrToken::Node(n)) => {
                if matches!(n.kind(), SyntaxKind::BLOCK | SyntaxKind::MODULE) {
                    depth += 1;
                }
                parents.push(n.kind());
            }
            rowan::WalkEvent::Leave(rowan::NodeOrToken::Node(n)) => {
                if matches!(n.kind(), SyntaxKind::BLOCK | SyntaxKind::MODULE) {
                    depth -= 1;
                }
                parents.pop();
            }
            rowan::WalkEvent::Enter(rowan::NodeOrToken::Token(t)) => {
                let d = match parents.last() {
                    Some(SyntaxKind::BLOCK | SyntaxKind::MODULE) => depth.saturating_sub(1),
                    _ => depth,
                };
                depths.push((u32::from(t.text_range().end()), d));
            }
            rowan::WalkEvent::Leave(rowan::NodeOrToken::Token(_)) => {}
        }
    }
    depths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Analyzer;

    fn apply(text: &str, edits: &[FmtEdit]) -> String {
        let mut out = text.to_string();
        for e in edits.iter().rev() {
            out.replace_range(e.span.start as usize..e.span.end as usize, &e.new_text);
        }
        out
    }

    #[test]
    fn normalizes_indentation_to_scope_depth() {
        let a = Analyzer::embedded();
        let src = "Object Tank\nMaxHealth = 1\n      Behavior = AutoHealBehavior ModuleTag_01\nHealingAmount = 5\n      End\nEnd\n";
        let edits = format_edits(&a.parse(src), src, "  ");
        // The per-line edits sit within COALESCE_GAP of each other, so they
        // come back as a single merged edit.
        assert_eq!(edits.len(), 1);
        let formatted = apply(src, &edits);
        assert_eq!(
            formatted,
            "Object Tank\n  MaxHealth = 1\n  Behavior = AutoHealBehavior ModuleTag_01\n    HealingAmount = 5\n  End\nEnd\n"
        );
    }

    #[test]
    fn distant_edits_stay_separate() {
        let a = Analyzer::embedded();
        // Two misindented lines separated by > COALESCE_GAP bytes of
        // correctly indented filler must not be merged into one edit
        // spanning the untouched middle.
        let filler = "  Filler = 1\n".repeat(50);
        let src = format!("Object Tank\nMaxHealth = 1\n{filler}VisionRange = 2\nEnd\n");
        let edits = format_edits(&a.parse(&src), &src, "  ");
        assert_eq!(edits.len(), 2);
        assert_eq!(
            apply(&src, &edits),
            format!("Object Tank\n  MaxHealth = 1\n{filler}  VisionRange = 2\nEnd\n")
        );
    }

    #[test]
    fn formatted_text_needs_no_edits_and_comments_are_untouched() {
        let a = Analyzer::embedded();
        let src = "; header comment\nObject Tank\n  ; body comment, hand-indented\n  MaxHealth = 1\nEnd\n";
        assert!(format_edits(&a.parse(src), src, "  ").is_empty());
    }
}
