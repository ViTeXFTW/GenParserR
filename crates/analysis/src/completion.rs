//! Context-aware completions.
//!
//! Resolves what is valid at a byte offset and returns candidate items:
//! * file scope -> top-level block keywords;
//! * inside a block/module, at the start of a line -> field names + module slots;
//! * after `=` -> enum/bitflag members, `Yes`/`No`, module names, or (with the
//!   workspace index) names of the referenced definition kind.

use genparser_schema::ValueType;
use genparser_syntax::ast::{Field, Module};
use genparser_syntax::{Parse, SyntaxKind, SyntaxNode};

use crate::model::scope_schema;
use crate::{Analyzer, WorkspaceIndex};

/// The role of a completion item, mapped to LSP `CompletionItemKind` by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Block,
    Field,
    Module,
    EnumMember,
    Value,
    Reference,
}

/// A single completion candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub label: String,
    pub kind: CompletionKind,
    pub detail: Option<String>,
}

/// Compute completions at byte `offset`.
pub fn complete(
    analyzer: &Analyzer,
    parse: &Parse,
    offset: u32,
    index: Option<&WorkspaceIndex>,
) -> Vec<Completion> {
    let root = parse.syntax();
    let ctx = classify_position(analyzer, &root, offset);
    match ctx {
        PosContext::TopLevel => analyzer
            .block_names()
            .map(|n| Completion {
                label: n.to_string(),
                kind: CompletionKind::Block,
                detail: Some("block".into()),
            })
            .collect(),
        PosContext::FieldKey(scope_node) => field_key_completions(analyzer, &scope_node),
        PosContext::FieldValue {
            scope_node,
            key,
            value_index,
        } => field_value_completions(analyzer, &scope_node, &key, value_index, index),
        PosContext::ModuleName => module_name_completions(analyzer),
    }
}

enum PosContext {
    TopLevel,
    /// Completing a field/slot keyword inside this scope node.
    FieldKey(SyntaxNode),
    /// Completing the value of `key` inside this scope node; `value_index` is
    /// how many value tokens already precede the cursor (the position within
    /// a token-list value).
    FieldValue {
        scope_node: SyntaxNode,
        key: String,
        value_index: usize,
    },
    /// Completing a module type name after a slot `=`.
    ModuleName,
}

