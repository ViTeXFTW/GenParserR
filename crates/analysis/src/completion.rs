//! Context-aware completions.
//!
//! Resolves what is valid at a byte offset and returns candidate items:
//! * file scope -> top-level block keywords;
//! * inside a block/module, at the start of a line -> field names + module slots;
//! * after `=` -> enum/bitflag members, `Yes`/`No`, module names, or (with the
//!   workspace index) names of the referenced definition kind.

use zerosyntax_schema::ValueType;
use zerosyntax_syntax::ast::{Block, Field, Module};
use zerosyntax_syntax::{Parse, SyntaxKind, SyntaxNode};

use crate::model::{
    is_model_asset_type, is_model_member_type, model_member_ini_name, models_in_scope, scope_schema,
};
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
    /// Optional LSP snippet string. When the client supports snippets, the
    /// server uses this as `insertText` with `InsertTextFormat::SNIPPET`
    /// instead of the plain `label`. `$0` marks the final cursor position;
    /// `${N:placeholder}` marks tab-stops. `None` means plain-label insertion.
    pub insert: Option<String>,
}

/// Compute completions at byte `offset`.
/// `file` is the current document URI, used for string-key lookup from co-located `.str` files.
pub fn complete(
    analyzer: &Analyzer,
    parse: &Parse,
    offset: u32,
    index: Option<&WorkspaceIndex>,
    file: Option<&str>,
) -> Vec<Completion> {
    let root = parse.syntax();
    let ctx = classify_position(analyzer, &root, offset);
    match ctx {
        PosContext::TopLevel => top_level_completions(analyzer),
        PosContext::FieldKey(scope_node) => field_key_completions(analyzer, &scope_node),
        PosContext::FieldValue {
            scope_node,
            key,
            value_index,
        } => field_value_completions(analyzer, &scope_node, &key, value_index, index, file),
        PosContext::ModuleName { slot_accepts } => module_name_completions(analyzer, &slot_accepts),
        PosContext::SubBlockArg { argument_type } => {
            completions_for_type(analyzer, &argument_type, 0, index)
        }
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
    /// Completing a module type name after a slot `=`. Carries the slot's
    /// accepted interfaces so completions can be filtered to valid modules only.
    ModuleName {
        slot_accepts: Vec<String>,
    },
    /// Completing the argument of a sub-block header.
    SubBlockArg {
        argument_type: zerosyntax_schema::ValueType,
    },
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
            let key = field
                .key()
                .map(|k| k.text().to_string())
                .unwrap_or_default();
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
            let slot = Module(module_node.clone()).slot();
            let slot_accepts = slot.as_ref().and_then(|s| {
                parent.as_ref().and_then(|p| {
                    p.module_slots()
                        .iter()
                        .find(|ms| ms.keyword == s.text())
                        .map(|ms| ms.accepts.clone())
                })
            });
            if let Some(accepts) = slot_accepts {
                return PosContext::ModuleName {
                    slot_accepts: accepts,
                };
            }
            // Check for a sub-block with an argument_type (e.g. ConditionState = <flags>).
            if let Some(arg_type) = slot
                .as_ref()
                .and_then(|s| sub_block_arg_type(parent.as_ref(), s.text()))
            {
                return PosContext::SubBlockArg {
                    argument_type: arg_type,
                };
            }
        } else if on_header_line(&module_node, offset) {
            let parent = enclosing_scope(&module_node).map(|p| scope_schema(analyzer, &p));
            let slot = Module(module_node.clone()).slot();
            if let Some(arg_type) = slot.as_ref().and_then(|s| {
                (offset >= u32::from(s.text_range().end()))
                    .then(|| sub_block_arg_type(parent.as_ref(), s.text()))
                    .flatten()
            }) {
                return PosContext::SubBlockArg {
                    argument_type: arg_type,
                };
            }
        }
        // Otherwise we're inside the module body -> completing a field key.
        return PosContext::FieldKey(module_node);
    }

    // Inside a BLOCK body -> field key completion for that block.
    // Special case: if the cursor is on the block's keyword token at file scope
    // with no `=` before it, the user is typing a block keyword -> offer block
    // names (TopLevel) so the popup appears while typing.
    if let Some(block_node) = ancestor_of_kind(&node, SyntaxKind::BLOCK) {
        let is_top_level = block_node
            .parent()
            .map(|p| p.kind() == SyntaxKind::ROOT)
            .unwrap_or(false);
        if is_top_level && on_header_line(&block_node, offset) && !after_equals(&block_node, offset)
        {
            if let Some(kw) = Block(block_node.clone()).keyword() {
                if offset <= u32::from(kw.text_range().end()) {
                    return PosContext::TopLevel;
                }
            }
        }
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
            insert: None,
        })
        .collect();
    for slot in scope.module_slots() {
        // Snippet: `Slot = $0` so cursor lands at the module-name position.
        let insert = Some(format!("{} = $0", slot.keyword));
        out.push(Completion {
            label: slot.keyword.clone(),
            kind: CompletionKind::Field,
            detail: Some("module slot".into()),
            insert,
        });
    }
    for sub in scope.sub_blocks() {
        // Snippet: include `= ${1:NONE}` argument placeholder when the sub-block takes one.
        let insert = if sub.argument_type.is_some() {
            let arg = sub
                .argument_type
                .as_ref()
                .map(argument_placeholder)
                .unwrap_or("NONE".into());
            if space_separated_sub_block_arg(&sub.keyword) {
                Some(format!("{} ${{1:{arg}}}\n\t$0\nEnd", sub.keyword))
            } else {
                Some(format!("{} = ${{1:{arg}}}\n\t$0\nEnd", sub.keyword))
            }
        } else {
            Some(format!("{}\n\t$0\nEnd", sub.keyword))
        };
        out.push(Completion {
            label: sub.keyword.clone(),
            kind: CompletionKind::Block,
            detail: Some("sub-block".into()),
            insert,
        });
    }
    out
}

