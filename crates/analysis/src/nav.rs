//! Navigation helpers: resolve what schema entity sits under a byte offset, for
//! go-to-definition and hover.

use zerosyntax_schema::{RefKind, ValueType};
use zerosyntax_syntax::ast::{Block, Field};
use zerosyntax_syntax::{Parse, SyntaxKind, SyntaxNode, SyntaxToken};

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

/// If a reference-typed field value sits under `offset`, resolve it. Handles
/// single references, every token of a `ReferenceList`, and reference-typed
/// elements of a `token_list`.
pub fn reference_at(analyzer: &Analyzer, parse: &Parse, offset: u32) -> Option<ReferenceAt> {
    let root = parse.syntax();
    let tok = token_at(&root, offset)?;
    let field_node = tok.parent().filter(|p| p.kind() == SyntaxKind::FIELD)?;
    let field = Field(field_node.clone());
    // The token must be one of the value tokens, not the key.
    let pos = field.value_tokens().iter().position(|t| t == &tok)?;
    let scope = field_node
        .ancestors()
        .skip(1)
        .find(|n| matches!(n.kind(), SyntaxKind::BLOCK | SyntaxKind::MODULE))?;
    let schema = scope_schema(analyzer, &scope);
    let key = field.key()?;
    let value_tokens = field.value_tokens();
    let value_type = &schema.field(key.text())?.value_type;
    let active_type = value_type
        .variant_for_first_token(value_tokens.first().map(|t| t.text().trim_matches('"')))?;
    let input = value_tokens
        .iter()
        .map(|token| token.text().trim_matches('"'))
        .collect::<Vec<_>>();
    let element_type = active_type.token_type_at_input(&input, pos)?;
    let ref_kind = match element_type {
        ValueType::Reference { ref_kind } | ValueType::ReferenceList { ref_kind } => *ref_kind,
        ValueType::Prefixed { value_type, .. } => match value_type.as_ref() {
            ValueType::Reference { ref_kind } | ValueType::ReferenceList { ref_kind } => *ref_kind,
            _ => return None,
        },
        _ => return None,
    };
    let (name, span) = match element_type {
        ValueType::Prefixed { prefix, .. } => {
            let text = tok.text().trim_matches('"');
            let (actual, name) = text.split_once(':')?;
            if !actual.eq_ignore_ascii_case(prefix) || name.is_empty() {
                return None;
            }
            let start = u32::from(tok.text_range().start())
                + u32::from(tok.text().starts_with('"'))
                + actual.len() as u32
                + 1;
            (
                name.to_string(),
                crate::Span {
                    start,
                    end: start + name.len() as u32,
                },
            )
        }
        _ => (
            tok.text().trim_matches('"').to_string(),
            tok.text_range().into(),
        ),
    };
    Some(ReferenceAt {
        kind: ref_kind,
        name,
        span,
    })
}

/// If `offset` sits on a *definition's* name token (the second header word of
/// a block whose keyword `defines` a reference kind), resolve it. Together
/// with [`reference_at`] this powers find-references and rename from either
/// end of an edge.
pub fn definition_at(analyzer: &Analyzer, parse: &Parse, offset: u32) -> Option<ReferenceAt> {
    let root = parse.syntax();
    let tok = token_at(&root, offset)?;
    let block_node = tok.parent().filter(|p| p.kind() == SyntaxKind::BLOCK)?;
    let block = Block(block_node);
    if block.name().as_ref() != Some(&tok) {
        return None;
    }
    let kind = analyzer.block(block.keyword()?.text())?.defines?;
    Some(ReferenceAt {
        kind,
        name: tok.text().to_string(),
        span: tok.text_range().into(),
    })
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

    #[test]
    fn split_prefixed_reference_span_excludes_prefix() {
        let a = Analyzer::embedded();
        let src = "Object Tank\n  Behavior = TransitionDamageFX ModuleTag_01\n    DamagedParticleSystem1 = Bone: NONE RandomBone: No PSys: MissingParticle\n  End\nEnd\n";
        let offset = src.find("MissingParticle").unwrap() as u32;
        let reference = reference_at(&a, &a.parse(src), offset).unwrap();
        assert_eq!(reference.kind, RefKind::ParticleSystem);
        assert_eq!(reference.name, "MissingParticle");
        assert_eq!(
            &src[reference.span.start as usize..reference.span.end as usize],
            "MissingParticle"
        );
    }
}
