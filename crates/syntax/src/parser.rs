//! Error-recovering parser that turns the token stream into a lossless rowan CST.
//!
//! ## Grammar (line-oriented, faithful to the engine)
//!
//! The engine processes one line at a time against the field-parse table of the
//! current scope. A line either sets a field (single line) or opens a nested,
//! `End`-terminated scope (a top-level block, a module like `Behavior = ...`, or
//! a sub-block like `ConditionState = ...`). Whether a given line opens a scope
//! is *schema-determined*, so this parser delegates that decision to an
//! [`OpenerOracle`]; `End` always closes the nearest open scope.
//!
//! The tree is lossless: every byte of input appears exactly once, so the CST
//! round-trips back to the original text (see tests). Unterminated scopes are
//! left open to end-of-file (analysis reports the missing `End`).

use rowan::GreenNodeBuilder;

use crate::kind::{IniLang, SyntaxKind};
use crate::lexer::{tokenize, Token};

pub type SyntaxNode = rowan::SyntaxNode<IniLang>;
pub type SyntaxToken = rowan::SyntaxToken<IniLang>;
pub type SyntaxElement = rowan::SyntaxElement<IniLang>;

/// Decides whether a line opens a new `End`-terminated scope. Implemented by
/// the analysis layer from the schema (block keywords, module slots, sub-block
/// fields); tests use [`FixedOpeners`].
pub trait OpenerOracle {
    /// Decide whether a line opens a nested `End`-terminated scope.
    ///
    /// `enclosing` is the head keyword of the innermost currently-open scope
    /// (`None` at file scope), so the decision can be context-sensitive — the
    /// engine opens child scopes based on the *current* scope's parse table.
    /// `head` is the first bare token on the line; `has_equals` is whether the
    /// line contains an `=`.
    fn opens_scope(&self, enclosing: Option<&str>, head: &str, has_equals: bool) -> bool;
}

/// An oracle backed by explicit keyword sets. Block keywords open scopes
/// anywhere; this is sufficient for tests and as a degraded-mode default.
pub struct FixedOpeners {
    pub keywords: std::collections::HashSet<String>,
}

impl FixedOpeners {
    pub fn new<I, S>(keywords: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            keywords: keywords.into_iter().map(Into::into).collect(),
        }
    }
}

impl OpenerOracle for FixedOpeners {
    fn opens_scope(&self, _enclosing: Option<&str>, head: &str, _has_equals: bool) -> bool {
        self.keywords.contains(head)
    }
}

/// A no-op oracle: nothing opens a scope. Useful for pure lexical highlighting.
pub struct NoOpeners;
impl OpenerOracle for NoOpeners {
    fn opens_scope(&self, _enclosing: Option<&str>, _head: &str, _has_equals: bool) -> bool {
        false
    }
}

/// The result of a parse: the syntax tree plus recovery diagnostics.
pub struct Parse {
    green: rowan::GreenNode,
    pub errors: Vec<SyntaxError>,
}

/// A structural (not semantic) parse error, with the byte range it concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub message: String,
    pub start: usize,
    pub end: usize,
}

impl Parse {
    /// The typed root node of the tree.
    pub fn syntax(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }

    pub fn green(&self) -> &rowan::GreenNode {
        &self.green
    }
}

/// Parse `src` using the engine's default lexer rules and the given oracle.
pub fn parse(src: &str, oracle: &dyn OpenerOracle) -> Parse {
    let tokens = tokenize(src);
    Parser {
        src,
        tokens: &tokens,
        builder: GreenNodeBuilder::new(),
        errors: Vec::new(),
        open_scopes: Vec::new(),
    }
    .run(oracle)
}

/// A single logical line: the inclusive token-index range `[start, end)` and the
/// classification needed to drive scoping.
struct Line {
    start: usize,
    end: usize,
    /// First bare WORD on the line, if any (the "head").
    head: Option<(usize, String)>,
    has_equals: bool,
    /// True if the head is exactly `End`.
    is_end: bool,
    /// True if the line has no significant (non-trivia) tokens.
    blank: bool,
}