fn sub_block_arg_type(
    scope: Option<&crate::model::ScopeSchema<'_>>,
    keyword: &str,
) -> Option<zerosyntax_schema::ValueType> {
    scope?
        .sub_blocks()
        .iter()
        .find(|sb| sb.keyword == keyword)
        .and_then(|sb| sb.argument_type.clone())
}

fn space_separated_sub_block_arg(keyword: &str) -> bool {
    matches!(keyword, "SideInfo" | "SkirmishBuildList" | "Structure")
}

fn argument_placeholder(ty: &ValueType) -> String {
    match ty {
        ValueType::Enum { value_set } if value_set == "ai_side" => "America".into(),
        ValueType::Reference { ref_kind } => format!("{ref_kind:?}"),
        _ => "NONE".into(),
    }
}

fn field_value_completions(
    analyzer: &Analyzer,
    scope_node: &SyntaxNode,
    key: &str,
    value_index: usize,
    index: Option<&WorkspaceIndex>,
    file: Option<&str>,
) -> Vec<Completion> {
    // RemoveModule / ReplaceModule: suggest module tags from the origin object.
    if key.eq_ignore_ascii_case("RemoveModule") || key.eq_ignore_ascii_case("ReplaceModule") {
        if let Some(idx) = index {
            let obj_name = Block(scope_node.clone())
                .name()
                .map(|n| n.text().to_string())
                .unwrap_or_default();
            if !obj_name.is_empty() {
                let tags: Vec<Completion> = idx
                    .module_tags_for_object(&obj_name)
                    .map(|tag| Completion {
                        label: tag.to_string(),
                        kind: CompletionKind::Reference,
                        detail: Some("module tag".into()),
                        insert: None,
                    })
                    .collect();
                if !tags.is_empty() {
                    return tags;
                }
            }
        }
    }
    // DisplayName: add string table keys from the companion .str file when available.
    let mut base = {
        let scope = scope_schema(analyzer, scope_node);
        if let Some(f) = scope.field(key) {
            if let Some(asset_completions) =
                model_asset_completions(analyzer, scope_node, &f.value_type, value_index, index)
            {
                asset_completions
            } else {
                completions_for_type(analyzer, &f.value_type, value_index, index)
            }
        } else {
            Vec::new()
        }
    };
    if key.eq_ignore_ascii_case("DisplayName") {
        if let (Some(idx), Some(f)) = (index, file) {
            base.extend(idx.string_keys_for_ini(f).map(|k| Completion {
                label: k.to_string(),
                kind: CompletionKind::Value,
                detail: Some("string key".into()),
                insert: None,
            }));
        }
    }
    base
}

fn model_asset_completions(
    analyzer: &Analyzer,
    scope_node: &SyntaxNode,
    ty: &ValueType,
    value_index: usize,
    index: Option<&WorkspaceIndex>,
) -> Option<Vec<Completion>> {
    let index = index?;
    if !index.has_model_assets() {
        return None;
    }
    let ty = token_value_type(ty, value_index);
    if is_model_asset_type(ty) {
        return Some(
            index
                .model_names()
                .map(|name| Completion {
                    label: name.to_string(),
                    kind: CompletionKind::Reference,
                    detail: Some("W3D model".into()),
                    insert: None,
                })
                .collect(),
        );
    }
    if !is_model_member_type(ty) {
        return None;
    }
    let mut seen = std::collections::HashSet::new();
    let out = models_in_scope(analyzer, scope_node)
        .into_iter()
        .flat_map(|model| {
            index
                .model_members(&model)
                .map(|member| model_member_ini_name(member).to_string())
                .collect::<Vec<_>>()
        })
        .filter(|member| seen.insert(member.to_ascii_lowercase()))
        .map(|member| Completion {
            label: member,
            kind: CompletionKind::Reference,
            detail: Some("W3D model member".into()),
            insert: None,
        })
        .collect();
    Some(out)
}

