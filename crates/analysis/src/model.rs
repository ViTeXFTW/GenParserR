//! Bridges the syntax tree to the schema: given a scope node (a `BLOCK` or
//! `MODULE`), determine which schema entity it is and look up fields / slots.

use genparser_schema::{BlockType, Field, ModuleSlot, ModuleType};
use genparser_syntax::ast::{Block, Module};
use genparser_syntax::{SyntaxKind, SyntaxNode};

use crate::Analyzer;

/// What schema entity a scope node corresponds to.
pub enum ScopeSchema<'a> {
    /// A top-level block with a known schema.
    Block(&'a BlockType),
    /// A nested module with a known schema.
    Module(&'a ModuleType),
    /// A scope we have no schema for (unknown block/module, or an anonymous
    /// sub-block such as `DefaultConditionState`). Validation is lenient here:
    /// fields are not flagged as unknown.
    Unknown,
}

impl<'a> ScopeSchema<'a> {
    /// The declared fields of this scope, or empty when unknown.
    pub fn fields(&self) -> &'a [Field] {
        match self {
            ScopeSchema::Block(b) => &b.fields,
            ScopeSchema::Module(m) => &m.fields,
            ScopeSchema::Unknown => &[],
        }
    }

    /// Look up a field by name.
    pub fn field(&self, name: &str) -> Option<&'a Field> {
        self.fields().iter().find(|f| f.name == name)
    }

    /// Module slots exposed by this scope (only blocks have them in the schema).
    pub fn module_slots(&self) -> &'a [ModuleSlot] {
        match self {
            ScopeSchema::Block(b) => &b.module_slots,
            _ => &[],
        }
    }

    /// Whether we have a field schema at all (drives unknown-field diagnostics).
    pub fn has_field_schema(&self) -> bool {
        !matches!(self, ScopeSchema::Unknown) && !self.fields().is_empty()
    }

    /// A human label for diagnostics, e.g. `block Weapon` / `module ActiveBody`.
    pub fn label(&self) -> String {
        match self {
            ScopeSchema::Block(b) => format!("block `{}`", b.name),
            ScopeSchema::Module(m) => format!("module `{}`", m.name),
            ScopeSchema::Unknown => "this block".to_string(),
        }
    }
}

/// Resolve the schema for a `BLOCK` or `MODULE` node.
pub fn scope_schema<'a>(analyzer: &'a Analyzer, node: &SyntaxNode) -> ScopeSchema<'a> {
    match node.kind() {
        SyntaxKind::BLOCK => {
            let kw = Block(node.clone())
                .keyword()
                .map(|t| t.text().to_string());
            match kw.and_then(|k| analyzer.block(&k)) {
                Some(b) => ScopeSchema::Block(b),
                None => ScopeSchema::Unknown,
            }
        }
        SyntaxKind::MODULE => {
            let name = Module(node.clone())
                .module_name()
                .map(|t| t.text().to_string());
            match name.and_then(|n| analyzer.module(&n)) {
                Some(m) => ScopeSchema::Module(m),
                None => ScopeSchema::Unknown,
            }
        }
        _ => ScopeSchema::Unknown,
    }
}

/// The chain of scope schemas enclosing `node`, innermost first. Used by
/// completion to know what is valid at the cursor.
pub fn enclosing_scopes<'a>(analyzer: &'a Analyzer, node: &SyntaxNode) -> Vec<ScopeSchema<'a>> {
    let mut out = Vec::new();
    let mut cur = Some(node.clone());
    while let Some(n) = cur {
        if matches!(n.kind(), SyntaxKind::BLOCK | SyntaxKind::MODULE) {
            out.push(scope_schema(analyzer, &n));
        }
        cur = n.parent();
    }
    out
}
