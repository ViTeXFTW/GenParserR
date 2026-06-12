//! Diagnostics: validates a parsed document against the schema (and, when
//! available, the cross-file index).
//!
//! Two layers, per the project's "stricter / helpful" stance:
//! * engine-faithful errors — unknown block, unknown field, bad value type,
//!   bad enum/bitflag member, unterminated block;
//! * stricter warnings/hints — unknown module, unresolved cross-file reference.

use std::collections::HashMap;
use std::sync::Arc;

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
    let mut out = parser_errors(parse);
    for node in parse.syntax().children() {
        out.extend(diagnose_root_child(analyzer, index, &node));
    }
    out
}

/// Structural errors from the parser (unterminated blocks, stray `End`).
fn parser_errors(parse: &Parse) -> Vec<Diagnostic> {
    parse
        .errors
        .iter()
        .map(|err| Diagnostic {
            span: Span::new(err.start as u32, err.end as u32),
            severity: Severity::Error,
            code: "syntax",
            message: err.message.clone(),
        })
        .collect()
}

/// All schema diagnostics for one direct child of ROOT, with absolute spans.
/// Depends only on the child's own subtree, the schema, and the index — never
/// on siblings — which is what makes per-block caching sound.
fn diagnose_root_child(
    analyzer: &Analyzer,
    index: Option<&WorkspaceIndex>,
    node: &SyntaxNode,
) -> Vec<Diagnostic> {
    let mut ctx = Ctx {
        analyzer,
        index,
        out: Vec::new(),
    };
    match node.kind() {
        SyntaxKind::BLOCK => ctx.block(node),
        SyntaxKind::FIELD => {
            // A field at file scope is meaningless to the engine — unless
            // it is a single-line directive block (`BenchProfile`,
            // `ReallyLowMHz`), which the parser deliberately emits as a
            // field.
            if let Some(key) = Field(node.clone()).key() {
                let is_inline_block = ctx
                    .analyzer
                    .block(key.text())
                    .is_some_and(|b| !b.terminated);
                if !is_inline_block {
                    ctx.error(
                        &key,
                        "stray-field",
                        format!("`{}` is not inside a block", key.text()),
                    );
                }
            }
        }
        _ => {}
    }
    ctx.out
}

/// A per-document cache of block-level diagnostics, keyed on green-node
/// identity. After an incremental reparse, unchanged top-level blocks keep
/// pointer-identical green nodes, so their diagnostics are reused (spans are
/// stored block-relative and rebased to the block's new offset).
///
/// Reference diagnostics depend on the [`WorkspaceIndex`], so the cache is
/// cleared whenever the index [`generation`](WorkspaceIndex::generation)
/// changes.
#[derive(Default)]
pub struct DiagnosticsCache {
    index_generation: Option<u64>,
    map: HashMap<usize, CacheEntry>,
    hits: u64,
    misses: u64,
}

struct CacheEntry {
    /// Keeps the green allocation alive so the pointer key can't be reused by
    /// a new allocation while this entry exists.
    _green: rowan::GreenNode,
    /// Diagnostics with spans relative to the block's start offset.
    diags: Arc<Vec<Diagnostic>>,
}

impl DiagnosticsCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cumulative (hits, misses) over the cache's lifetime.
    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }
}

