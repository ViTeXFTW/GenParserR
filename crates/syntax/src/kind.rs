//! `SyntaxKind`: the tag for every token (leaf) and node (composite) in the CST.
//!
//! Token kinds are produced by the [`crate::lexer`]; node kinds are produced by
//! the parser. The enum is `#[repr(u16)]` so it can back a rowan syntax tree.

/// Every kind of syntax element. Leaf (token) kinds come first, then composite
/// (node) kinds, then the sentinel `EOF`/`ERROR` helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
#[allow(non_camel_case_types)]
pub enum SyntaxKind {
    // --- tokens (leaves) ---
    /// Runs of spaces and tabs (trivia).
    WHITESPACE = 0,
    /// A line ending: `\n`, `\r\n`, or `\r`.
    NEWLINE,
    /// `; ...` to end of line (trivia).
    COMMENT,
    /// `=` field assignment / separator.
    EQUALS,
    /// A `"..."` quoted string (engine `parseQuotedAsciiString`).
    STRING,
    /// A bare token: any run of non-separator characters. Numbers, identifiers,
    /// asset names, enum members and `+`/`-`-prefixed bitflags are all WORDs;
    /// their finer meaning is resolved against the schema during analysis.
    WORD,
    /// Any character that could not be lexed (should be rare).
    ERROR_TOKEN,

    // --- nodes (composites) ---
    /// The whole document.
    ROOT,
    /// A top-level block: `Keyword [Name] <body> End`.
    BLOCK,
    /// A nested module: `Slot = ModuleName [Tag] <body> End`.
    MODULE,
    /// A `Key = value [value...]` line.
    FIELD,
    /// The value portion of a field or module header (one or more value tokens).
    VALUE,
    /// A node wrapping content the parser could not classify (error recovery).
    ERROR,

    /// Sentinel; not a real kind.
    EOF,
}

impl SyntaxKind {
    /// Whitespace, newlines and comments — preserved in the CST but ignored by
    /// the grammar.
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            SyntaxKind::WHITESPACE | SyntaxKind::NEWLINE | SyntaxKind::COMMENT
        )
    }
}

impl From<SyntaxKind> for rowan::SyntaxKind {
    fn from(kind: SyntaxKind) -> Self {
        rowan::SyntaxKind(kind as u16)
    }
}

/// The rowan language definition tying `rowan::SyntaxKind` back to [`SyntaxKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IniLang {}

impl rowan::Language for IniLang {
    type Kind = SyntaxKind;

    fn kind_from_raw(raw: rowan::SyntaxKind) -> Self::Kind {
        assert!(raw.0 <= SyntaxKind::EOF as u16);
        // SAFETY: SyntaxKind is repr(u16), contiguous from 0..=EOF, and we
        // asserted the value is in range.
        unsafe { std::mem::transmute::<u16, SyntaxKind>(raw.0) }
    }

    fn kind_to_raw(kind: Self::Kind) -> rowan::SyntaxKind {
        kind.into()
    }
}
