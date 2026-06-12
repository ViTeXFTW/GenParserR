//! Tokenizer for the Generals INI format.
//!
//! Faithful to the engine's rules (see `INI.h::getSeps` / `INI.cpp`):
//! the default separators are `" \n\r\t="`, `;` starts a comment that runs to
//! end of line, and `"` delimits a quoted string. `:` and `%` are only
//! separators inside specific value parsers (color, coord, percent), so at the
//! token level they are part of a [`SyntaxKind::WORD`]; analysis splits them.

use logos::Logos;

use crate::kind::SyntaxKind;

/// Raw token classes recognized by the lexer. Mapped 1:1 to [`SyntaxKind`].
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum RawToken {
    #[regex(r"[ \t]+")]
    Whitespace,

    #[regex(r"\r\n|\n|\r")]
    Newline,

    // `;` to end of line.
    #[regex(r";[^\n\r]*")]
    Comment,

    #[token("=")]
    Equals,

    // A quoted string: opening quote, any chars except quote/newline, optional
    // closing quote (unterminated strings still lex, for error recovery).
    #[regex(r#""[^"\r\n]*""#)]
    #[regex(r#""[^"\r\n]*"#)]
    String,

    // A bare token: one or more chars that are not a separator, comment start,
    // or quote. `+`/`-`/`:`/`%`/digits/letters all fall here.
    #[regex(r#"[^ \t\r\n=;"]+"#)]
    Word,
}

impl RawToken {
    fn syntax_kind(self) -> SyntaxKind {
        match self {
            RawToken::Whitespace => SyntaxKind::WHITESPACE,
            RawToken::Newline => SyntaxKind::NEWLINE,
            RawToken::Comment => SyntaxKind::COMMENT,
            RawToken::Equals => SyntaxKind::EQUALS,
            RawToken::String => SyntaxKind::STRING,
            RawToken::Word => SyntaxKind::WORD,
        }
    }
}

/// One lexed token: its kind and its half-open byte range in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: SyntaxKind,
    pub start: usize,
    pub end: usize,
}

impl Token {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start..self.end]
    }

    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }
}

/// Tokenize `src` into a flat list of tokens covering the entire input.
///
/// The returned tokens are contiguous and gap-free: any byte that logos fails
/// to classify is emitted as a single [`SyntaxKind::ERROR_TOKEN`] so the parser
/// (and round-trip tests) see every byte exactly once.
pub fn tokenize(src: &str) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    let mut lex = RawToken::lexer(src);
    while let Some(result) = lex.next() {
        let span = lex.span();
        let kind = match result {
            Ok(tok) => tok.syntax_kind(),
            Err(()) => SyntaxKind::ERROR_TOKEN,
        };
        // Merge consecutive error tokens to keep the stream tidy.
        if kind == SyntaxKind::ERROR_TOKEN {
            if let Some(last) = out.last_mut() {
                if last.kind == SyntaxKind::ERROR_TOKEN && last.end == span.start {
                    last.end = span.end;
                    continue;
                }
            }
        }
        out.push(Token {
            kind,
            start: span.start,
            end: span.end,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<SyntaxKind> {
        tokenize(src).into_iter().map(|t| t.kind).collect()
    }

    /// The lexer must be lossless: concatenating token texts reproduces input.
    fn assert_lossless(src: &str) {
        let joined: String = tokenize(src).iter().map(|t| t.text(src)).collect();
        assert_eq!(joined, src, "lexer dropped bytes for {src:?}");
    }

    #[test]
    fn field_assignment() {
        use SyntaxKind::*;
        assert_eq!(
            kinds("PrimaryDamage = 50.0"),
            vec![WORD, WHITESPACE, EQUALS, WHITESPACE, WORD]
        );
        assert_lossless("PrimaryDamage = 50.0");
    }

    #[test]
    fn no_space_around_equals() {
        use SyntaxKind::*;
        assert_eq!(kinds("ClipSize=8"), vec![WORD, EQUALS, WORD]);
    }

    #[test]
    fn comment_runs_to_eol() {
        use SyntaxKind::*;
        assert_eq!(
            kinds("Foo = Bar ; a comment\nBaz"),
            vec![WORD, WHITESPACE, EQUALS, WHITESPACE, WORD, WHITESPACE, COMMENT, NEWLINE, WORD]
        );
        assert_lossless("Foo = Bar ; a comment\nBaz");
    }

    #[test]
    fn bitflags_keep_sign_in_word() {
        use SyntaxKind::*;
        // `+`/`-` are part of the word; analysis interprets the modifier.
        assert_eq!(
            kinds("DamageType = +ARMOR_PIERCING -FLAME"),
            vec![WORD, WHITESPACE, EQUALS, WHITESPACE, WORD, WHITESPACE, WORD]
        );
    }

    #[test]
    fn quoted_string() {
        use SyntaxKind::*;
        assert_eq!(
            kinds(r#"DisplayName = "Tank Marauder""#),
            vec![WORD, WHITESPACE, EQUALS, WHITESPACE, STRING]
        );
        assert_lossless(r#"DisplayName = "Tank Marauder""#);
    }

    #[test]
    fn unterminated_string_still_lexes() {
        use SyntaxKind::*;
        assert_eq!(
            kinds(r#"Name = "oops"#),
            vec![WORD, WHITESPACE, EQUALS, WHITESPACE, STRING]
        );
        assert_lossless(r#"Name = "oops"#);
    }

    #[test]
    fn color_and_coord_are_words_at_lex_level() {
        use SyntaxKind::*;
        // `R:255` is a single WORD here; the color parser splits on ':'.
        assert_eq!(
            kinds("Color = R:255 G:128 B:0"),
            vec![WORD, WHITESPACE, EQUALS, WHITESPACE, WORD, WHITESPACE, WORD, WHITESPACE, WORD]
        );
    }

    #[test]
    fn crlf_line_endings() {
        use SyntaxKind::*;
        assert_eq!(kinds("A\r\nB"), vec![WORD, NEWLINE, WORD]);
        assert_lossless("A\r\nB");
    }
}
