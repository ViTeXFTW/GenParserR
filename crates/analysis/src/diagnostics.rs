//! Diagnostics: validates a parsed document against the schema (and, when
//! available, the cross-file index).
//!
//! Two layers, per the project's "stricter / helpful" stance:
//! * engine-faithful errors — unknown block, unknown field, bad value type,
//!   bad enum/bitflag member, unterminated block;
//! * stricter warnings/hints — unknown module, unresolved cross-file reference.

use genparser_schema::{RefKind, ValueType};
use genparser_syntax::ast::{Block, Field, Module};
use genparser_syntax::{Parse, SyntaxKind, SyntaxNode, SyntaxToken};

use crate::model::{scope_schema, ScopeSchema};
use crate::{Analyzer, Span, WorkspaceIndex};

/// Severity of a diagnostic, mapped to LSP severities by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Hint,
}

/// A single diagnostic over a byte span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub span: Span,
    pub severity: Severity,
    /// Stable machine code (e.g. `unknown-field`) for client filtering.
    pub code: &'static str,
    pub message: String,
}

/// Run all diagnostics over `parse`. `index` enables cross-file reference checks
/// (pass `None` to skip them, e.g. for single-file analysis).
pub fn diagnose(
    analyzer: &Analyzer,
    parse: &Parse,
    index: Option<&WorkspaceIndex>,
) -> Vec<Diagnostic> {
    let mut ctx = Ctx {
        analyzer,
        index,
        out: Vec::new(),
    };

    // Structural errors from the parser (unterminated blocks, stray `End`).
    for err in &parse.errors {
        ctx.out.push(Diagnostic {
            span: Span::new(err.start as u32, err.end as u32),
            severity: Severity::Error,
            code: "syntax",
            message: err.message.clone(),
        });
    }

    let root = parse.syntax();
    for node in root.children() {
        match node.kind() {
            SyntaxKind::BLOCK => ctx.block(&node),
            SyntaxKind::FIELD => {
                // A field at file scope is meaningless to the engine.
                if let Some(key) = Field(node.clone()).key() {
                    ctx.error(
                        &key,
                        "stray-field",
                        format!("`{}` is not inside a block", key.text()),
                    );
                }
            }
            _ => {}
        }
    }

    ctx.out
}

struct Ctx<'a> {
    analyzer: &'a Analyzer,
    index: Option<&'a WorkspaceIndex>,
    out: Vec<Diagnostic>,
}

impl<'a> Ctx<'a> {
    fn block(&mut self, node: &SyntaxNode) {
        let block = Block(node.clone());
        let schema = scope_schema(self.analyzer, node);
        if let Some(keyword) = block.keyword() {
            if self.analyzer.block(keyword.text()).is_none() {
                self.error(
                    &keyword,
                    "unknown-block",
                    format!("unknown block type `{}`", keyword.text()),
                );
            }
        }
        self.walk(node, &schema);
    }

    /// Validate every field / nested scope directly inside `node`, given the
    /// resolved schema of `node` itself.
    fn walk(&mut self, node: &SyntaxNode, scope: &ScopeSchema) {
        for child in node.children() {
            match child.kind() {
                SyntaxKind::FIELD => self.field(&child, scope),
                SyntaxKind::MODULE => self.module(&child, scope),
                SyntaxKind::BLOCK => {
                    // Blocks nested in blocks are unusual but handled for safety.
                    let inner = scope_schema(self.analyzer, &child);
                    self.walk(&child, &inner);
                }
                _ => {}
            }
        }
    }

    fn field(&mut self, node: &SyntaxNode, scope: &ScopeSchema) {
        let field = Field(node.clone());
        let Some(key) = field.key() else { return };
        let name = key.text();

        if let Some(schema_field) = scope.field(name) {
            self.validate_value(&field, &schema_field.value_type);
        } else if scope.has_field_schema()
            && !scope.module_slots().iter().any(|s| s.keyword == name)
        {
            self.warning(
                &key,
                "unknown-field",
                format!("unknown field `{name}` in {}", scope.label()),
            );
        }
    }

