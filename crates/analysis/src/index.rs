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

/// A definition name's entry: display casing plus all its locations.
struct NameEntry {
    /// The name as first written (for completion display).
    display: String,
    locations: Vec<Location>,
}

/// Workspace-wide symbol table, grouped by reference kind then name.
///
/// Name lookup is **case-insensitive**, mirroring the engine: shipped game
/// data references `MappedImage SAPathFinder1` as `SAPathfinder1` and the
/// game resolves it.
#[derive(Default)]
pub struct WorkspaceIndex {
    by_kind: HashMap<RefKind, HashMap<String, NameEntry>>,
    /// Reverse map (lowercased names) so a file's entries can be removed.
    files: HashMap<String, Vec<(RefKind, String)>>,
    /// Bumped whenever the *name set* changes (not mere span shifts), so
    /// consumers (the per-block diagnostics cache) can invalidate cheaply.
    generation: u64,
}

impl WorkspaceIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// A counter that changes whenever reference resolution could change.
    /// Re-indexing a file with the same definition names does **not** bump it,
    /// so a keystroke that doesn't touch a block header keeps caches warm.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Replace all definitions contributed by `file` with `defs`.
    pub fn set_file(&mut self, file: &str, defs: Vec<Definition>) {
        let names: Vec<(RefKind, String)> = defs
            .iter()
            .map(|d| (d.kind, d.name.to_ascii_lowercase()))
            .collect();
        let changed = match self.files.get(file) {
            Some(old) => *old != names,
            None => !names.is_empty(),
        };
        if changed {
            self.generation += 1;
        }
        self.remove_entries(file);
        for def in defs {
            let entry = self
                .by_kind
                .entry(def.kind)
                .or_default()
                .entry(def.name.to_ascii_lowercase())
                .or_insert_with(|| NameEntry {
                    display: def.name.clone(),
                    locations: Vec::new(),
                });
            entry.locations.push(Location {
                file: file.to_string(),
                span: def.span,
            });
        }
        self.files.insert(file.to_string(), names);
    }

    /// Drop all definitions contributed by `file`.
    pub fn remove_file(&mut self, file: &str) {
        if self.files.get(file).is_some_and(|v| !v.is_empty()) {
            self.generation += 1;
        }
        self.remove_entries(file);
    }

    fn remove_entries(&mut self, file: &str) {
        if let Some(entries) = self.files.remove(file) {
            for (kind, lower) in entries {
                if let Some(names) = self.by_kind.get_mut(&kind) {
                    if let Some(entry) = names.get_mut(&lower) {
                        entry.locations.retain(|l| l.file != file);
                        if entry.locations.is_empty() {
                            names.remove(&lower);
                        }
                    }
                }
            }
        }
    }

    /// Is `name` defined for `kind` anywhere in the workspace?
    /// Case-insensitive, like the engine's own name lookups.
    pub fn is_defined(&self, kind: RefKind, name: &str) -> bool {
        self.by_kind
            .get(&kind)
            .map(|n| n.contains_key(&name.to_ascii_lowercase()))
            .unwrap_or(false)
    }

    /// All definition locations for `name` of `kind` (case-insensitive).
    pub fn locations(&self, kind: RefKind, name: &str) -> &[Location] {
        self.by_kind
            .get(&kind)
            .and_then(|n| n.get(&name.to_ascii_lowercase()))
            .map(|e| e.locations.as_slice())
            .unwrap_or(&[])
    }

    /// All known names for a kind (for reference completion), in their
    /// originally-written casing.
    pub fn names(&self, kind: RefKind) -> impl Iterator<Item = &str> {
        self.by_kind
            .get(&kind)
            .into_iter()
            .flat_map(|n| n.values().map(|e| e.display.as_str()))
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
        // The engine resolves names case-insensitively (shipped data relies
        // on it: `MappedImage SAPathFinder1` vs `ButtonImage = SAPathfinder1`).
        assert!(idx.is_defined(RefKind::Weapon, "ak47"));
        assert!(idx.is_defined(RefKind::Object, "TANK"));
        assert!(!idx.is_defined(RefKind::Weapon, "Nonexistent"));
        // Completion shows the original casing.
        assert!(idx.names(RefKind::Weapon).any(|n| n == "AK47"));
    }

    #[test]
    fn generation_tracks_name_changes_only() {
        let a = Analyzer::embedded();
        let mut idx = WorkspaceIndex::new();
        let g0 = idx.generation();
        idx.set_file(
            "f.ini",
            definitions_in(&a, &a.parse("Weapon AK47\nEnd\n"), "f.ini"),
        );
        let g1 = idx.generation();
        assert_ne!(g0, g1, "new definitions bump the generation");

        // Same names at shifted spans (e.g. a comment typed above): no bump.
        idx.set_file(
            "f.ini",
            definitions_in(&a, &a.parse("; c\nWeapon AK47\nEnd\n"), "f.ini"),
        );
        assert_eq!(idx.generation(), g1);

        // Renaming a definition bumps.
        idx.set_file(
            "f.ini",
            definitions_in(&a, &a.parse("Weapon M16\nEnd\n"), "f.ini"),
        );
        assert_ne!(idx.generation(), g1);

        // A file with no definitions, set repeatedly: no bumps after removal.
        let g2 = idx.generation();
        idx.set_file("empty.ini", vec![]);
        assert_eq!(idx.generation(), g2);
        idx.remove_file("empty.ini");
        assert_eq!(idx.generation(), g2);
    }

    #[test]
    fn updating_a_file_replaces_its_symbols() {
        let a = Analyzer::embedded();
        let mut idx = WorkspaceIndex::new();
        idx.set_file(
            "f.ini",
            definitions_in(&a, &a.parse("Weapon Old\nEnd\n"), "f.ini"),
        );
        idx.set_file(
            "f.ini",
            definitions_in(&a, &a.parse("Weapon New\nEnd\n"), "f.ini"),
        );
        assert!(!idx.is_defined(RefKind::Weapon, "Old"));
        assert!(idx.is_defined(RefKind::Weapon, "New"));
    }
}
