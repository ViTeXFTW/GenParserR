//! Semantic tokens: classify each token for highlighting, using the schema so
//! enum members, references, numbers, etc. are coloured by meaning rather than
//! by lexical shape. The server maps [`SemKind`] to LSP semantic token types.

use std::collections::BTreeMap;

use genparser_schema::ValueType;
use genparser_syntax::ast::{Block, Field, Module};
use genparser_syntax::{Parse, SyntaxKind, SyntaxNode, SyntaxToken};

use crate::model::{scope_schema, ScopeSchema};
use crate::{Analyzer, Span};

/// Semantic classification of a token. Names mirror the project's documented
/// mapping to standard LSP `SemanticTokenType`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemKind {
    /// Block keyword (`Object`, `Weapon`, `End`) and module-slot keywords.
    Keyword,
    /// A block/definition name (`AK47`).
    BlockName,
    /// A field key (left of `=`).
    Field,
    /// A module type name (`W3DModelDraw`).
    Module,
    /// An enum / bitflag member, or a boolean literal.
    EnumMember,
    /// `=` and bitflag `+`/`-` operators.
    Operator,
    /// A numeric literal.
    Number,
    /// A quoted or asset string.
    StringLit,
    /// A reference to another named definition.
    Reference,
    /// A comment.
    Comment,
}

/// A classified token over a byte span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemToken {
    pub span: Span,
    pub kind: SemKind,
}

/// Produce semantic tokens for the whole document, sorted by start offset and
/// non-overlapping (one classification per token).
pub fn semantic_tokens(analyzer: &Analyzer, parse: &Parse) -> Vec<SemToken> {
    let mut sem = Sem {
        analyzer,
        by_start: BTreeMap::new(),
    };
    let root = parse.syntax();

    // Structured pass: headers and field values, schema-aware.
    for node in root.children() {
        match node.kind() {
            SyntaxKind::BLOCK => {
                let schema = scope_schema(analyzer, &node);
                sem.block_header(&node);
                sem.walk(&node, &schema);
            }
            SyntaxKind::FIELD => sem.field(&Field(node.clone()), &ScopeSchema::Unknown),
            _ => {}
        }
    }

    // Lexical pass: comments, `=`, `End`, and any uncovered strings.
    for el in root.descendants_with_tokens() {
        if let Some(tok) = el.as_token() {
            match tok.kind() {
                SyntaxKind::COMMENT => sem.set(tok, SemKind::Comment),
                SyntaxKind::EQUALS => sem.set(tok, SemKind::Operator),
                SyntaxKind::WORD if tok.text().eq_ignore_ascii_case("End") => {
                    sem.set(tok, SemKind::Keyword)
                }
                SyntaxKind::STRING => sem.set_if_absent(tok, SemKind::StringLit),
                _ => {}
            }
        }
    }

    sem.by_start.into_values().collect()
}

struct Sem<'a> {
    analyzer: &'a Analyzer,
    by_start: BTreeMap<u32, SemToken>,
}

impl<'a> Sem<'a> {
    fn block_header(&mut self, node: &SyntaxNode) {
        let block = Block(node.clone());
        if let Some(kw) = block.keyword() {
            self.set(&kw, SemKind::Keyword);
        }
        if let Some(name) = block.name() {
            self.set(&name, SemKind::BlockName);
        }
    }

    fn module_header(&mut self, node: &SyntaxNode, is_real_module: bool) {
        let module = Module(node.clone());
        if let Some(slot) = module.slot() {
            self.set(&slot, SemKind::Keyword);
        }
        if let Some(name) = module.module_name() {
            // Real module: the name is a module type; sub-block: it's an arg.
            self.set(&name, if is_real_module { SemKind::Module } else { SemKind::EnumMember });
        }
    }

    fn walk(&mut self, node: &SyntaxNode, scope: &ScopeSchema) {
        for child in node.children() {
            match child.kind() {
                SyntaxKind::FIELD => self.field(&Field(child.clone()), scope),
                SyntaxKind::MODULE => {
                    let module = Module(child.clone());
                    let is_real = module
                        .slot()
                        .map(|s| scope.module_slots().iter().any(|ms| ms.keyword == s.text()))
                        .unwrap_or(false);
                    self.module_header(&child, is_real);
                    let inner = if is_real {
                        scope_schema(self.analyzer, &child)
                    } else {
                        ScopeSchema::Unknown
                    };
                    self.walk(&child, &inner);
                }
                SyntaxKind::BLOCK => {
                    let inner = scope_schema(self.analyzer, &child);
                    self.block_header(&child);
                    self.walk(&child, &inner);
                }
                _ => {}
            }
        }
    }

