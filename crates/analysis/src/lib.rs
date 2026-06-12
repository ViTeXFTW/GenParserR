//! Semantic analysis for Generals INI documents.
//!
//! The [`Analyzer`] owns the engine [`Schema`] and exposes the IDE-agnostic
//! operations the language server needs: parsing (with a schema-derived
//! [`OpenerOracle`]), [`diagnostics`], [`completion`], and [`semantic`] tokens.
//! All positions are byte offsets into the source; the server maps them to
//! LSP line/character positions.

use std::collections::{HashMap, HashSet};

use genparser_schema::{BlockType, ModuleType, RefKind, Schema, ValueSet};
use genparser_syntax::{parse, Edit, OpenerOracle, Parse, Strategy};

pub mod completion;
pub mod diagnostics;
pub mod index;
pub mod model;
pub mod nav;
pub mod semantic;

pub use diagnostics::{Diagnostic, Severity};
pub use index::WorkspaceIndex;

/// A half-open byte range `[start, end)` into the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: u32, end: u32) -> Self {
        Span { start, end }
    }
}

impl From<rowan::TextRange> for Span {
    fn from(r: rowan::TextRange) -> Self {
        Span {
            start: r.start().into(),
            end: r.end().into(),
        }
    }
}

/// The analyzer: a schema plus the lookup tables and opener oracle derived from
/// it. Cheap to share; build once and reuse across documents.
pub struct Analyzer {
    schema: Schema,
    block_by_name: HashMap<String, usize>,
    module_by_name: HashMap<String, usize>,
    value_set_by_id: HashMap<String, usize>,
    /// Engine-synthesized definitions ((kind, lowercased name)) that resolve
    /// without appearing in any file.
    builtins: std::collections::HashSet<(RefKind, String)>,
    openers: SchemaOpeners,
}

impl Analyzer {
    /// Build an analyzer from a schema (typically [`genparser_schema::embedded`]).
    pub fn new(schema: Schema) -> Self {
        let block_by_name = schema
            .blocks
            .iter()
            .enumerate()
            .map(|(i, b)| (b.name.clone(), i))
            .collect();
        let module_by_name = schema
            .modules
            .iter()
            .enumerate()
            .map(|(i, m)| (m.name.clone(), i))
            .collect();
        let value_set_by_id = schema
            .value_sets
            .iter()
            .enumerate()
            .map(|(i, v)| (v.id.clone(), i))
            .collect();
        let builtins = schema
            .builtins
            .iter()
            .map(|b| (b.ref_kind, b.name.to_ascii_lowercase()))
            .collect();
        let openers = SchemaOpeners::from_schema(&schema);
        Analyzer {
            schema,
            block_by_name,
            module_by_name,
            value_set_by_id,
            builtins,
            openers,
        }
    }