struct Parser<'a> {
    src: &'a str,
    tokens: &'a [Token],
    builder: GreenNodeBuilder<'static>,
    errors: Vec<SyntaxError>,
    /// For each currently-open scope: its start byte (for error spans) and the
    /// head keyword that opened it (for context-aware opener decisions).
    open_scopes: Vec<(usize, String)>,
}

impl<'a> Parser<'a> {
    fn run(mut self, oracle: &dyn OpenerOracle) -> Parse {
        self.builder.start_node(SyntaxKind::ROOT.into());

        let lines = self.split_lines();
        for line in &lines {
            if line.blank {
                self.emit_tokens(line.start, line.end);
                continue;
            }
            if line.is_end {
                self.close_scope(line);
            } else if self.open_scopes.is_empty() {
                // At file scope the engine reads every line's first token as a
                // block type, so any non-`End` line opens a block (an unknown
                // keyword is then reported as an unknown block, not a stray field).
                self.open_scope(line);
            } else if line
                .head
                .as_ref()
                .map(|(_, h)| {
                    let enclosing = self.open_scopes.last().map(|(_, k)| k.as_str());
                    oracle.opens_scope(enclosing, h, line.has_equals)
                })
                .unwrap_or(false)
            {
                self.open_scope(line);
            } else {
                self.field(line);
            }
        }

        // Close any scopes left open at EOF and report them.
        while let Some((start, _head)) = self.open_scopes.pop() {
            self.errors.push(SyntaxError {
                message: "block is not terminated with `End`".into(),
                start,
                end: self.src.len(),
            });
            self.builder.finish_node();
        }

        self.builder.finish_node(); // ROOT
        Parse {
            green: self.builder.finish(),
            errors: self.errors,
        }
    }

    /// Group tokens into logical lines (terminated by NEWLINE or EOF).
    fn split_lines(&self) -> Vec<Line> {
        let mut lines = Vec::new();
        let mut i = 0;
        while i < self.tokens.len() {
            let start = i;
            let mut head: Option<(usize, String)> = None;
            let mut has_equals = false;
            let mut blank = true;
            while i < self.tokens.len() {
                let tok = self.tokens[i];
                match tok.kind {
                    SyntaxKind::NEWLINE => {
                        i += 1;
                        break;
                    }
                    SyntaxKind::WHITESPACE | SyntaxKind::COMMENT => {}
                    SyntaxKind::EQUALS => {
                        has_equals = true;
                        blank = false;
                    }
                    SyntaxKind::WORD => {
                        blank = false;
                        if head.is_none() {
                            head = Some((i, tok.text(self.src).to_string()));
                        }
                    }
                    _ => blank = false,
                }
                i += 1;
            }
            // The engine matches the End token case-insensitively (INI.cpp uses
            // stricmp), so `end`/`END`/`End` all terminate a block.
            let is_end = matches!(&head, Some((_, h)) if h.eq_ignore_ascii_case("End"));
            lines.push(Line {
                start,
                end: i,
                head,
                has_equals,
                is_end,
                blank,
            });
        }
        lines
    }

    fn open_scope(&mut self, line: &Line) {
        // Top-level scopes are BLOCKs; nested scopes (modules / sub-blocks) MODULEs.
        let kind = if self.open_scopes.is_empty() {
            SyntaxKind::BLOCK
        } else {
            SyntaxKind::MODULE
        };
        let start_byte = self.tokens[line.start].start;
        let head = line
            .head
            .as_ref()
            .map(|(_, h)| h.clone())
            .unwrap_or_default();
        self.builder.start_node(kind.into());
        self.open_scopes.push((start_byte, head));
        self.emit_tokens(line.start, line.end); // header line
    }

    fn close_scope(&mut self, line: &Line) {
        if self.open_scopes.is_empty() {
            // Stray `End` with no open scope: wrap it as an error node.
            self.builder.start_node(SyntaxKind::ERROR.into());
            self.emit_tokens(line.start, line.end);
            self.builder.finish_node();
            self.errors.push(SyntaxError {
                message: "`End` without a matching block".into(),
                start: self.tokens[line.start].start,
                end: self.tokens[line.end - 1].end,
            });
            return;
        }
        self.emit_tokens(line.start, line.end); // the End line belongs to the scope
        self.open_scopes.pop();
        self.builder.finish_node();
    }