    fn field(&mut self, field: &Field, scope: &ScopeSchema) {
        if let Some(key) = field.key() {
            self.set(&key, SemKind::Field);
        }
        let ty = field
            .key()
            .and_then(|k| scope.field(k.text()))
            .map(|f| f.value_type.clone());
        for tok in field.value_tokens() {
            let kind = value_token_kind(&tok, ty.as_ref());
            self.set(&tok, kind);
        }
    }

    fn set(&mut self, tok: &SyntaxToken, kind: SemKind) {
        let span: Span = tok.text_range().into();
        self.by_start.insert(span.start, SemToken { span, kind });
    }

    fn set_if_absent(&mut self, tok: &SyntaxToken, kind: SemKind) {
        let span: Span = tok.text_range().into();
        self.by_start.entry(span.start).or_insert(SemToken { span, kind });
    }
}

/// Classify a field value token according to the field's value type.
fn value_token_kind(tok: &SyntaxToken, ty: Option<&ValueType>) -> SemKind {
    if tok.kind() == SyntaxKind::STRING {
        return SemKind::StringLit;
    }
    match ty {
        Some(ValueType::Bool)
        | Some(ValueType::Enum { .. })
        | Some(ValueType::BitFlags { .. }) => SemKind::EnumMember,
        Some(ValueType::Int)
        | Some(ValueType::UInt)
        | Some(ValueType::Real)
        | Some(ValueType::PositiveReal)
        | Some(ValueType::AngleReal)
        | Some(ValueType::Percent)
        | Some(ValueType::Duration)
        | Some(ValueType::Velocity)
        | Some(ValueType::Acceleration)
        | Some(ValueType::Color)
        | Some(ValueType::Coord2D)
        | Some(ValueType::Coord3D) => SemKind::Number,
        Some(ValueType::Reference { .. }) => SemKind::Reference,
        _ => SemKind::StringLit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<(SemKind, String)> {
        let a = Analyzer::embedded();
        let parse = a.parse(src);
        semantic_tokens(&a, &parse)
            .into_iter()
            .map(|t| {
                let s = &src[t.span.start as usize..t.span.end as usize];
                (t.kind, s.to_string())
            })
            .collect()
    }

    #[test]
    fn classifies_weapon_tokens() {
        let src = "Weapon AK47\n  PrimaryDamage = 50.0\n  DeathType = BURNED\nEnd ; done\n";
        let t = toks(src);
        let has = |k: SemKind, s: &str| t.iter().any(|(kk, ss)| *kk == k && ss == s);
        assert!(has(SemKind::Keyword, "Weapon"));
        assert!(has(SemKind::BlockName, "AK47"));
        assert!(has(SemKind::Field, "PrimaryDamage"));
        assert!(has(SemKind::Operator, "="));
        assert!(has(SemKind::Number, "50.0"));
        assert!(has(SemKind::EnumMember, "BURNED"));
        assert!(has(SemKind::Keyword, "End"));
        assert!(t.iter().any(|(k, _)| *k == SemKind::Comment));
    }

    #[test]
    fn classifies_module_type_name() {
        let src = "Object Tank\n  Body = ActiveBody Tag01\n    MaxHealth = 100\n  End\nEnd\n";
        let t = toks(src);
        assert!(t.iter().any(|(k, s)| *k == SemKind::Module && s == "ActiveBody"));
        assert!(t.iter().any(|(k, s)| *k == SemKind::Number && s == "100"));
    }

    #[test]
    fn tokens_are_sorted_and_non_overlapping() {
        let a = Analyzer::embedded();
        let src = "Weapon AK47\n  PrimaryDamage = 50.0\nEnd\n";
        let toks = semantic_tokens(&a, &a.parse(src));
        for w in toks.windows(2) {
            assert!(w[0].span.end <= w[1].span.start, "overlap: {:?}", w);
        }
    }
}