    /// Build an analyzer from the embedded schema.
    pub fn embedded() -> Self {
        Self::new(genparser_schema::embedded())
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Parse `src` using the schema-derived opener oracle.
    pub fn parse(&self, src: &str) -> Parse {
        parse(src, &self.openers)
    }

    /// Incrementally reparse after an edit (see [`genparser_syntax::reparse`]):
    /// splices the reparsed region into `old` when possible, falling back to a
    /// full parse. Always identical to `self.parse(new_text)`.
    pub fn reparse(
        &self,
        old: &Parse,
        old_text: &str,
        new_text: &str,
        edit: Edit,
    ) -> (Parse, Strategy) {
        genparser_syntax::reparse(old, old_text, new_text, edit, &self.openers)
    }

    pub fn block(&self, name: &str) -> Option<&BlockType> {
        self.block_by_name
            .get(name)
            .map(|&i| &self.schema.blocks[i])
    }

    pub fn module(&self, name: &str) -> Option<&ModuleType> {
        self.module_by_name
            .get(name)
            .map(|&i| &self.schema.modules[i])
    }

    pub fn value_set(&self, id: &str) -> Option<&ValueSet> {
        self.value_set_by_id
            .get(id)
            .map(|&i| &self.schema.value_sets[i])
    }

    /// All top-level block keywords (for completion at file scope).
    pub fn block_names(&self) -> impl Iterator<Item = &str> {
        self.schema.blocks.iter().map(|b| b.name.as_str())
    }

    /// Is `name` an engine-synthesized definition of `kind` (case-insensitive)?
    pub fn is_builtin(&self, kind: RefKind, name: &str) -> bool {
        self.builtins.contains(&(kind, name.to_ascii_lowercase()))
    }

    /// Engine-synthesized definition names of `kind` (for completion).
    pub fn builtin_names(&self, kind: RefKind) -> impl Iterator<Item = &str> {
        self.schema
            .builtins
            .iter()
            .filter(move |b| b.ref_kind == kind)
            .map(|b| b.name.as_str())
    }
}

/// An [`OpenerOracle`] backed by the schema. Scope-opening is *context-aware*,
/// mirroring the engine: a line opens a nested `End`-terminated scope only when
/// its head keyword is a valid child of the current scope — either a module slot
/// declared by the enclosing block, or one of a curated set of sub-block
/// keywords the engine parses recursively (condition states, armor/weapon sets,
/// etc.). At file scope the parser opens a block for any line itself, so the
/// oracle is only consulted for nested lines.
///
/// This context-sensitivity is what prevents a field/value whose first token
/// happens to equal a block keyword (e.g. `Armor = TankArmor` inside an
/// `ArmorSet`, or `Animation foo.bar` inside a `ConditionState`) from being
/// mistaken for a new block.
pub struct SchemaOpeners {
    /// Scope head keyword -> the set of child keywords that open nested scopes
    /// inside it: a block's module slots and declared sub-blocks, and each
    /// sub-block's own nested sub-blocks (keyed by the sub-block keyword).
    scope_children: HashMap<String, HashSet<String>>,
    /// Curated sub-block keywords valid inside any scope.
    subblocks: HashSet<String>,
    /// Block keywords that are single-line directives, not `End`-terminated
    /// (`terminated: false` in the schema).
    inline_blocks: HashSet<String>,
}

/// Sub-block keywords the engine parses as nested `End`-terminated scopes via
/// custom parse functions (so they aren't visible as module slots in the
/// schema). Curated and easily extended.
/// Keep this list minimal: a curated keyword opens a scope inside *any*
/// enclosing scope, so a field with the same name elsewhere is misparsed as a
/// block (the `Turret = <bone>` vs `Turret`-scope collision). Prefer
/// schema-declared `sub_blocks` — on blocks (context-keyed by block keyword)
/// or on modules (keyed under the hosting slot keyword, e.g. `Behavior`).
const CURATED_SUBBLOCKS: &[&str] = &[
    "ConditionState",
    "DefaultConditionState",
    "TransitionState",
    "AnimationState",
    "DefaultAnimationState",
    "IdleAnimationState",
    "ArmorSet",
    "WeaponSet",
    "AttackContactPoint",
    "InheritableModule",
    "OverrideableByLikeKind",
    // RadiusDecalTemplate scopes inside Behavior modules (deployment updates).
    "AttackAreaDecal",
    "TargetingReticleDecal",
    "DeliveryDecal",
    "GridDecalTemplate",
];

impl SchemaOpeners {
    pub fn from_schema(schema: &Schema) -> Self {
        let mut scope_children: HashMap<String, HashSet<String>> = HashMap::new();

        // Sub-block keywords can repeat across parents (e.g. `ImagePart` under
        // both `ControlBarScheme` and `AnimatingPart`); children sets merge.
        fn add_subblocks(
            map: &mut HashMap<String, HashSet<String>>,
            parent: &str,
            subs: &[genparser_schema::SubBlock],
        ) {
            for sub in subs {
                map.entry(parent.to_string())
                    .or_default()
                    .insert(sub.keyword.clone());
                add_subblocks(map, &sub.keyword, &sub.sub_blocks);
            }
        }

        for block in &schema.blocks {
            let entry = scope_children.entry(block.name.clone()).or_default();
            entry.extend(block.module_slots.iter().map(|s| s.keyword.clone()));
            add_subblocks(&mut scope_children, &block.name, &block.sub_blocks);
        }
        // Module sub-blocks (e.g. AIUpdate's `Turret`) open inside module
        // scopes, where the oracle's enclosing head is the hosting *slot*
        // keyword (`Behavior = AIUpdateInterface ...` opens a scope headed
        // "Behavior") — so key them under every declared slot keyword.
        let slot_keywords: HashSet<String> = schema
            .blocks
            .iter()
            .flat_map(|b| b.module_slots.iter().map(|s| s.keyword.clone()))
            .collect();
        for module in &schema.modules {
            for slot in &slot_keywords {
                add_subblocks(&mut scope_children, slot, &module.sub_blocks);
            }
        }

        let subblocks = CURATED_SUBBLOCKS.iter().map(|s| s.to_string()).collect();
        let inline_blocks = schema
            .blocks
            .iter()
            .filter(|b| !b.terminated)
            .map(|b| b.name.clone())
            .collect();
        SchemaOpeners {
            scope_children,
            subblocks,
            inline_blocks,
        }
    }
}

impl OpenerOracle for SchemaOpeners {
    fn opens_scope(&self, enclosing: Option<&str>, head: &str, _has_equals: bool) -> bool {
        match enclosing {
            // File scope is handled by the parser (it opens a block for every
            // line), so this is only a defensive fallback.
            None => false,
            // Inside a scope, a child opens only if it is a curated sub-block or
            // a module slot / declared sub-block of the enclosing scope.
            Some(scope_head) => {
                self.subblocks.contains(head)
                    || self
                        .scope_children
                        .get(scope_head)
                        .is_some_and(|children| children.contains(head))
            }
        }
    }

    fn opens_at_file_scope(&self, head: &str) -> bool {
        !self.inline_blocks.contains(head)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_from_embedded_schema() {
        let a = Analyzer::embedded();
        assert!(a.block("Object").is_some());
        assert!(a.block("Weapon").is_some());
        assert!(a.module("ActiveBody").is_some());
        // Context-aware: `Behavior` opens only inside `Object`; `ConditionState`
        // opens inside any scope (curated sub-block); a leaf field never opens;
        // and a block keyword used as a field inside a sub-block stays a leaf.
        assert!(a.openers.opens_scope(Some("Object"), "Behavior", true));
        assert!(a
            .openers
            .opens_scope(Some("W3DModelDraw"), "ConditionState", true));
        assert!(!a.openers.opens_scope(Some("Object"), "PrimaryDamage", true));
        // `Armor`/`Animation` are block keywords but must NOT open when they are
        // fields nested inside another scope (the bug this guards against).
        assert!(!a.openers.opens_scope(Some("ArmorSet"), "Armor", true));
        assert!(!a
            .openers
            .opens_scope(Some("DefaultConditionState"), "Animation", false));
    }
}
