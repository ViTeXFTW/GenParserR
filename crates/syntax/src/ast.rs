//! Thin typed views over the untyped rowan CST.
//!
//! These wrappers give analysis ergonomic access to the parts it cares about
//! (a block's keyword and name, a field's key and value tokens, a module's slot
//! and name) without it having to walk raw nodes. They are cheap handles; the
//! underlying tree owns the data.

use crate::kind::SyntaxKind;
use crate::parser::{SyntaxNode, SyntaxToken};

fn node_kind(n: &SyntaxNode) -> SyntaxKind {
    n.kind()
}

fn token_kind(t: &SyntaxToken) -> SyntaxKind {
    t.kind()
}

/// Significant (non-trivia) tokens that are direct children of `node`
/// (i.e. on its header line, before any nested node).
fn header_tokens(node: &SyntaxNode) -> impl Iterator<Item = SyntaxToken> + '_ {
    node.children_with_tokens()
        .filter_map(move |el| el.into_token().filter(|t| !token_kind(t).is_trivia()))
}

fn header_value_tokens(node: &SyntaxNode) -> Vec<SyntaxToken> {
    let mut seen_key = false;
    let mut out = Vec::new();
    for element in node.children_with_tokens() {
        let Some(token) = element.into_token() else {
            break;
        };
        match token_kind(&token) {
            SyntaxKind::NEWLINE => break,
            SyntaxKind::WORD if !seen_key => seen_key = true,
            SyntaxKind::WORD | SyntaxKind::STRING if seen_key => out.push(token),
            _ => {}
        }
    }
    out
}

/// A top-level block, e.g. `Weapon AK47 ... End`.
#[derive(Debug, Clone)]
pub struct Block(pub SyntaxNode);

impl Block {
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node_kind(&node) == SyntaxKind::BLOCK).then_some(Block(node))
    }

    /// The block keyword token (`Weapon`).
    pub fn keyword(&self) -> Option<SyntaxToken> {
        header_tokens(&self.0).find(|t| token_kind(t) == SyntaxKind::WORD)
    }

    /// The block name token (`AK47`), i.e. the second header word, if present.
    pub fn name(&self) -> Option<SyntaxToken> {
        header_tokens(&self.0)
            .filter(|t| token_kind(t) == SyntaxKind::WORD)
            .nth(1)
    }

    pub fn fields(&self) -> impl Iterator<Item = Field> + '_ {
        self.0.children().filter_map(Field::cast)
    }

    pub fn modules(&self) -> impl Iterator<Item = Module> + '_ {
        self.0.children().filter_map(Module::cast)
    }
}

/// A nested module / sub-block, e.g. `Behavior = PhysicsBehavior ModuleTag_02`.
#[derive(Debug, Clone)]
pub struct Module(pub SyntaxNode);

impl Module {
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node_kind(&node) == SyntaxKind::MODULE).then_some(Module(node))
    }

    /// The slot / sub-block keyword (`Behavior`, `ConditionState`, ...).
    pub fn slot(&self) -> Option<SyntaxToken> {
        header_tokens(&self.0).find(|t| token_kind(t) == SyntaxKind::WORD)
    }

    /// The module type name following `=` (`PhysicsBehavior`). For sub-blocks
    /// written without `=` (e.g. `DefaultConditionState`) this is `None`.
    pub fn module_name(&self) -> Option<SyntaxToken> {
        self.word_after_equals(0)
    }

    /// The module tag following the module name (`ModuleTag_01`), i.e. the
    /// second word after `=`. The engine requires one per module, unique
    /// within the object.
    pub fn tag(&self) -> Option<SyntaxToken> {
        self.word_after_equals(1)
    }

    /// Header arguments after the slot keyword, with or without `=`.
    pub fn argument_tokens(&self) -> Vec<SyntaxToken> {
        header_value_tokens(&self.0)
    }

    fn word_after_equals(&self, nth: usize) -> Option<SyntaxToken> {
        let mut seen_equals = false;
        let mut skip = nth;
        for el in self.0.children_with_tokens() {
            if let Some(t) = el.as_token() {
                match token_kind(t) {
                    SyntaxKind::EQUALS => seen_equals = true,
                    // The header ends at the first newline: later direct
                    // tokens (e.g. the closing `End` line) are not arguments.
                    SyntaxKind::NEWLINE => break,
                    SyntaxKind::WORD if seen_equals => {
                        if skip == 0 {
                            return Some(t.clone());
                        }
                        skip -= 1;
                    }
                    _ => {}
                }
            } else {
                break; // entered nested nodes; header is done
            }
        }
        None
    }

    pub fn fields(&self) -> impl Iterator<Item = Field> + '_ {
        self.0.children().filter_map(Field::cast)
    }

    pub fn modules(&self) -> impl Iterator<Item = Module> + '_ {
        self.0.children().filter_map(Module::cast)
    }
}

/// A single `Key = value [value...]` line.
#[derive(Debug, Clone)]
pub struct Field(pub SyntaxNode);

impl Field {
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node_kind(&node) == SyntaxKind::FIELD).then_some(Field(node))
    }

    /// The field key token (left of `=`).
    pub fn key(&self) -> Option<SyntaxToken> {
        header_tokens(&self.0).find(|t| token_kind(t) == SyntaxKind::WORD)
    }

    /// The value tokens after the key, in order. The engine lexes `=` as just
    /// another separator (INI.cpp seps are `" \n\r\t="`), so `RemoveModule
    /// ModuleTag_01` carries a value exactly like `Key = Value` does.
    pub fn value_tokens(&self) -> Vec<SyntaxToken> {
        header_value_tokens(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{parse, FixedOpeners};

    fn openers() -> FixedOpeners {
        FixedOpeners::new(["Object", "Weapon", "Draw", "Body"])
    }

    #[test]
    fn reads_block_keyword_and_name() {
        let src = "Weapon AK47\n  PrimaryDamage = 50.0\nEnd\n";
        let root = parse(src, &openers()).syntax();
        let block = root.first_child().and_then(Block::cast).unwrap();
        assert_eq!(block.keyword().unwrap().text(), "Weapon");
        assert_eq!(block.name().unwrap().text(), "AK47");
        let field = block.fields().next().unwrap();
        assert_eq!(field.key().unwrap().text(), "PrimaryDamage");
        assert_eq!(field.value_tokens()[0].text(), "50.0");
    }

    #[test]
    fn reads_module_slot_and_name() {
        let src = "Object Tank\n  Body = ActiveBody Tag01\n    MaxHealth = 100\n  End\nEnd\n";
        let root = parse(src, &openers()).syntax();
        let object = root.first_child().and_then(Block::cast).unwrap();
        let module = object.modules().next().unwrap();
        assert_eq!(module.slot().unwrap().text(), "Body");
        assert_eq!(module.module_name().unwrap().text(), "ActiveBody");
        assert_eq!(
            module.fields().next().unwrap().key().unwrap().text(),
            "MaxHealth"
        );
    }
}
