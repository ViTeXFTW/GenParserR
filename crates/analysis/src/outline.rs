//! Document structure: the block/module outline (`textDocument/documentSymbol`)
//! and foldable scope spans (`textDocument/foldingRange`). Purely structural —
//! derived from the CST alone, no schema needed.

use genparser_syntax::ast::{Block, Module};
use genparser_syntax::{Parse, SyntaxKind, SyntaxNode};

use crate::Span;

/// What a symbol is, mapped to LSP `SymbolKind` by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymKind {
    /// A top-level block (`Object AmericaTankCrusader`).
    Block,
    /// A `Behavior = … ModuleTag_x`-style module.
    Module,
    /// A structural sub-block (`WeaponSet`, `ConditionState`, …).
    SubBlock,
}

/// One outline entry: a block, module, or sub-block scope.
#[derive(Debug, Clone)]
pub struct DocSymbol {
    /// Display name: the block's name (`AmericaTankCrusader`), the module's
    /// type name (`AutoHealBehavior`), or the sub-block keyword (`WeaponSet`).
    pub name: String,
    /// Secondary text: the block keyword (`Object`) or the module tag.
    pub detail: Option<String>,
    pub kind: SymKind,
    /// The whole scope, header line through its `End`.
    pub span: Span,
    /// The token to highlight when the symbol is selected.
    pub selection: Span,
    pub children: Vec<DocSymbol>,
}

/// The nested block/module outline of a document.
pub fn document_symbols(parse: &Parse) -> Vec<DocSymbol> {
    parse
        .syntax()
        .children()
        .filter(|n| n.kind() == SyntaxKind::BLOCK)
        .filter_map(|n| block_symbol(&n))
        .collect()
}

fn block_symbol(node: &SyntaxNode) -> Option<DocSymbol> {
    let block = Block(node.clone());
    let keyword = block.keyword()?;
    let name_tok = block.name();
    let (name, detail, selection) = match &name_tok {
        Some(n) => (
            n.text().to_string(),
            Some(keyword.text().to_string()),
            Span::from(n.text_range()),
        ),
        // Singleton blocks (GameData, AudioSettings, …) have no name.
        None => (keyword.text().to_string(), None, Span::from(keyword.text_range())),
    };
    Some(DocSymbol {
        name,
        detail,
        kind: SymKind::Block,
        span: node.text_range().into(),
        selection,
        children: child_symbols(node),
    })
}

fn child_symbols(node: &SyntaxNode) -> Vec<DocSymbol> {
    node.children()
        .filter(|n| n.kind() == SyntaxKind::MODULE)
        .filter_map(|n| module_symbol(&n))
        .collect()
}

fn module_symbol(node: &SyntaxNode) -> Option<DocSymbol> {
    let module = Module(node.clone());
    let slot = module.slot()?;
    // `Behavior = AutoHealBehavior ModuleTag_x` shows as "AutoHealBehavior"
    // with the tag as detail; sub-blocks (`WeaponSet`, `ConditionState =
    // DAMAGED`) show under their keyword with the argument as detail.
    let (name, detail, kind, selection) = match module.module_name() {
        Some(n) if module.tag().is_some() => (
            n.text().to_string(),
            module.tag().map(|t| t.text().to_string()),
            SymKind::Module,
            Span::from(n.text_range()),
        ),
        Some(n) => (
            slot.text().to_string(),
            Some(n.text().to_string()),
            SymKind::SubBlock,
            Span::from(slot.text_range()),
        ),
        None => (
            slot.text().to_string(),
            None,
            SymKind::SubBlock,
            Span::from(slot.text_range()),
        ),
    };
    Some(DocSymbol {
        name,
        detail,
        kind,
        span: node.text_range().into(),
        selection,
        children: child_symbols(node),
    })
}

/// Every foldable scope span (blocks, modules, sub-blocks), outermost first.
/// The server folds from the header line to the line of the closing `End`.
pub fn folding_ranges(parse: &Parse) -> Vec<Span> {
    let mut out = Vec::new();
    collect_folds(&parse.syntax(), &mut out);
    out
}

fn collect_folds(node: &SyntaxNode, out: &mut Vec<Span>) {
    for child in node.children() {
        if matches!(child.kind(), SyntaxKind::BLOCK | SyntaxKind::MODULE) {
            out.push(child.text_range().into());
            collect_folds(&child, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Analyzer;

    #[test]
    fn outlines_blocks_and_modules() {
        let a = Analyzer::embedded();
        let src = "Object Tank\n  Behavior = AutoHealBehavior ModuleTag_01\n    HealingAmount = 5\n  End\n  WeaponSet\n    Conditions = None\n  End\nEnd\nWeapon AK47\n  PrimaryDamage = 10.0\nEnd\n";
        let parse = a.parse(src);
        let syms = document_symbols(&parse);
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].name, "Tank");
        assert_eq!(syms[0].detail.as_deref(), Some("Object"));
        assert_eq!(syms[0].kind, SymKind::Block);
        let kids = &syms[0].children;
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].name, "AutoHealBehavior");
        assert_eq!(kids[0].detail.as_deref(), Some("ModuleTag_01"));
        assert_eq!(kids[0].kind, SymKind::Module);
        assert_eq!(kids[1].name, "WeaponSet");
        assert_eq!(kids[1].kind, SymKind::SubBlock);
        assert_eq!(syms[1].name, "AK47");
    }

    #[test]
    fn folds_nested_scopes() {
        let a = Analyzer::embedded();
        let src = "Object Tank\n  Behavior = AutoHealBehavior ModuleTag_01\n    HealingAmount = 5\n  End\nEnd\n";
        let folds = folding_ranges(&a.parse(src));
        assert_eq!(folds.len(), 2, "the Object and the Behavior fold");
        assert!(folds[0].start < folds[1].start && folds[1].end <= folds[0].end);
    }
}
