//! Formatting: an indentation normalizer over the lossless CST
//! (`textDocument/formatting`). Reindents each line to its scope depth and
//! touches nothing else — token spacing, casing, blank lines, and comment
//! lines (whose intended scope is ambiguous) are left exactly as written.

use genparser_syntax::{Parse, SyntaxKind};

use crate::Span;

/// One whitespace replacement: substitute `new_text` for the text at `span`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FmtEdit {
    pub span: Span,
    pub new_text: String,
}

/// Compute the minimal indentation edits to normalize `text`. `indent` is one
/// level's worth of whitespace (e.g. `"  "` or `"\t"`), from the client's
/// formatting options.
pub fn format_edits(parse: &Parse, text: &str, indent: &str) -> Vec<FmtEdit> {
    let root = parse.syntax();
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
                let depth = depth_at(&root, (line_start + ws_len) as u32);
                let desired = indent.repeat(depth);
                if desired != &content[..ws_len] {
                    out.push(FmtEdit {
                        span: Span::new(line_start as u32, (line_start + ws_len) as u32),
                        new_text: desired,
                    });
                }
            }
        }
        line_start += line.len();
    }
    out
}

/// The scope depth of the significant token at `offset`: fields sit one level
/// inside their scope; a scope's own header and `End` lines sit at the depth
/// of the scope's parent.
fn depth_at(root: &genparser_syntax::SyntaxNode, offset: u32) -> usize {
    let off = rowan::TextSize::from(offset);
    let Some(tok) = root.token_at_offset(off).right_biased() else {
        return 0;
    };
    let Some(parent) = tok.parent() else { return 0 };
    let scopes = parent
        .ancestors()
        .filter(|n| matches!(n.kind(), SyntaxKind::BLOCK | SyntaxKind::MODULE))
        .count();
    match parent.kind() {
        // Header words and `End` belong to the scope node itself.
        SyntaxKind::BLOCK | SyntaxKind::MODULE => scopes.saturating_sub(1),
        _ => scopes,
    }
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
        let formatted = apply(src, &edits);
        assert_eq!(
            formatted,
            "Object Tank\n  MaxHealth = 1\n  Behavior = AutoHealBehavior ModuleTag_01\n    HealingAmount = 5\n  End\nEnd\n"
        );
    }

    #[test]
    fn formatted_text_needs_no_edits_and_comments_are_untouched() {
        let a = Analyzer::embedded();
        let src = "; header comment\nObject Tank\n  ; body comment, hand-indented\n  MaxHealth = 1\nEnd\n";
        assert!(format_edits(&a.parse(src), src, "  ").is_empty());
    }
}
