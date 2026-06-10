//! Cross-file symbol index: which named definitions (objects, weapons, ...)
//! exist across the workspace, and where. Powers reference diagnostics,
//! reference completions, and go-to-definition.
//!
//! The server owns one [`WorkspaceIndex`], updating it per file as documents
//! change ([`WorkspaceIndex::set_file`]).

use std::collections::HashMap;

use genparser_schema::RefKind;
use genparser_syntax::ast::Block;
use genparser_syntax::{Parse, SyntaxKind};

use crate::{Analyzer, Span};

/// A definition's location within a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub file: String,
    pub span: Span,
}

/// A named definition discovered in a document.
#[derive(Debug, Clone)]
pub struct Definition {
    pub name: String,
    pub kind: RefKind,
    pub span: Span,
}

/// Workspace-wide symbol table, grouped by reference kind then name.
#[derive(Default)]
pub struct WorkspaceIndex {
    by_kind: HashMap<RefKind, HashMap<String, Vec<Location>>>,
    /// Reverse map so a file's old entries can be removed on update.
    files: HashMap<String, Vec<(RefKind, String)>>,
}

impl WorkspaceIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace all definitions contributed by `file` with `defs`.
    pub fn set_file(&mut self, file: &str, defs: Vec<Definition>) {
        self.remove_file(file);
        let mut owned = Vec::new();
        for def in defs {
            self.by_kind
                .entry(def.kind)
                .or_default()
                .entry(def.name.clone())
                .or_default()
                .push(Location {
                    file: file.to_string(),
                    span: def.span,
                });
            owned.push((def.kind, def.name));
        }
        self.files.insert(file.to_string(), owned);
    }

    /// Drop all definitions contributed by `file`.
    pub fn remove_file(&mut self, file: &str) {
        if let Some(entries) = self.files.remove(file) {
            for (kind, name) in entries {
                if let Some(names) = self.by_kind.get_mut(&kind) {
                    if let Some(locs) = names.get_mut(&name) {
                        locs.retain(|l| l.file != file);
                        if locs.is_empty() {
                            names.remove(&name);
                        }
                    }
                }
            }
        }
    }

    /// Is `name` defined for `kind` anywhere in the workspace?
    pub fn is_defined(&self, kind: RefKind, name: &str) -> bool {
        self.by_kind
            .get(&kind)
            .map(|n| n.contains_key(name))
            .unwrap_or(false)
    }

    /// All definition locations for `name` of `kind`.
    pub fn locations(&self, kind: RefKind, name: &str) -> &[Location] {
        self.by_kind
            .get(&kind)
            .and_then(|n| n.get(name))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// All known names for a kind (for reference completion).
    pub fn names(&self, kind: RefKind) -> impl Iterator<Item = &str> {
        self.by_kind
            .get(&kind)
            .into_iter()
            .flat_map(|n| n.keys().map(|s| s.as_str()))
    }
}

/// Collect the named definitions a parsed document contributes (top-level
/// blocks whose keyword `defines` a reference kind).
pub fn definitions_in(analyzer: &Analyzer, parse: &Parse, _file: &str) -> Vec<Definition> {
    let mut out = Vec::new();
    let root = parse.syntax();
    for node in root.children().filter(|n| n.kind() == SyntaxKind::BLOCK) {
        let block = Block(node.clone());
        let Some(keyword) = block.keyword() else {
            continue;
        };
        let Some(schema_block) = analyzer.block(keyword.text()) else {
            continue;
        };
        let Some(kind) = schema_block.defines else {
            continue;
        };
        if let Some(name) = block.name() {
            out.push(Definition {
                name: name.text().to_string(),
                kind,
                span: name.text_range().into(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_named_definitions() {
        let a = Analyzer::embedded();
        let src = "Weapon AK47\nEnd\nObject Tank\nEnd\n";
        let parse = a.parse(src);
        let defs = definitions_in(&a, &parse, "f.ini");
        let mut idx = WorkspaceIndex::new();
        idx.set_file("f.ini", defs);
        assert!(idx.is_defined(RefKind::Weapon, "AK47"));
        assert!(idx.is_defined(RefKind::Object, "Tank"));
        assert!(!idx.is_defined(RefKind::Weapon, "Nonexistent"));
    }

    #[test]
    fn updating_a_file_replaces_its_symbols() {
        let a = Analyzer::embedded();
        let mut idx = WorkspaceIndex::new();
        idx.set_file("f.ini", definitions_in(&a, &a.parse("Weapon Old\nEnd\n"), "f.ini"));
        idx.set_file("f.ini", definitions_in(&a, &a.parse("Weapon New\nEnd\n"), "f.ini"));
        assert!(!idx.is_defined(RefKind::Weapon, "Old"));
        assert!(idx.is_defined(RefKind::Weapon, "New"));
    }
}