fn classify_position(analyzer: &Analyzer, root: &SyntaxNode, offset: u32) -> PosContext {
    let off = rowan::TextSize::from(offset.min(root.text_range().end().into()));
    let element = root.covering_element(rowan::TextRange::empty(off));
    let node = match &element {
        rowan::NodeOrToken::Node(n) => n.clone(),
        rowan::NodeOrToken::Token(t) => t.parent().unwrap_or_else(|| root.clone()),
    };

    // Are we on a FIELD line? (most common while typing `Key = value`)
    if let Some(field_node) = ancestor_of_kind(&node, SyntaxKind::FIELD) {
        let scope_node = enclosing_scope(&field_node);
        if after_equals(&field_node, offset) {
            let field = Field(field_node.clone());
            let key = field.key().map(|k| k.text().to_string()).unwrap_or_default();
            let value_index = field
                .value_tokens()
                .iter()
                .filter(|t| u32::from(t.text_range().end()) <= offset)
                .count();
            return PosContext::FieldValue {
                scope_node: scope_node.unwrap_or_else(|| root.clone()),
                key,
                value_index,
            };
        }
        return match scope_node {
            Some(s) => PosContext::FieldKey(s),
            None => PosContext::TopLevel,
        };
    }

    // On a MODULE header line, after `=`, completing the module type name.
    if let Some(module_node) = ancestor_of_kind(&node, SyntaxKind::MODULE) {
        // Only treat as module-name context if the cursor is on the header line
        // (before any nested field/scope) and after `=`, and the slot is a real
        // module slot of the parent block.
        if on_header_line(&module_node, offset) && after_equals(&module_node, offset) {
            let parent = enclosing_scope(&module_node).map(|p| scope_schema(analyzer, &p));
            let is_real = Module(module_node.clone())
                .slot()
                .map(|s| {
                    parent
                        .as_ref()
                        .map(|p| p.module_slots().iter().any(|ms| ms.keyword == s.text()))
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if is_real {
                return PosContext::ModuleName;
            }
        }
        // Otherwise we're inside the module body -> completing a field key.
        return PosContext::FieldKey(module_node);
    }

    // Inside a BLOCK body (blank line) -> field key completion for that block.
    if let Some(block_node) = ancestor_of_kind(&node, SyntaxKind::BLOCK) {
        return PosContext::FieldKey(block_node);
    }

    // Not inside anything -> file scope.
    PosContext::TopLevel
}

fn field_key_completions(analyzer: &Analyzer, scope_node: &SyntaxNode) -> Vec<Completion> {
    let scope = scope_schema(analyzer, scope_node);
    let mut out: Vec<Completion> = scope
        .fields()
        .iter()
        .map(|f| Completion {
            label: f.name.clone(),
            kind: CompletionKind::Field,
            detail: Some(type_label(&f.value_type)),
        })
        .collect();
    for slot in scope.module_slots() {
        out.push(Completion {
            label: slot.keyword.clone(),
            kind: CompletionKind::Field,
            detail: Some("module slot".into()),
        });
    }
    out
}

fn field_value_completions(
    analyzer: &Analyzer,
    scope_node: &SyntaxNode,
    key: &str,
    value_index: usize,
    index: Option<&WorkspaceIndex>,
) -> Vec<Completion> {
    let scope = scope_schema(analyzer, scope_node);
    let Some(field) = scope.field(key) else {
        return Vec::new();
    };
    completions_for_type(analyzer, &field.value_type, value_index, index)
}

fn completions_for_type(
    analyzer: &Analyzer,
    ty: &ValueType,
    value_index: usize,
    index: Option<&WorkspaceIndex>,
) -> Vec<Completion> {
    match ty {
        // Token lists complete the element at the cursor's position.
        ValueType::TokenList { tokens } => tokens
            .get(value_index)
            .map(|elem| completions_for_type(analyzer, elem, 0, index))
            .unwrap_or_default(),
        ValueType::Bool => ["Yes", "No"]
            .iter()
            .map(|v| Completion {
                label: v.to_string(),
                kind: CompletionKind::Value,
                detail: None,
            })
            .collect(),
        ValueType::Enum { value_set } | ValueType::BitFlags { value_set } => analyzer
            .value_set(value_set)
            .map(|set| {
                set.members
                    .iter()
                    .map(|m| Completion {
                        label: m.name.clone(),
                        kind: CompletionKind::EnumMember,
                        detail: Some(value_set.clone()),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        ValueType::Reference { ref_kind } | ValueType::ReferenceList { ref_kind } => {
            let mut out: Vec<Completion> = index
                .map(|idx| {
                    idx.names(*ref_kind)
                        .map(|n| Completion {
                            label: n.to_string(),
                            kind: CompletionKind::Reference,
                            detail: Some(format!("{ref_kind:?}")),
                        })
                        .collect()
                })
                .unwrap_or_default();
            // Engine-synthesized names (e.g. Upgrade_Veterancy_*) are valid
            // targets that appear in no file.
            out.extend(analyzer.builtin_names(*ref_kind).map(|n| Completion {
                label: n.to_string(),
                kind: CompletionKind::Reference,
                detail: Some(format!("{ref_kind:?} (engine builtin)")),
            }));
            out
        }
        _ => Vec::new(),
    }
}

fn module_name_completions(analyzer: &Analyzer) -> Vec<Completion> {
    analyzer
        .schema()
        .modules
        .iter()
        .map(|m| Completion {
            label: m.name.clone(),
            kind: CompletionKind::Module,
            detail: Some("module".into()),
        })
        .collect()
}

// --- position helpers ---

fn ancestor_of_kind(node: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxNode> {
    let mut cur = Some(node.clone());
    while let Some(n) = cur {
        if n.kind() == kind {
            return Some(n);
        }
        cur = n.parent();
    }
    None
}

/// The nearest BLOCK/MODULE ancestor of `node` (its enclosing scope).
fn enclosing_scope(node: &SyntaxNode) -> Option<SyntaxNode> {
    node.ancestors()
        .skip(1)
        .find(|n| matches!(n.kind(), SyntaxKind::BLOCK | SyntaxKind::MODULE))
}

/// True if there is an `=` token before `offset` within `node`'s own tokens.
fn after_equals(node: &SyntaxNode, offset: u32) -> bool {
    for el in node.children_with_tokens() {
        if let Some(t) = el.as_token() {
            if t.kind() == SyntaxKind::EQUALS && u32::from(t.text_range().end()) <= offset {
                return true;
            }
        } else {
            break; // reached nested nodes; header is over
        }
    }
    false
}

/// True if `offset` lies on the header line of a scope (before its first nested
/// child node).
fn on_header_line(node: &SyntaxNode, offset: u32) -> bool {
    let first_child_start = node
        .children()
        .next()
        .map(|c| u32::from(c.text_range().start()));
    match first_child_start {
        Some(start) => offset <= start,
        None => true,
    }
}

fn type_label(ty: &ValueType) -> String {
    match ty {
        ValueType::Bool => "Yes/No".into(),
        ValueType::Enum { value_set } => format!("enum {value_set}"),
        ValueType::BitFlags { value_set } => format!("flags {value_set}"),
        ValueType::Reference { ref_kind } => format!("ref {ref_kind:?}"),
        other => format!("{other:?}")
            .split(['{', ' '])
            .next()
            .unwrap_or("")
            .to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(src: &str, offset: u32) -> Vec<String> {
        let a = Analyzer::embedded();
        complete(&a, &a.parse(src), offset, None)
            .into_iter()
            .map(|c| c.label)
            .collect()
    }

    #[test]
    fn top_level_suggests_block_keywords() {
        let out = labels("", 0);
        assert!(out.contains(&"Object".to_string()));
        assert!(out.contains(&"Weapon".to_string()));
    }

    #[test]
    fn inside_weapon_suggests_fields() {
        // Cursor on the indented blank line inside the block.
        let src = "Weapon AK47\n  \nEnd\n";
        let offset = "Weapon AK47\n  ".len() as u32;
        let out = labels(src, offset);
        assert!(out.contains(&"PrimaryDamage".to_string()), "{out:?}");
        assert!(out.contains(&"ClipSize".to_string()));
    }

    #[test]
    fn bool_value_suggests_yes_no() {
        let src = "Weapon AK47\n  ScaleWeaponSpeed = \nEnd\n";
        let offset = "Weapon AK47\n  ScaleWeaponSpeed = ".len() as u32;
        let out = labels(src, offset);
        assert!(out.contains(&"Yes".to_string()) && out.contains(&"No".to_string()), "{out:?}");
    }

    #[test]
    fn enum_value_suggests_members() {
        let src = "Weapon AK47\n  DeathType = \nEnd\n";
        let offset = "Weapon AK47\n  DeathType = ".len() as u32;
        let out = labels(src, offset);
        assert!(out.contains(&"BURNED".to_string()), "{out:?}");
        assert!(out.contains(&"NORMAL".to_string()));
    }

    #[test]
    fn module_slot_value_suggests_modules() {
        let src = "Object Tank\n  Body = \n  End\nEnd\n";
        let offset = "Object Tank\n  Body = ".len() as u32;
        let out = labels(src, offset);
        assert!(out.contains(&"ActiveBody".to_string()), "{out:?}");
    }
}