    fn field(&mut self, line: &Line) {
        self.builder.start_node(SyntaxKind::FIELD.into());
        self.emit_tokens(line.start, line.end);
        self.builder.finish_node();
    }

    fn emit_tokens(&mut self, start: usize, end: usize) {
        for tok in &self.tokens[start..end] {
            self.builder
                .token(tok.kind.into(), tok.text(self.src));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opener_set() -> FixedOpeners {
        FixedOpeners::new([
            "Object",
            "Weapon",
            "Armor",
            "Draw",
            "Body",
            "Behavior",
            "ConditionState",
            "DefaultConditionState",
        ])
    }

    /// Concatenating all leaf tokens must reproduce the source exactly.
    fn assert_round_trips(src: &str) {
        let parse = parse(src, &opener_set());
        let text: String = parse.syntax().text().to_string();
        assert_eq!(text, src);
    }

    fn child_kinds(src: &str) -> Vec<SyntaxKind> {
        parse(src, &opener_set())
            .syntax()
            .children()
            .map(|n| n.kind())
            .collect()
    }

    #[test]
    fn simple_block_with_fields() {
        let src = "Weapon AK47\n  PrimaryDamage = 50.0\n  ClipSize = 30\nEnd\n";
        assert_round_trips(src);
        assert_eq!(child_kinds(src), vec![SyntaxKind::BLOCK]);
        let block = parse(src, &opener_set())
            .syntax()
            .first_child()
            .unwrap();
        let fields = block
            .children()
            .filter(|n| n.kind() == SyntaxKind::FIELD)
            .count();
        assert_eq!(fields, 2);
    }

    #[test]
    fn nested_module_scope() {
        let src = "\
Object Tank
  Draw = W3DModelDraw ModuleTag_01
    DefaultConditionState
      Model = TANK
    End
  End
  Body = ActiveBody ModuleTag_02
    MaxHealth = 100
  End
End
";
        assert_round_trips(src);
        let root = parse(src, &opener_set()).syntax();
        let object = root.first_child().unwrap();
        // Object should contain two MODULE children (Draw and Body).
        let modules = object
            .children()
            .filter(|n| n.kind() == SyntaxKind::MODULE)
            .count();
        assert_eq!(modules, 2);
    }

    #[test]
    fn unterminated_block_is_reported_but_lossless() {
        let src = "Weapon AK47\n  PrimaryDamage = 50.0\n";
        assert_round_trips(src);
        let parse = parse(src, &opener_set());
        assert_eq!(parse.errors.len(), 1);
        assert!(parse.errors[0].message.contains("not terminated"));
    }

    #[test]
    fn stray_end_is_reported() {
        let src = "End\n";
        assert_round_trips(src);
        let parse = parse(src, &opener_set());
        assert_eq!(parse.errors.len(), 1);
        assert!(parse.errors[0].message.contains("without a matching"));
    }

    #[test]
    fn lowercase_end_closes_block() {
        // The engine matches `End` case-insensitively; `end` must close the block.
        // Use an opener set without `Armor` so the inner `Armor =` is a plain
        // leaf field and this test isolates the case-insensitive `End` behavior.
        let openers = FixedOpeners::new(["Weapon"]);
        let src = "Armor test\n  Armor = ARMOR_PIERCING 100%\nend\n";
        assert_eq!(parse(src, &openers).syntax().text().to_string(), src);
        let parse = parse(src, &openers);
        assert!(parse.errors.is_empty(), "{:?}", parse.errors);
        assert_eq!(
            parse.syntax().children().map(|n| n.kind()).collect::<Vec<_>>(),
            vec![SyntaxKind::BLOCK]
        );
    }

    #[test]
    fn comments_and_blank_lines_preserved() {
        let src = "; header comment\n\nWeapon AK47\nEnd\n";
        assert_round_trips(src);
    }
}