    fn module(&mut self, node: &SyntaxNode, parent: &ScopeSchema) {
        let module = Module(node.clone());
        // A MODULE node is a *real* module only when its slot keyword is one of
        // the parent block's declared module slots; otherwise it is an
        // anonymous sub-block (e.g. `ConditionState = DAMAGED`) and the token
        // after `=` is an argument, not a module name.
        let is_real_module = module
            .slot()
            .map(|s| parent.module_slots().iter().any(|ms| ms.keyword == s.text()))
            .unwrap_or(false);

        let inner = if is_real_module {
            if let Some(name) = module.module_name() {
                match self.analyzer.module(name.text()) {
                    Some(_) => scope_schema(self.analyzer, node),
                    None => {
                        self.warning(
                            &name,
                            "unknown-module",
                            format!("unknown module `{}`", name.text()),
                        );
                        ScopeSchema::Unknown
                    }
                }
            } else {
                ScopeSchema::Unknown
            }
        } else {
            ScopeSchema::Unknown
        };

        self.walk(node, &inner);
    }

    /// Validate a field's value tokens against its declared type.
    fn validate_value(&mut self, field: &Field, ty: &ValueType) {
        let tokens = field.value_tokens();
        if tokens.is_empty() {
            if let Some(key) = field.key() {
                // Most fields require at least one value; lenient types don't.
                if !matches!(ty, ValueType::Unknown { .. } | ValueType::AsciiStringList) {
                    self.warning(&key, "missing-value", format!("`{}` expects a value", key.text()));
                }
            }
            return;
        }
        let first = &tokens[0];
        match ty {
            ValueType::Bool => {
                let v = unquote(first.text()).to_ascii_lowercase();
                if v != "yes" && v != "no" {
                    self.error(first, "bad-bool", format!("expected `Yes` or `No`, found `{}`", first.text()));
                }
            }
            ValueType::Int => self.check_number(first, NumKind::Int),
            ValueType::UInt => self.check_number(first, NumKind::UInt),
            ValueType::Real
            | ValueType::AngleReal
            | ValueType::Velocity
            | ValueType::Acceleration
            | ValueType::Duration => self.check_number(first, NumKind::Real),
            ValueType::PositiveReal => {
                self.check_number(first, NumKind::Real);
                if let Ok(n) = first.text().parse::<f64>() {
                    if n <= 0.0 {
                        self.warning(first, "non-positive", format!("`{}` should be greater than 0", first.text()));
                    }
                }
            }
            ValueType::Percent => {
                let t = first.text().trim_end_matches('%');
                if t.parse::<f64>().is_err() {
                    self.error(first, "bad-percent", format!("expected a percentage, found `{}`", first.text()));
                }
            }
            ValueType::Enum { value_set } => {
                self.check_enum_member(value_set, first);
            }
            ValueType::BitFlags { value_set } => {
                for tok in &tokens {
                    let raw = tok.text().trim_start_matches(['+', '-']);
                    if raw.eq_ignore_ascii_case("NONE") || raw.eq_ignore_ascii_case("ALL") {
                        continue;
                    }
                    self.check_bitflag_member(value_set, tok, raw);
                }
            }
            ValueType::Reference { ref_kind } => {
                self.check_reference(*ref_kind, first);
            }
            // No value-level validation for these (yet).
            ValueType::AsciiString
            | ValueType::QuotedString
            | ValueType::AsciiStringList
            | ValueType::Color
            | ValueType::Coord2D
            | ValueType::Coord3D
            | ValueType::Unknown { .. } => {}
        }
    }

    fn check_number(&mut self, tok: &SyntaxToken, kind: NumKind) {
        let text = tok.text();
        let ok = match kind {
            NumKind::Int => text.parse::<i64>().is_ok(),
            NumKind::UInt => text.parse::<u64>().is_ok(),
            NumKind::Real => text.parse::<f64>().is_ok(),
        };
        if !ok {
            let what = match kind {
                NumKind::Int => "an integer",
                NumKind::UInt => "a non-negative integer",
                NumKind::Real => "a number",
            };
            self.error(tok, "bad-number", format!("expected {what}, found `{text}`"));
        }
    }

    fn check_enum_member(&mut self, value_set: &str, tok: &SyntaxToken) {
        let Some(set) = self.analyzer.value_set(value_set) else { return };
        if set.members.is_empty() {
            return; // value set we couldn't populate; don't flag
        }
        let v = tok.text();
        if !set.members.iter().any(|m| m.name.eq_ignore_ascii_case(v)) {
            self.error(
                tok,
                "bad-enum",
                format!("`{v}` is not a valid value (expected one of {value_set})"),
            );
        }
    }

    fn check_bitflag_member(&mut self, value_set: &str, tok: &SyntaxToken, raw: &str) {
        let Some(set) = self.analyzer.value_set(value_set) else { return };
        if set.members.is_empty() {
            return;
        }
        if !set.members.iter().any(|m| m.name.eq_ignore_ascii_case(raw)) {
            self.error(
                tok,
                "bad-flag",
                format!("`{raw}` is not a valid {value_set} flag"),
            );
        }
    }

