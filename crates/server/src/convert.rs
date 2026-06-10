//! Conversions between analysis byte offsets and LSP line/character positions,
//! and between analysis result types and `lsp-types`.
//!
//! Positions use a character-column model (LSP `character` == count of Unicode
//! scalar values from the line start). Generals INI files are effectively
//! ASCII, so this matches UTF-16 columns in practice; non-BMP text is the only
//! unsupported edge case.

use genparser_analysis::completion::{Completion, CompletionKind};
use genparser_analysis::diagnostics::{Diagnostic as AnDiagnostic, Severity};
use genparser_analysis::semantic::SemKind;
use genparser_analysis::Span;
use ropey::Rope;
use tower_lsp::lsp_types::*;

/// Convert a byte offset to an LSP position via the rope.
pub fn offset_to_position(rope: &Rope, byte: u32) -> Position {
    let byte = (byte as usize).min(rope.len_bytes());
    let char = rope.byte_to_char(byte);
    let line = rope.char_to_line(char);
    let line_start = rope.line_to_char(line);
    Position {
        line: line as u32,
        character: (char - line_start) as u32,
    }
}

/// Convert an LSP position to a byte offset via the rope (clamped to bounds).
pub fn position_to_offset(rope: &Rope, pos: Position) -> u32 {
    let line = (pos.line as usize).min(rope.len_lines().saturating_sub(1));
    let line_start = rope.line_to_char(line);
    let line_len = rope.line(line).len_chars();
    let char = line_start + (pos.character as usize).min(line_len);
    rope.char_to_byte(char) as u32
}

/// Convert an analysis span to an LSP range.
pub fn span_to_range(rope: &Rope, span: Span) -> Range {
    Range {
        start: offset_to_position(rope, span.start),
        end: offset_to_position(rope, span.end),
    }
}

/// Convert an analysis diagnostic to an LSP diagnostic.
pub fn to_lsp_diagnostic(rope: &Rope, d: &AnDiagnostic) -> Diagnostic {
    Diagnostic {
        range: span_to_range(rope, d.span),
        severity: Some(match d.severity {
            Severity::Error => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
            Severity::Hint => DiagnosticSeverity::HINT,
        }),
        code: Some(NumberOrString::String(d.code.to_string())),
        source: Some("genparser".to_string()),
        message: d.message.clone(),
        ..Default::default()
    }
}

/// Convert an analysis completion to an LSP completion item.
pub fn to_lsp_completion(c: Completion) -> CompletionItem {
    CompletionItem {
        label: c.label,
        kind: Some(match c.kind {
            CompletionKind::Block => CompletionItemKind::CLASS,
            CompletionKind::Field => CompletionItemKind::PROPERTY,
            CompletionKind::Module => CompletionItemKind::CONSTRUCTOR,
            CompletionKind::EnumMember => CompletionItemKind::ENUM_MEMBER,
            CompletionKind::Value => CompletionItemKind::VALUE,
            CompletionKind::Reference => CompletionItemKind::REFERENCE,
        }),
        detail: c.detail,
        ..Default::default()
    }
}

/// The semantic-token legend (type names), ordered to match [`sem_kind_index`].
pub fn semantic_legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::KEYWORD,    // 0
            SemanticTokenType::CLASS,      // 1
            SemanticTokenType::PROPERTY,   // 2
            SemanticTokenType::TYPE,       // 3
            SemanticTokenType::ENUM_MEMBER,// 4
            SemanticTokenType::OPERATOR,   // 5
            SemanticTokenType::NUMBER,     // 6
            SemanticTokenType::STRING,     // 7
            SemanticTokenType::VARIABLE,   // 8
            SemanticTokenType::COMMENT,    // 9
        ],
        token_modifiers: vec![],
    }
}

fn sem_kind_index(kind: SemKind) -> u32 {
    match kind {
        SemKind::Keyword => 0,
        SemKind::BlockName => 1,
        SemKind::Field => 2,
        SemKind::Module => 3,
        SemKind::EnumMember => 4,
        SemKind::Operator => 5,
        SemKind::Number => 6,
        SemKind::StringLit => 7,
        SemKind::Reference => 8,
        SemKind::Comment => 9,
    }
}

/// Delta-encode analysis semantic tokens into the LSP wire format. Input must be
/// sorted by start offset (as produced by `semantic_tokens`).
pub fn to_lsp_semantic_tokens(
    rope: &Rope,
    tokens: &[genparser_analysis::semantic::SemToken],
) -> Vec<SemanticToken> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;
    for t in tokens {
        let pos = offset_to_position(rope, t.span.start);
        let start_char = rope.byte_to_char((t.span.start as usize).min(rope.len_bytes()));
        let end_char = rope.byte_to_char((t.span.end as usize).min(rope.len_bytes()));
        let length = (end_char - start_char) as u32;
        let delta_line = pos.line - prev_line;
        let delta_start = if delta_line == 0 {
            pos.character - prev_start
        } else {
            pos.character
        };
        out.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type: sem_kind_index(t.kind),
            token_modifiers_bitset: 0,
        });
        prev_line = pos.line;
        prev_start = pos.character;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use genparser_analysis::semantic::{SemKind, SemToken};

    #[test]
    fn position_offset_round_trip() {
        let rope = Rope::from_str("Weapon AK47\n  PrimaryDamage = 50.0\nEnd\n");
        for byte in [0u32, 7, 14, 30, 35] {
            let pos = offset_to_position(&rope, byte);
            assert_eq!(position_to_offset(&rope, pos), byte, "byte {byte} via {pos:?}");
        }
    }

    #[test]
    fn position_on_second_line() {
        let rope = Rope::from_str("Weapon AK47\n  PrimaryDamage = 50.0\nEnd\n");
        // The 'P' of PrimaryDamage is at line 1, char 2.
        let byte = "Weapon AK47\n  ".len() as u32;
        assert_eq!(offset_to_position(&rope, byte), Position { line: 1, character: 2 });
    }

    #[test]
    fn delta_encodes_tokens_across_lines() {
        let rope = Rope::from_str("Weapon AK47\nEnd\n");
        let tokens = vec![
            SemToken { span: Span::new(0, 6), kind: SemKind::Keyword },   // "Weapon" line0 col0
            SemToken { span: Span::new(7, 11), kind: SemKind::BlockName },// "AK47"   line0 col7
            SemToken { span: Span::new(12, 15), kind: SemKind::Keyword }, // "End"    line1 col0
        ];
        let lsp = to_lsp_semantic_tokens(&rope, &tokens);
        assert_eq!(lsp[0].delta_line, 0);
        assert_eq!(lsp[0].delta_start, 0);
        assert_eq!(lsp[0].length, 6);
        assert_eq!(lsp[1].delta_line, 0);
        assert_eq!(lsp[1].delta_start, 7); // 7 - 0
        assert_eq!(lsp[2].delta_line, 1);
        assert_eq!(lsp[2].delta_start, 0); // new line resets column
        assert_eq!(lsp[2].length, 3);
    }
}