/// Like [`diagnose`], but reuses per-block results from `cache` for top-level
/// children whose green nodes are unchanged. Always returns exactly what
/// [`diagnose`] would (equivalence is tested); the cache only saves work.
pub fn diagnose_with_cache(
    analyzer: &Analyzer,
    parse: &Parse,
    index: Option<&WorkspaceIndex>,
    cache: &mut DiagnosticsCache,
) -> Vec<Diagnostic> {
    let generation = index.map(|i| i.generation());
    if cache.index_generation != generation {
        cache.map.clear();
        cache.index_generation = generation;
    }

    let mut out = parser_errors(parse);
    let mut next: HashMap<usize, CacheEntry> = HashMap::with_capacity(cache.map.len() + 8);
    for node in parse.syntax().children() {
        let offset = u32::from(node.text_range().start());
        let green = node.green().into_owned();
        let key = &*green as *const rowan::GreenNodeData as usize;

        let diags = if let Some(entry) = cache.map.get(&key) {
            cache.hits += 1;
            entry.diags.clone()
        } else {
            cache.misses += 1;
            let absolute = diagnose_root_child(analyzer, index, &node);
            Arc::new(
                absolute
                    .into_iter()
                    .map(|d| Diagnostic {
                        span: Span::new(d.span.start - offset, d.span.end - offset),
                        ..d
                    })
                    .collect::<Vec<_>>(),
            )
        };
        out.extend(diags.iter().map(|d| Diagnostic {
            span: Span::new(d.span.start + offset, d.span.end + offset),
            ..d.clone()
        }));
        next.insert(
            key,
            CacheEntry {
                _green: green,
                diags,
            },
        );
    }
    // Entries for children no longer in the tree are dropped here.
    cache.map = next;
    out
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
            // Only plain `Object`s: an ObjectReskin inherits its parent's
            // modules and sets, so neither side of the pairing is visible.
            if keyword.text() == "Object" {
                self.check_set_reachability(node);
            }
        }
        self.walk(node, &schema);
    }

    /// Block-local dead-code check: a `WeaponSet`/`ArmorSet` carrying the
    /// `PLAYER_UPGRADE` condition is only ever selected after a module on the
    /// *same* object sets that flag (WeaponSetUpgrade / the TransportContain
    /// family with armed passengers; ArmorUpgrade for armor). Both sides live
    /// in the same top-level block, so the check is sound under the per-block
    /// diagnostics cache. Skipped for map.ini-style override blocks
    /// (`AddModule`/`RemoveModule`/`ReplaceModule` present): those are
    /// partial definitions.
    fn check_set_reachability(&mut self, node: &SyntaxNode) {
        /// Modules that can set WEAPONSET_PLAYER_UPGRADE on their object
        /// (WeaponSetUpgrade.cpp; TransportContain.cpp `onContaining` sets it
        /// on the transport when a rider has a viable weapon — inherited by
        /// the whole TransportContain family).
        const WEAPON_TRIGGERS: [&str; 7] = [
            "WeaponSetUpgrade",
            "TransportContain",
            "HelixContain",
            "InternetHackContain",
            "OverlordContain",
            "RailedTransportContain",
            "RiderChangeContain",
        ];
        /// ArmorUpgrade.cpp is the only setter of ARMORSET_PLAYER_UPGRADE.
        const ARMOR_TRIGGERS: [&str; 1] = ["ArmorUpgrade"];

        let mut module_names: Vec<String> = Vec::new();
        // (set keyword, the PLAYER_UPGRADE condition token) per set sub-block.
        let mut player_upgrade_sets: Vec<(&'static str, SyntaxToken)> = Vec::new();
        let mut is_override_patch = has_direct_field(node, "RemoveModule");

        for child in node.children().filter(|n| n.kind() == SyntaxKind::MODULE) {
            let module = Module(child.clone());
            let Some(slot) = module.slot() else { continue };
            match slot.text() {
                "AddModule" | "ReplaceModule" => is_override_patch = true,
                kw @ ("WeaponSet" | "ArmorSet") => {
                    if let Some(tok) = direct_condition_token(&child, "PLAYER_UPGRADE") {
                        let kw = if kw == "WeaponSet" { "WeaponSet" } else { "ArmorSet" };
                        player_upgrade_sets.push((kw, tok));
                    }
                }
                _ => {
                    if let Some(name) = module.module_name() {
                        module_names.push(name.text().to_string());
                    }
                }
            }
        }
        if is_override_patch {
            return;
        }
        for (kw, tok) in player_upgrade_sets {
            let triggers: &[&str] = if kw == "WeaponSet" {
                &WEAPON_TRIGGERS
            } else {
                &ARMOR_TRIGGERS
            };
            if !module_names.iter().any(|m| triggers.iter().any(|t| t == m)) {
                self.warning(
                    &tok,
                    "unreachable-set",
                    format!(
                        "this `{kw}` requires the `PLAYER_UPGRADE` condition, but no module \
                         on this object can set it (e.g. `{}`) — the set can never be selected",
                        triggers[0]
                    ),
                );
            }
        }
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
                    Some(_) => {
                        // ThingTemplate.cpp: "there must be a module tag
                        // present, and it must be unique across all modules".
                        if module.tag().is_none() {
                            self.warning(
                                &name,
                                "missing-module-tag",
                                format!(
                                    "module `{}` has no module tag (e.g. `ModuleTag_01`)",
                                    name.text()
                                ),
                            );
                        }
                        scope_schema(self.analyzer, node)
                    }
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
            // Sub-block declared by the enclosing scope (WeaponSet, FX
            // nuggets, ...) — resolves to its field schema when modeled.
            scope_schema(self.analyzer, node)
        };

        // A WeaponSet without `Conditions` silently becomes the *default* set
        // (empty flags). Real game data spells `Conditions = None` out in all
        // but 5 of 862 cases, so the omission is almost always an accident.
        // (Not checked for ArmorSet: omitting it there is common practice.)
        if let ScopeSchema::SubBlock(sb) = &inner {
            if sb.keyword == "WeaponSet" && !has_direct_field(node, "Conditions") {
                if let Some(slot) = module.slot() {
                    self.warning(
                        &slot,
                        "missing-condition",
                        "`WeaponSet` has no `Conditions` field, making it the default set; \
                         use `Conditions = None` if that is intended"
                            .into(),
                    );
                }
            }
        }

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
        match ty {
            ValueType::BitFlags { value_set } => {
                for tok in &tokens {
                    let raw = tok.text().trim_start_matches(['+', '-']);
                    if raw.eq_ignore_ascii_case("NONE") || raw.eq_ignore_ascii_case("ALL") {
                        continue;
                    }
                    self.check_bitflag_member(value_set, tok, raw);
                }
            }
            // Every token names a definition of the same kind.
            ValueType::ReferenceList { ref_kind } => {
                for tok in &tokens {
                    self.check_reference(*ref_kind, tok);
                }
            }
            // A fixed sequence of typed tokens; each listed token is required
            // (the engine's parse function calls getNextToken for each).
            ValueType::TokenList { tokens: specs } => {
                for (i, spec) in specs.iter().enumerate() {
                    match tokens.get(i) {
                        Some(tok) => self.check_token(tok, spec),
                        None => {
                            if let Some(key) = field.key() {
                                self.warning(
                                    &key,
                                    "missing-value",
                                    format!(
                                        "`{}` expects {} values, found {}",
                                        key.text(),
                                        specs.len(),
                                        tokens.len()
                                    ),
                                );
                            }
                            break;
                        }
                    }
                }
            }
            // `R:0 G:0 B:0 [A:255]` — components are ints in 0..=255, the `A`
            // component is optional (INI.cpp parseRGBColor / parseColorInt).
            ValueType::Color => self.check_axes(field, &tokens, &["R", "G", "B"], Some("A"), true),
            // `X:0 Y:0 [Z:0]` — reals (INI.cpp parseCoord2D / parseCoord3D).
            ValueType::Coord2D => self.check_axes(field, &tokens, &["X", "Y"], None, false),
            ValueType::Coord3D => self.check_axes(field, &tokens, &["X", "Y", "Z"], None, false),
            single => self.check_token(&tokens[0], single),
        }
    }

    /// Validate one value token against a single-token type.
    fn check_token(&mut self, tok: &SyntaxToken, ty: &ValueType) {
        match ty {
            ValueType::Bool => {
                let v = unquote(tok.text()).to_ascii_lowercase();
                if v != "yes" && v != "no" {
                    self.error(tok, "bad-bool", format!("expected `Yes` or `No`, found `{}`", tok.text()));
                }
            }
            ValueType::Int => self.check_number(tok, NumKind::Int),
            ValueType::UInt => self.check_number(tok, NumKind::UInt),
            ValueType::Real
            | ValueType::AngleReal
            | ValueType::Velocity
            | ValueType::Acceleration
            | ValueType::Duration => self.check_number(tok, NumKind::Real),
            ValueType::PositiveReal => {
                self.check_number(tok, NumKind::Real);
                if let Ok(n) = tok.text().parse::<f64>() {
                    if n <= 0.0 {
                        self.warning(tok, "non-positive", format!("`{}` should be greater than 0", tok.text()));
                    }
                }
            }
            // Stricter than the engine (which reads a bare real): require the
            // `%` sign, because `Armor = X 2` almost never means 2 percent.
            ValueType::Percent => {
                let ok = tok
                    .text()
                    .strip_suffix('%')
                    .is_some_and(|n| n.parse::<f64>().is_ok());
                if !ok {
                    self.error(
                        tok,
                        "bad-percent",
                        format!("expected a percentage like `100%`, found `{}`", tok.text()),
                    );
                }
            }
            ValueType::Enum { value_set } => self.check_enum_member(value_set, tok),
            ValueType::BitFlags { value_set } => {
                let raw = tok.text().trim_start_matches(['+', '-']);
                if !raw.eq_ignore_ascii_case("NONE") && !raw.eq_ignore_ascii_case("ALL") {
                    self.check_bitflag_member(value_set, tok, raw);
                }
            }
            ValueType::Reference { ref_kind } | ValueType::ReferenceList { ref_kind } => {
                self.check_reference(*ref_kind, tok)
            }
            // No single-token validation for these.
            ValueType::AsciiString
            | ValueType::QuotedString
            | ValueType::AsciiStringList
            | ValueType::Color
            | ValueType::Coord2D
            | ValueType::Coord3D
            | ValueType::TokenList { .. }
            | ValueType::Unknown { .. } => {}
        }
    }

    /// Validate a tagged-axis value (`R:255 G:0 B:0`, `X:1 Y:2 Z:3`). The
    /// engine tokenizes with `:` as a separator, so `R:255`, `R: 255` and
    /// `R : 255` are all accepted; tags match case-insensitively and in order.
    /// `int_0_255` selects the color rule (ints 0..=255) over reals.
    fn check_axes(
        &mut self,
        field: &Field,
        tokens: &[SyntaxToken],
        axes: &[&str],
        optional: Option<&str>,
        int_0_255: bool,
    ) {
        let code = if int_0_255 { "bad-color" } else { "bad-coord" };
        let expected = || {
            axes.iter()
                .map(|a| format!("{a}:<n>"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        // Flatten to (subtoken, source token), splitting each WORD on `:` the
        // way the engine's separator set does.
        let mut parts: Vec<(&str, &SyntaxToken)> = Vec::new();
        for tok in tokens {
            for part in tok.text().split(':') {
                if !part.is_empty() {
                    parts.push((part, tok));
                }
            }
        }
        let mut i = 0;
        let mut take = |this: &mut Self, axis: &str, required: bool| -> bool {
            let Some(&(tag, tag_tok)) = parts.get(i) else {
                if required {
                    if let Some(key) = field.key() {
                        this.error(
                            &key,
                            code,
                            format!("`{}` expects `{}`", key.text(), expected()),
                        );
                    }
                }
                return false;
            };
            if !tag.eq_ignore_ascii_case(axis) {
                if required {
                    this.error(
                        tag_tok,
                        code,
                        format!("expected `{axis}:`, found `{tag}` (format: `{}`)", expected()),
                    );
                }
                return false;
            }
            let Some(&(value, value_tok)) = parts.get(i + 1) else {
                this.error(tag_tok, code, format!("`{axis}:` is missing its value"));
                return false;
            };
            if int_0_255 {
                match value.parse::<i64>() {
                    Ok(n) if (0..=255).contains(&n) => {}
                    Ok(n) => this.error(
                        value_tok,
                        code,
                        format!("`{axis}:{n}` is out of range (0-255)"),
                    ),
                    Err(_) => this.error(
                        value_tok,
                        code,
                        format!("expected an integer for `{axis}:`, found `{value}`"),
                    ),
                }
            } else if value.parse::<f64>().is_err() {
                this.error(
                    value_tok,
                    code,
                    format!("expected a number for `{axis}:`, found `{value}`"),
                );
            }
            i += 2;
            true
        };
        for axis in axes {
            if !take(self, axis, true) {
                return;
            }
        }
        if let Some(axis) = optional {
            take(self, axis, false);
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
        // `None` is the universal null reference; `NoSound` is the audio one;
        // builtins (e.g. `Upgrade_Veterancy_*`) exist in no file by design.
        if name.is_empty()
            || name.eq_ignore_ascii_case("None")
            || (kind == RefKind::AudioEvent && name.eq_ignore_ascii_case("NoSound"))
            || self.analyzer.is_builtin(kind, name)
        {
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

/// Does `node` directly contain a FIELD line whose key is `name`?
fn has_direct_field(node: &SyntaxNode, name: &str) -> bool {
    node.children()
        .filter(|n| n.kind() == SyntaxKind::FIELD)
        .filter_map(|n| Field(n).key())
        .any(|k| k.text() == name)
}

/// The value token of `node`'s direct `Conditions` field that equals `cond`
/// (case-insensitive, ignoring a `+` prefix), if any.
fn direct_condition_token(node: &SyntaxNode, cond: &str) -> Option<SyntaxToken> {
    node.children()
        .filter(|n| n.kind() == SyntaxKind::FIELD)
        .map(Field)
        .filter(|f| f.key().is_some_and(|k| k.text() == "Conditions"))
        .flat_map(|f| f.value_tokens())
        .find(|t| t.text().trim_start_matches('+').eq_ignore_ascii_case(cond))
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
    fn set_reachability_flags_only_untriggered_player_upgrade_sets() {
        let upgrade_set = "  WeaponSet\n    Conditions = PLAYER_UPGRADE\n    Weapon = PRIMARY G\n  End\n";
        let trigger = "  Behavior = WeaponSetUpgrade ModuleTag_01\n    TriggeredBy = Upgrade_X\n  End\n";

        let dead = format!("Object T\n{upgrade_set}End\n");
        assert_eq!(codes(&dead).iter().filter(|c| **c == "unreachable-set").count(), 1);

        let alive = format!("Object T\n{upgrade_set}{trigger}End\n");
        assert!(!codes(&alive).contains(&"unreachable-set"), "module triggers the set");

        // The TransportContain family can set the flag without an upgrade.
        let transport = format!(
            "Object T\n{upgrade_set}  Behavior = OverlordContain ModuleTag_02\n  End\nEnd\n"
        );
        assert!(!codes(&transport).contains(&"unreachable-set"));

        // Override patches (map.ini AddModule/RemoveModule) are partial: silent.
        let patched = format!("Object T\n  RemoveModule ModuleTag_99\n{upgrade_set}End\n");
        assert!(!codes(&patched).contains(&"unreachable-set"));

        // ObjectReskin inherits the parent's modules: silent.
        let reskin = format!("ObjectReskin T P\n{upgrade_set}End\n");
        assert!(!codes(&reskin).contains(&"unreachable-set"));

        // Other conditions (VETERAN etc.) are set externally: silent.
        let vet = "Object T\n  WeaponSet\n    Conditions = VETERAN\n    Weapon = PRIMARY G\n  End\nEnd\n";
        assert!(!codes(vet).contains(&"unreachable-set"));
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
    fn cached_diagnose_matches_and_reuses_blocks() {
        let a = Analyzer::embedded();
        let src = "Weapon AK47\n  ClipSize = lots\nEnd\nWeapon B\nEnd\nWeapon C\nEnd\nWeapon D\n  PrimaryDamg = 1\nEnd\n";
        let parse = a.parse(src);
        let mut cache = DiagnosticsCache::new();
        assert_eq!(diagnose_with_cache(&a, &parse, None, &mut cache), diagnose(&a, &parse, None));
        assert_eq!(cache.stats().0, 0, "first run is all misses");

        // Edit inside the *last* block; the splice widens one sibling, so the
        // first two blocks keep pointer-identical green nodes and their
        // diagnostics must come from the cache.
        let edited = src.replace("PrimaryDamg = 1", "PrimaryDamg = 2");
        let at = src.find('1').unwrap();
        let (inc, strategy) = a.reparse(
            &parse,
            src,
            &edited,
            genparser_syntax::Edit { start: at, old_end: at + 1, new_len: 1 },
        );
        assert_eq!(strategy, genparser_syntax::Strategy::Spliced);
        assert_eq!(diagnose_with_cache(&a, &inc, None, &mut cache), diagnose(&a, &inc, None));
        assert!(cache.stats().0 >= 1, "unchanged block must hit the cache");
    }

    #[test]
    fn cache_invalidates_when_index_generation_changes() {
        let a = Analyzer::embedded();
        let mut index = WorkspaceIndex::new();
        // Weapon block referencing a projectile-like name via an Object field
        // isn't trivial to stage; instead verify the mechanism: same parse,
        // index gains a definition between runs, output must follow suit.
        let src = "Weapon AK47\n  ClipSize = 30\nEnd\n";
        let parse = a.parse(src);
        let mut cache = DiagnosticsCache::new();
        let g0 = index.generation();
        let first = diagnose_with_cache(&a, &parse, Some(&index), &mut cache);
        index.set_file("other.ini", crate::index::definitions_in(&a, &a.parse("Object X\nEnd\n"), "other.ini"));
        assert_ne!(index.generation(), g0);
        let second = diagnose_with_cache(&a, &parse, Some(&index), &mut cache);
        assert_eq!(first, second); // no reference fields here; same result
        let (hits, _) = cache.stats();
        assert_eq!(hits, 0, "generation bump must clear the cache");
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