    fn check_reference(&mut self, kind: RefKind, tok: &SyntaxToken) {
        let Some(index) = self.index else { return };
        let name = unquote(tok.text());
        if name.is_empty() || name.eq_ignore_ascii_case("None") {
            return;
        }
        if !index.is_defined(kind, name) {
            self.warning(
                tok,
                "unresolved-reference",
                format!("`{name}` is not defined anywhere in the workspace"),
            );
        }
    }

    fn error(&mut self, tok: &SyntaxToken, code: &'static str, message: String) {
        self.push(tok, Severity::Error, code, message);
    }

    fn warning(&mut self, tok: &SyntaxToken, code: &'static str, message: String) {
        self.push(tok, Severity::Warning, code, message);
    }

    fn push(&mut self, tok: &SyntaxToken, severity: Severity, code: &'static str, message: String) {
        self.out.push(Diagnostic {
            span: tok.text_range().into(),
            severity,
            code,
            message,
        });
    }
}

enum NumKind {
    Int,
    UInt,
    Real,
}

/// Strip surrounding double quotes from a token's text, if present.
fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .map(|s| s.strip_suffix('"').unwrap_or(s))
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let a = Analyzer::embedded();
        diagnose(&a, &a.parse(src), None)
    }

    fn codes(src: &str) -> Vec<&'static str> {
        diags(src).into_iter().map(|d| d.code).collect()
    }

    #[test]
    fn clean_weapon_has_no_diagnostics() {
        let src = "Weapon AK47\n  PrimaryDamage = 50.0\n  ClipSize = 30\nEnd\n";
        assert!(diags(src).is_empty(), "{:?}", diags(src));
    }

    #[test]
    fn unknown_block_is_error() {
        assert!(codes("Wepon AK47\nEnd\n").contains(&"unknown-block"));
    }

    #[test]
    fn unknown_field_is_warning() {
        let src = "Weapon AK47\n  PrimaryDamg = 50.0\nEnd\n";
        let d = diags(src);
        assert!(d.iter().any(|d| d.code == "unknown-field" && d.severity == Severity::Warning));
    }

    #[test]
    fn bad_bool_and_number_are_errors() {
        // ScaleWeaponSpeed is a Bool field; ClipSize is an Int field.
        let src = "Weapon AK47\n  ScaleWeaponSpeed = Maybe\n  ClipSize = lots\nEnd\n";
        let c = codes(src);
        assert!(c.contains(&"bad-bool"), "{c:?}");
        assert!(c.contains(&"bad-number"), "{c:?}");
    }

    #[test]
    fn bad_enum_member_is_error() {
        // DeathType is an Enum over TheDeathNames; BURNED is valid, NONSENSE is not.
        let ok = "Weapon AK47\n  DeathType = BURNED\nEnd\n";
        assert!(!codes(ok).contains(&"bad-enum"), "{:?}", diags(ok));
        let bad = "Weapon AK47\n  DeathType = NONSENSE\nEnd\n";
        assert!(codes(bad).contains(&"bad-enum"));
    }

    #[test]
    fn unterminated_block_is_syntax_error() {
        assert!(codes("Weapon AK47\n  ClipSize = 30\n").contains(&"syntax"));
    }

    #[test]
    fn object_module_fields_are_validated() {
        // ActiveBody.MaxHealth is Real; a bad value should be flagged, and an
        // unknown module field should warn.
        let src = "\
Object Tank
  Body = ActiveBody Tag01
    MaxHealth = lots
    Bogus = 1
  End
End
";
        let c = codes(src);
        assert!(c.contains(&"bad-number"), "{c:?}");
        assert!(c.contains(&"unknown-field"), "{c:?}");
    }

    #[test]
    fn unknown_module_warns() {
        let src = "Object Tank\n  Body = NotARealModule Tag01\n  End\nEnd\n";
        assert!(codes(src).contains(&"unknown-module"));
    }

    #[test]
    fn reference_resolution_with_index() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        // Define nothing; a Weapon reference on an Object should be unresolved.
        // Object.weapon references go through WeaponSet (custom parser), so use a
        // field we know maps to a reference: PrimaryDamageRadius is Real, so
        // instead assert the plumbing via a synthetic check below.
        index.set_file("a.ini", crate::index::definitions_in(&a, &a.parse("Weapon AK47\nEnd\n"), "a.ini"));
        assert!(index.is_defined(RefKind::Weapon, "AK47"));
    }
}
