//! Navigation helpers: resolve what schema entity sits under a byte offset, for
//! go-to-definition and hover.

use genparser_schema::{RefKind, ValueType};
use genparser_syntax::ast::{Block, Field};
use genparser_syntax::{Parse, SyntaxKind, SyntaxNode, SyntaxToken};

use crate::model::scope_schema;
use crate::{Analyzer, Span};

/// A reference occurrence: the referenced kind, the name, and its span.
pub struct ReferenceAt {
    pub kind: RefKind,
    pub name: String,
    pub span: Span,
}

/// What the token under the cursor means, for hover.
pub enum HoverInfo {
    Block {
        name: String,
        span: Span,
    },
    Field {
        name: String,
        ty: ValueType,
        parse_fn: String,
        span: Span,
    },
}

fn token_at(root: &SyntaxNode, offset: u32) -> Option<SyntaxToken> {
    let off = rowan::TextSize::from(offset.min(root.text_range().end().into()));
    root.token_at_offset(off)
        .right_biased()
        .or_else(|| root.token_at_offset(off).left_biased())
        .filter(|t| t.kind() == SyntaxKind::WORD || t.kind() == SyntaxKind::STRING)
}

/// If a reference-typed field value sits under `offset`, resolve it.
pub fn reference_at(analyzer: &Analyzer, parse: &Parse, offset: u32) -> Option<ReferenceAt> {
    let root = parse.syntax();
    let tok = token_at(&root, offset)?;
    let field_node = tok.parent().filter(|p| p.kind() == SyntaxKind::FIELD)?;
    let field = Field(field_node.clone());
    // The token must be one of the value tokens, not the key.
    if !field.value_tokens().iter().any(|t| t == &tok) {
        return None;
    }
    let scope = field_node
        .ancestors()
        .skip(1)
        .find(|n| matches!(n.kind(), SyntaxKind::BLOCK | SyntaxKind::MODULE))?;
    let schema = scope_schema(analyzer, &scope);
    let key = field.key()?;
    if let ValueType::Reference { ref_kind } = &schema.field(key.text())?.value_type {
        let name = tok.text().trim_matches('"').to_string();
        return Some(ReferenceAt {
            kind: *ref_kind,
            name,
            span: tok.text_range().into(),
        });
    }
    None
}

/// Resolve hover information for the token under `offset`.
pub fn hover_at(analyzer: &Analyzer, parse: &Parse, offset: u32) -> Option<HoverInfo> {
    let root = parse.syntax();
    let tok = token_at(&root, offset)?;
    let parent = tok.parent()?;
    match parent.kind() {
        SyntaxKind::BLOCK => {
            // Hovering the block keyword.
            let block = Block(parent.clone());
            if block.keyword().as_ref() == Some(&tok) {
                return Some(HoverInfo::Block {
                    name: tok.text().to_string(),
                    span: tok.text_range().into(),
                });
            }
            None
        }
        SyntaxKind::FIELD => {
            let field = Field(parent.clone());
            let key = field.key()?;
            if key != tok {
                return None;
            }
            let scope = parent
                .ancestors()
                .skip(1)
                .find(|n| matches!(n.kind(), SyntaxKind::BLOCK | SyntaxKind::MODULE))?;
            let schema = scope_schema(analyzer, &scope);
            let f = schema.field(key.text())?;
            Some(HoverInfo::Field {
                name: f.name.clone(),
                ty: f.value_type.clone(),
                parse_fn: f.parse_fn.clone(),
                span: tok.text_range().into(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hovers_a_field() {
        let a = Analyzer::embedded();
        let src = "Weapon AK47\n  PrimaryDamage = 50.0\nEnd\n";
        let offset = "Weapon AK47\n  Primary".len() as u32;
        match hover_at(&a, &a.parse(src), offset) {
            Some(HoverInfo::Field { name, ty, .. }) => {
                assert_eq!(name, "PrimaryDamage");
                assert_eq!(ty, ValueType::Real);
            }
            _ => panic!("expected field hover"),
        }
    }
}