fn token_value_type(ty: &ValueType, value_index: usize) -> &ValueType {
    match ty {
        ValueType::TokenList { tokens } => tokens.get(value_index).unwrap_or(ty),
        _ => ty,
    }
}

/// Build a single-token snippet placeholder for a value type, used when
/// generating a full-sequence snippet for TokenList or structured types.
/// `n` is the tab-stop index (1-based).
fn type_snippet_placeholder(ty: &ValueType, n: usize) -> String {
    match ty {
        ValueType::Bool => format!("${{{n}:Yes}}"),
        ValueType::Int | ValueType::UInt => format!("${{{n}:0}}"),
        ValueType::Real
        | ValueType::PositiveReal
        | ValueType::AngleReal
        | ValueType::Velocity
        | ValueType::Acceleration => format!("${{{n}:0}}"),
        ValueType::Percent => format!("${{{n}:100%}}"),
        ValueType::Duration => format!("${{{n}:1000}}"),
        ValueType::Enum { .. } | ValueType::BitFlags { .. } => format!("${{{n}:NONE}}"),
        ValueType::Reference { ref_kind } | ValueType::ReferenceList { ref_kind } => {
            format!("${{{n}:{ref_kind:?}}}")
        }
        ValueType::AsciiString | ValueType::AsciiStringList | ValueType::QuotedString => {
            format!("${{{n}:Value}}")
        }
        ValueType::W3dModel => format!("${{{n}:Model}}"),
        ValueType::W3dModelMember => format!("${{{n}:Bone}}"),
        _ => format!("${{{n}:?}}"),
    }
}

fn completions_for_type(
    analyzer: &Analyzer,
    ty: &ValueType,
    value_index: usize,
    index: Option<&WorkspaceIndex>,
) -> Vec<Completion> {
    match ty {
        // Token lists: at position 0 offer a full-sequence snippet plus per-token completions.
        ValueType::TokenList { tokens } => {
            let mut out = tokens
                .get(value_index)
                .map(|elem| completions_for_type(analyzer, elem, 0, index))
                .unwrap_or_default();
            // At the first token, also inject a full-sequence snippet.
            if value_index == 0 && tokens.len() > 1 {
                let snippet: String = tokens
                    .iter()
                    .enumerate()
                    .map(|(i, t)| type_snippet_placeholder(t, i + 1))
                    .collect::<Vec<_>>()
                    .join(" ");
                out.insert(
                    0,
                    Completion {
                        label: "<full sequence>".into(),
                        kind: CompletionKind::Value,
                        detail: Some(format!("{} tokens", tokens.len())),
                        insert: Some(snippet),
                    },
                );
            }
            out
        }
        // Structured positional types: offer a single snippet.
        ValueType::Color => vec![Completion {
            label: "R: G: B:".into(),
            kind: CompletionKind::Value,
            detail: Some("color".into()),
            insert: Some("R:${1:255} G:${2:255} B:${3:255}".into()),
        }],
        ValueType::Coord2D => vec![Completion {
            label: "X: Y:".into(),
            kind: CompletionKind::Value,
            detail: Some("2D coordinate".into()),
            insert: Some("X:${1:0} Y:${2:0}".into()),
        }],
        ValueType::Coord3D => vec![Completion {
            label: "X: Y: Z:".into(),
            kind: CompletionKind::Value,
            detail: Some("3D coordinate".into()),
            insert: Some("X:${1:0} Y:${2:0} Z:${3:0}".into()),
        }],
        ValueType::Bool => ["Yes", "No"]
            .iter()
            .map(|v| Completion {
                label: v.to_string(),
                kind: CompletionKind::Value,
                detail: None,
                insert: None,
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
                        insert: None,
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
                            insert: None,
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
                insert: None,
            }));
            out
        }
        ValueType::W3dModel | ValueType::W3dModelMember => Vec::new(),
        _ => Vec::new(),
    }
}

fn top_level_completions(analyzer: &Analyzer) -> Vec<Completion> {
    analyzer
        .schema()
        .blocks
        .iter()
        .map(|b| {
            let insert = if !b.terminated {
                // Single-line directive — no End needed.
                None
            } else if b.named {
                Some(format!("{} ${{1:Name}}\n\t$0\nEnd", b.name))
            } else {
                Some(format!("{}\n\t$0\nEnd", b.name))
            };
            Completion {
                label: b.name.clone(),
                kind: CompletionKind::Block,
                detail: Some("block".into()),
                insert,
            }
        })
        .collect()
}

