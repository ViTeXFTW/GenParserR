//! Lexer + lossless CST for the C&C Generals: Zero Hour INI format.
//!
//! See [`lexer`] for tokenization (faithful to the engine's `;`-comment and
//! whitespace/`=` separator rules) and [`parser`] for the rowan-based concrete
//! syntax tree with error recovery.

pub mod ast;
pub mod kind;
pub mod lexer;
pub mod parser;

pub use kind::{IniLang, SyntaxKind};
pub use parser::{parse, OpenerOracle, Parse, SyntaxError, SyntaxNode, SyntaxToken};
