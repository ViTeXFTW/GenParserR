//! Quick fixes (`textDocument/codeAction`): mechanical repairs for the two
//! highest-frequency mistakes — a scope missing its `End`, and a misspelled
//! enum / bitflag member (did-you-mean by edit distance against the value
//! set).

use genparser_schema::ValueType;
use genparser_syntax::ast::Field;
use genparser_syntax::{Parse, SyntaxErrorKind, SyntaxKind, SyntaxNode, SyntaxToken};

use crate::model::scope_schema;
use crate::{Analyzer, Span};

/// One offered fix: replace `span`'s text with `new_text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    pub title: String,
    pub span: Span,
    pub new_text: String,
}

/// All fixes applicable within `range` (the editor's selection/cursor line).
pub fn fixes(analyzer: &Analyzer, parse: &Parse, text: &str, range: Span) -> Vec<Fix> {
    let mut out = Vec::new();
    insert_missing_ends(parse, text, range, &mut out);
    suggest_members(analyzer, parse, range, &mut out);
    out
}

/// For each unterminated scope intersecting `range`, offer to close it. The
/// parser reports these only at EOF (an open scope swallows the rest of the
/// file), so the fix appends an `End` line at the end of the text, indented
/// like the scope's header line.
fn insert_missing_ends(parse: &Parse, text: &str, range: Span, out: &mut Vec<Fix>) {
    for err in &parse.errors {
        if err.kind != SyntaxErrorKind::UnterminatedBlock {
            continue;
        }
        let (start, end) = (err.start as u32, err.end as u32);
        if end < range.start || start > range.end {
            continue;
        }
        let line_start = text[..err.start].rfind('\n').map_or(0, |i| i + 1);
        let indent = &text[line_start..err.start];
        let head = text[err.start..].split_whitespace().next().unwrap_or("?");
        let lead = if text.ends_with('\n') || text.is_empty() { "" } else { "\n" };
        let at = text.len() as u32;
        out.push(Fix {
            title: format!("Insert missing `End` for `{head}`"),
            span: Span::new(at, at),
            new_text: format!("{lead}{indent}End\n"),
        });
    }
}

/// For misspelled enum / bitflag members under `range`, offer the closest
/// value-set members (edit distance ≤ 2, best 3).
fn suggest_members(analyzer: &Analyzer, parse: &Parse, range: Span, out: &mut Vec<Fix>) {
    suggest_in_node(analyzer, &parse.syntax(), range, out);
}

fn suggest_in_node(analyzer: &Analyzer, node: &SyntaxNode, range: Span, out: &mut Vec<Fix>) {
    for child in node.children() {
        let r = child.text_range();
        if u32::from(r.end()) < range.start || u32::from(r.start()) > range.end {
            continue;
        }
        match child.kind() {
            SyntaxKind::FIELD => suggest_in_field(analyzer, &child, range, out),
            SyntaxKind::BLOCK | SyntaxKind::MODULE => {
                suggest_in_node(analyzer, &child, range, out)
            }
            _ => {}
        }
    }
}

fn suggest_in_field(analyzer: &Analyzer, node: &SyntaxNode, range: Span, out: &mut Vec<Fix>) {
    let field = Field(node.clone());
    let Some(key) = field.key() else { return };
    let Some(scope) = node
        .ancestors()
        .skip(1)
        .find(|n| matches!(n.kind(), SyntaxKind::BLOCK | SyntaxKind::MODULE))
    else {
        return;
    };
    let schema = scope_schema(analyzer, &scope);
    let Some(schema_field) = schema.field(key.text()) else { return };
    let tokens = field.value_tokens();
    let mut check = |tok: &SyntaxToken, value_set: &str, flags: bool| {
        let span = Span::from(tok.text_range());
        if span.end < range.start || span.start > range.end {
            return;
        }
        let (prefix, raw) = match flags {
            true => {
                let raw = tok.text().trim_start_matches(['+', '-']);
                (&tok.text()[..tok.text().len() - raw.len()], raw)
            }
            false => ("", tok.text()),
        };
        if raw.is_empty()
            || (flags && (raw.eq_ignore_ascii_case("NONE") || raw.eq_ignore_ascii_case("ALL")))
        {
            return;
        }
        let Some(set) = analyzer.value_set(value_set) else { return };
        if set.members.iter().any(|m| m.name.eq_ignore_ascii_case(raw)) {
            return; // valid member, nothing to fix
        }
        let mut ranked: Vec<(usize, &str)> = set
            .members
            .iter()
            .map(|m| (edit_distance(raw, &m.name), m.name.as_str()))
            .filter(|(d, _)| *d <= 2)
            .collect();
        ranked.sort();
        for (_, name) in ranked.into_iter().take(3) {
            out.push(Fix {
                title: format!("Replace with `{name}`"),
                span,
                new_text: format!("{prefix}{name}"),
            });
        }
    };
    match &schema_field.value_type {
        ValueType::Enum { value_set } => {
            if let Some(tok) = tokens.first() {
                check(tok, value_set, false);
            }
        }
        ValueType::BitFlags { value_set } => {
            for tok in &tokens {
                check(tok, value_set, true);
            }
        }
        ValueType::TokenList { tokens: specs } => {
            for (spec, tok) in specs.iter().zip(tokens.iter()) {
                match spec {
                    ValueType::Enum { value_set } => check(tok, value_set, false),
                    ValueType::BitFlags { value_set } => check(tok, value_set, true),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Case-insensitive Levenshtein distance, early-exiting via the band bound
/// implied by `ED_MAX` callers use (small inputs; plain DP is fine).
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().map(|c| c.to_ascii_uppercase()).collect();
    let b: Vec<char> = b.chars().map(|c| c.to_ascii_uppercase()).collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Analyzer;

    #[test]
    fn offers_missing_end() {
        let a = Analyzer::embedded();
        let src = "Object Tank\n  MaxHealth = 1\n";
        let parse = a.parse(src);
        let fx = fixes(&a, &parse, src, Span::new(0, src.len() as u32));
        assert_eq!(fx.len(), 1, "{fx:?}");
        assert!(fx[0].title.contains("`End`") && fx[0].title.contains("Object"));
        assert_eq!(fx[0].new_text, "End\n");
        assert_eq!(fx[0].span, Span::new(src.len() as u32, src.len() as u32));
    }

    #[test]
    fn suggests_close_enum_and_flag_members() {
        let a = Analyzer::embedded();
        // `Appearance` is enum locomotor_appearance; TREDS ≈ TREADS.
        let src = "Locomotor L\n  Appearance = TREDS\nEnd\n";
        let parse = a.parse(src);
        let fx = fixes(&a, &parse, src, Span::new(0, src.len() as u32));
        assert!(fx.iter().any(|f| f.new_text == "TREADS"), "{fx:?}");

        // Bitflag with prefix op: the `+` is preserved in the replacement.
        let src2 = "Object T\n  Behavior = SlowDeathBehavior ModuleTag_01\n    DeathTypes = NONE +EXPLODDED\n  End\nEnd\n";
        let parse2 = a.parse(src2);
        let fx2 = fixes(&a, &parse2, src2, Span::new(0, src2.len() as u32));
        assert!(fx2.iter().any(|f| f.new_text == "+EXPLODED"), "{fx2:?}");
    }

    #[test]
    fn valid_members_get_no_fixes() {
        let a = Analyzer::embedded();
        let src = "Locomotor L\n  Appearance = TREADS\nEnd\n";
        let parse = a.parse(src);
        assert!(fixes(&a, &parse, src, Span::new(0, src.len() as u32)).is_empty());
    }
}