fn module_name_completions(analyzer: &Analyzer, slot_accepts: &[String]) -> Vec<Completion> {
    analyzer
        .schema()
        .modules
        .iter()
        .filter(|m| {
            slot_accepts.is_empty() || m.interfaces.iter().any(|i| slot_accepts.contains(i))
        })
        .map(|m| {
            // Snippet: module name + placeholder tag + indented body + End.
            // Also satisfies missing-module-tag in one accept.
            let insert = Some(format!("{} ${{1:ModuleTag_01}}\n\t$0\nEnd", m.name));
            Completion {
                label: m.name.clone(),
                kind: CompletionKind::Module,
                detail: Some("module".into()),
                insert,
            }
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

/// True if `offset` lies on the header line of a scope.
///
/// The header ends at the first NEWLINE token among the node's direct
/// children_with_tokens. This correctly handles module nodes with empty bodies:
/// a cursor on the line after the header is NOT on the header, even when the
/// body has no child nodes yet.
fn on_header_line(node: &SyntaxNode, offset: u32) -> bool {
    for el in node.children_with_tokens() {
        if let Some(t) = el.as_token() {
            if t.kind() == SyntaxKind::NEWLINE {
                return offset <= u32::from(t.text_range().start());
            }
        } else {
            // First child node encountered before any NEWLINE; past the header.
            break;
        }
    }
    // No NEWLINE found: single-line node — entire node is header.
    true
}

fn type_label(ty: &ValueType) -> String {
    match ty {
        ValueType::Bool => "Yes/No".into(),
        ValueType::Enum { value_set } => format!("enum {value_set}"),
        ValueType::BitFlags { value_set } => format!("flags {value_set}"),
        ValueType::Reference { ref_kind } => format!("ref {ref_kind:?}"),
        ValueType::W3dModel => "w3d model".into(),
        ValueType::W3dModelMember => "w3d model member".into(),
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
        complete(&a, &a.parse(src), offset, None, None)
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
        assert!(
            out.contains(&"Yes".to_string()) && out.contains(&"No".to_string()),
            "{out:?}"
        );
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

    #[test]
    fn model_asset_completions_use_index() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        index.set_file_models(
            "models/Good.w3d",
            vec![crate::index::ModelAsset {
                name: "Good".into(),
                members: vec!["Cargo01".into(), "Tire01".into()],
            }],
        );
        let src = "\
Object Tank
  Draw = W3DTruckDraw ModuleTag_01
    DefaultConditionState
      Model = 
      HideSubObject = 
    End
  End
End
";
        let model_offset = src.find("Model = ").unwrap() + "Model = ".len();
        let out: Vec<_> = complete(&a, &a.parse(src), model_offset as u32, Some(&index), None)
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert!(out.contains(&"Good".to_string()), "{out:?}");

        let src = src.replace("Model = ", "Model = Good");
        let bone_offset = src.find("HideSubObject = ").unwrap() + "HideSubObject = ".len();
        let out: Vec<_> = complete(&a, &a.parse(&src), bone_offset as u32, Some(&index), None)
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert!(out.contains(&"Cargo".to_string()), "{out:?}");
        assert!(out.contains(&"Tire".to_string()), "{out:?}");
    }

    #[test]
    fn weapon_bone_completions_use_token_positions() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        index.set_file_models(
            "models/Good.w3d",
            vec![crate::index::ModelAsset {
                name: "Good".into(),
                members: vec!["Muzzle01".into(), "Muzzle02".into()],
            }],
        );
        let src = "\
Object Tank
  Draw = W3DTruckDraw ModuleTag_01
    DefaultConditionState
      Model = Good
      WeaponFireFXBone = 
    End
  End
End
";
        let slot_offset = src.find("WeaponFireFXBone = ").unwrap() + "WeaponFireFXBone = ".len();
        let out: Vec<_> = complete(&a, &a.parse(src), slot_offset as u32, Some(&index), None)
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert!(out.contains(&"PRIMARY".to_string()), "{out:?}");
        assert!(!out.contains(&"Muzzle".to_string()), "{out:?}");

        let src = src.replace("WeaponFireFXBone = ", "WeaponFireFXBone = PRIMARY ");
        let bone_offset = src.find("PRIMARY ").unwrap() + "PRIMARY ".len();
        let out: Vec<_> = complete(&a, &a.parse(&src), bone_offset as u32, Some(&index), None)
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert!(out.contains(&"Muzzle".to_string()), "{out:?}");
        assert_eq!(
            out.iter()
                .filter(|label| label.as_str() == "Muzzle")
                .count(),
            1
        );
        assert!(!out.contains(&"PRIMARY".to_string()), "{out:?}");
    }
}
