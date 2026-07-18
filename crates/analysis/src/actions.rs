//! Quick fixes (`textDocument/codeAction`): mechanical repairs that include
//! a scope missing its `End`, a misspelled enum / bitflag member (did-you-mean
//! by edit distance against the value set), and diagnostic-tied fixes (remove
//! unreachable sets, scaffold stub definitions, suppress diagnostics in-file).

use zerosyntax_schema::{RefKind, ValueType};
use zerosyntax_syntax::ast::{Field, Module};
use zerosyntax_syntax::{Parse, SyntaxErrorKind, SyntaxKind, SyntaxNode, SyntaxToken};

use crate::diagnostics::{pragma_rest, pragma_words, Diagnostic};
use crate::model::scope_schema;
use crate::{nav, Analyzer, Span, WorkspaceIndex};

/// One offered fix: replace `span`'s text with `new_text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    pub title: String,
    pub span: Span,
    pub new_text: String,
}

/// All fixes applicable within `range` (the editor's selection/cursor line).
/// `diags` should be the suppression-filtered diagnostics for the document
/// (from `diagnose_with_cache`); `index` is the workspace index for cross-file
/// checks (e.g. stub creation).
pub fn fixes(
    analyzer: &Analyzer,
    parse: &Parse,
    text: &str,
    range: Span,
    diags: &[Diagnostic],
    index: Option<&WorkspaceIndex>,
) -> Vec<Fix> {
    let mut out = Vec::new();
    insert_missing_ends(parse, text, range, &mut out);
    suggest_members(analyzer, parse, range, &mut out);
    diagnostic_fixes(analyzer, parse, text, range, diags, index, &mut out);
    out
}

/// For each unterminated scope intersecting `range`, offer to close it. The
/// parser reports these only at EOF (an open scope swallows the rest of the
/// file), so the fix appends an `End` line at the end of the text, indented
/// like the scope's header line.
fn insert_missing_ends(parse: &Parse, text: &str, range: Span, out: &mut Vec<Fix>) {
    for err in &parse.errors {
        if err.kind != SyntaxErrorKind::UnterminatedBlock {
            continue;
        }
        let (start, end) = (err.start as u32, err.end as u32);
        if end < range.start || start > range.end {
            continue;
        }
        let line_start = text[..err.start].rfind('\n').map_or(0, |i| i + 1);
        let indent = &text[line_start..err.start];
        let head = text[err.start..].split_whitespace().next().unwrap_or("?");
        let lead = if text.ends_with('\n') || text.is_empty() {
            ""
        } else {
            "\n"
        };
        let at = text.len() as u32;
        out.push(Fix {
            title: format!("Insert missing `End` for `{head}`"),
            span: Span::new(at, at),
            new_text: format!("{lead}{indent}End\n"),
        });
    }
}

/// For misspelled enum / bitflag members under `range`, offer the closest
/// value-set members (edit distance ≤ 2, best 3).
fn suggest_members(analyzer: &Analyzer, parse: &Parse, range: Span, out: &mut Vec<Fix>) {
    suggest_in_node(analyzer, &parse.syntax(), range, out);
}

fn suggest_in_node(analyzer: &Analyzer, node: &SyntaxNode, range: Span, out: &mut Vec<Fix>) {
    for child in node.children() {
        let r = child.text_range();
        if u32::from(r.end()) < range.start || u32::from(r.start()) > range.end {
            continue;
        }
        match child.kind() {
            SyntaxKind::FIELD => suggest_in_field(analyzer, &child, range, out),
            SyntaxKind::BLOCK | SyntaxKind::MODULE => suggest_in_node(analyzer, &child, range, out),
            _ => {}
        }
    }
}

fn suggest_in_field(analyzer: &Analyzer, node: &SyntaxNode, range: Span, out: &mut Vec<Fix>) {
    let field = Field(node.clone());
    let Some(key) = field.key() else { return };
    let Some(scope) = node
        .ancestors()
        .skip(1)
        .find(|n| matches!(n.kind(), SyntaxKind::BLOCK | SyntaxKind::MODULE))
    else {
        return;
    };
    let schema = scope_schema(analyzer, &scope);
    let Some(schema_field) = schema.field(key.text()) else {
        return;
    };
    let tokens = field.value_tokens();
    let mut check = |tok: &SyntaxToken, value_set: &str, flags: bool| {
        let span = Span::from(tok.text_range());
        if span.end < range.start || span.start > range.end {
            return;
        }
        let (prefix, raw) = match flags {
            true => {
                let raw = tok.text().trim_start_matches(['+', '-']);
                (&tok.text()[..tok.text().len() - raw.len()], raw)
            }
            false => ("", tok.text()),
        };
        if raw.is_empty()
            || (flags && (raw.eq_ignore_ascii_case("NONE") || raw.eq_ignore_ascii_case("ALL")))
        {
            return;
        }
        let Some(set) = analyzer.value_set(value_set) else {
            return;
        };
        if set.members.iter().any(|m| m.name.eq_ignore_ascii_case(raw)) {
            return; // valid member, nothing to fix
        }
        let mut ranked: Vec<(usize, &str)> = set
            .members
            .iter()
            .map(|m| (edit_distance(raw, &m.name), m.name.as_str()))
            .filter(|(d, _)| *d <= 2)
            .collect();
        ranked.sort();
        for (_, name) in ranked.into_iter().take(3) {
            out.push(Fix {
                title: format!("Replace with `{name}`"),
                span,
                new_text: format!("{prefix}{name}"),
            });
        }
    };
    match &schema_field.value_type {
        ValueType::Enum { value_set } => {
            if let Some(tok) = tokens.first() {
                check(tok, value_set, false);
            }
        }
        ValueType::BitFlags { value_set } => {
            for tok in &tokens {
                check(tok, value_set, true);
            }
        }
        ValueType::TokenList { tokens: specs } => {
            for (spec, tok) in specs.iter().zip(tokens.iter()) {
                match spec {
                    ValueType::Enum { value_set } => check(tok, value_set, false),
                    ValueType::BitFlags { value_set } => check(tok, value_set, true),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

// ── Diagnostic-backed fixes ─────────────────────────────────────────────────

/// Dispatch on `diagnostic.code` to offer targeted repairs for diagnostics
/// that overlap `range`.
fn diagnostic_fixes(
    analyzer: &Analyzer,
    parse: &Parse,
    text: &str,
    range: Span,
    diags: &[Diagnostic],
    index: Option<&WorkspaceIndex>,
    out: &mut Vec<Fix>,
) {
    use std::collections::HashSet;
    let mut suppress_seen: HashSet<&'static str> = HashSet::new();
    // (block_start, trigger): deduplicates scaffold inserts per object.
    let mut scaffold_seen: HashSet<(u32, &'static str)> = HashSet::new();

    for d in diags
        .iter()
        .filter(|d| d.span.end >= range.start && d.span.start <= range.end)
    {
        match d.code {
            "unreachable-set" => {
                unreachable_set_fixes(parse, text, d, &mut scaffold_seen, out);
            }
            "unresolved-reference" => {
                stub_definition_fix(analyzer, parse, text, d, index, out);
            }
            _ => {}
        }
        // Suppressing the misspelled-suppression hint is self-defeating.
        if d.code != "unknown-suppression" && suppress_seen.insert(d.code) {
            suppress_fix(parse, text, d.code, out);
        }
    }
}

/// For an `unreachable-set` diagnostic offer the mechanical counterpart:
/// remove/scaffold for dead sets, or add the missing set for a useless upgrade.
fn unreachable_set_fixes(
    parse: &Parse,
    text: &str,
    d: &Diagnostic,
    scaffold_seen: &mut std::collections::HashSet<(u32, &'static str)>,
    out: &mut Vec<Fix>,
) {
    // Resolve the PLAYER_UPGRADE token the diagnostic sits on.
    let off = rowan::TextSize::from(d.span.start);
    let root = parse.syntax();
    let tok = root
        .token_at_offset(off)
        .right_biased()
        .or_else(|| root.token_at_offset(off).left_biased());
    let Some(tok) = tok else { return };

    // tok → FIELD (Conditions = PLAYER_UPGRADE) → MODULE (WeaponSet/ArmorSet).
    let set_module_node = tok
        .parent()
        .and_then(|n| n.parent())
        .filter(|n| n.kind() == SyntaxKind::MODULE);
    let Some(set_module_node) = set_module_node else {
        if tok.text().eq_ignore_ascii_case("WeaponSetUpgrade") {
            unused_weapon_set_upgrade_fix(text, &tok, scaffold_seen, out);
        }
        return;
    };

    let set_ast = Module(set_module_node.clone());
    let Some(slot_tok) = set_ast.slot() else {
        return;
    };
    let set_keyword = slot_tok.text();

    let trigger: &'static str = match set_keyword {
        "WeaponSet" => "WeaponSetUpgrade",
        "ArmorSet" => "ArmorUpgrade",
        _ => return,
    };

    // (a) Remove the whole sub-block (text_range covers leading indent…End\n).
    let set_span = Span::from(set_module_node.text_range());
    out.push(Fix {
        title: format!("Remove unreachable `{set_keyword}`"),
        span: set_span,
        new_text: String::new(),
    });

    // (b) Insert trigger module before the Object's End line.
    let Some(object_node) = set_module_node
        .ancestors()
        .find(|n| n.kind() == SyntaxKind::BLOCK)
    else {
        return;
    };

    let block_start = u32::from(object_node.text_range().start());
    if !scaffold_seen.insert((block_start, trigger)) {
        return; // One scaffold per (object, trigger) pair.
    }

    // Collect existing module tags (case-insensitive) to avoid collisions.
    let used_tags: std::collections::HashSet<String> = object_node
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::MODULE)
        .filter_map(|n| Module(n).tag())
        .map(|t| t.text().to_ascii_lowercase())
        .collect();
    let mut n = 1u32;
    let tag = loop {
        let candidate = format!("ModuleTag_{n:02}");
        if !used_tags.contains(&candidate.to_ascii_lowercase()) {
            break candidate;
        }
        n += 1;
    };

    let insert_at = find_end_line_start(&object_node, text)
        .unwrap_or_else(|| u32::from(object_node.text_range().end()));

    let indent = leading_indent(&set_module_node, text);
    let body_indent = format!("{indent}  ");

    out.push(Fix {
        title: format!("Insert `{trigger}` trigger module"),
        span: Span::new(insert_at, insert_at),
        new_text: format!(
            "{indent}Behavior = {trigger} {tag}\n{body_indent}TriggeredBy = Upgrade_TODO\n{indent}End\n"
        ),
    });
}

fn unused_weapon_set_upgrade_fix(
    text: &str,
    tok: &SyntaxToken,
    scaffold_seen: &mut std::collections::HashSet<(u32, &'static str)>,
    out: &mut Vec<Fix>,
) {
    let Some(module_node) = tok.parent().filter(|n| n.kind() == SyntaxKind::MODULE) else {
        return;
    };
    let Some(object_node) = module_node
        .ancestors()
        .find(|n| n.kind() == SyntaxKind::BLOCK)
    else {
        return;
    };
    let block_start = u32::from(object_node.text_range().start());
    if !scaffold_seen.insert((block_start, "WeaponSet")) {
        return;
    }

    let insert_at = u32::from(module_node.text_range().start());
    let indent = leading_indent(&module_node, text);
    let body_indent = format!("{indent}  ");
    out.push(Fix {
        title: "Insert `PLAYER_UPGRADE` WeaponSet".to_string(),
        span: Span::new(insert_at, insert_at),
        new_text: format!(
            "{indent}WeaponSet\n{body_indent}Conditions = PLAYER_UPGRADE\n{body_indent}Weapon = PRIMARY NONE\n{indent}End\n"
        ),
    });
}

fn leading_indent<'a>(node: &SyntaxNode, text: &'a str) -> &'a str {
    let start = u32::from(node.text_range().start()) as usize;
    let indent_len = text[start..]
        .bytes()
        .take_while(|&b| b == b' ' || b == b'\t')
        .count();
    &text[start..start + indent_len]
}

/// Return the byte offset of the start of the `End` line within `block`.
fn find_end_line_start(block: &SyntaxNode, text: &str) -> Option<u32> {
    // children_with_tokens() is not DoubleEndedIterator; collect to reverse.
    let children: Vec<_> = block.children_with_tokens().collect();
    for el in children.iter().rev() {
        let Some(tok) = el.as_token() else { continue };
        if tok.kind() == SyntaxKind::WORD && tok.text().eq_ignore_ascii_case("end") {
            let end_start = u32::from(tok.text_range().start()) as usize;
            let line_start = text[..end_start].rfind('\n').map_or(0, |i| i + 1);
            return Some(line_start as u32);
        }
    }
    None
}

/// For an `unresolved-reference` diagnostic, offer to append a stub definition
/// at the end of the current file.
fn stub_definition_fix(
    analyzer: &Analyzer,
    parse: &Parse,
    text: &str,
    d: &Diagnostic,
    index: Option<&WorkspaceIndex>,
    out: &mut Vec<Fix>,
) {
    let Some(ref_at) = nav::reference_at(analyzer, parse, d.span.start) else {
        return;
    };
    let name = &ref_at.name;
    if name.contains(char::is_whitespace) {
        return; // Quoted multi-word names can't be a block header.
    }
    if index.is_some_and(|i| i.is_defined(ref_at.kind, name)) {
        return; // Defensive: already defined, diagnostic shouldn't have fired.
    }
    let Some(keyword) = stub_keyword(analyzer, ref_at.kind) else {
        return;
    };
    let lead = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let at = text.len() as u32;
    out.push(Fix {
        title: format!("Create stub `{keyword} {name}`"),
        span: Span::new(at, at),
        new_text: format!("{lead}\n{keyword} {name}\nEnd\n"),
    });
}

/// Return the single block keyword that defines `kind`, or `None` if the kind
/// maps to zero or multiple block keywords. For `Object` (also defined by
/// `ObjectReskin`), the preferred keyword `"Object"` is returned directly.
fn stub_keyword<'a>(analyzer: &'a Analyzer, kind: RefKind) -> Option<&'a str> {
    // ObjectReskin requires a parent-object argument, so it is never a valid stub.
    const PREFERRED: &[(RefKind, &str)] = &[(RefKind::Object, "Object")];
    if let Some(&(_, kw)) = PREFERRED.iter().find(|(k, _)| *k == kind) {
        if analyzer
            .schema()
            .blocks
            .iter()
            .any(|b| b.defines == Some(kind))
        {
            return Some(kw);
        }
    }
    let mut found: Option<&'a str> = None;
    for block in &analyzer.schema().blocks {
        if block.defines == Some(kind) {
            if found.is_some() {
                return None; // Ambiguous.
            }
            found = Some(block.name.as_str());
        }
    }
    found
}

/// Offer to add `; zerosyntax-disable: <code>` at the top of the file (or
/// append to an existing pragma line) for a diagnostic.
fn suppress_fix(parse: &Parse, text: &str, code: &'static str, out: &mut Vec<Fix>) {
    let root = parse.syntax();
    let mut first_pragma: Option<(u32, bool)> = None; // (insert offset, has_any_codes)
    let mut already_suppressed = false;

    for el in root.children_with_tokens() {
        let Some(tok) = el.as_token() else { continue };
        let Some((_base, rest)) = pragma_rest(tok) else {
            continue;
        };
        let code_listed = pragma_words(rest).any(|(s, e)| &rest[s..e] == code);
        if code_listed {
            already_suppressed = true;
            break;
        }
        if first_pragma.is_none() {
            let tok_end = u32::from(tok.text_range().end());
            // CRLF: the COMMENT token includes the trailing \r; insert before it.
            let insert_at = if tok.text().ends_with('\r') {
                tok_end - 1
            } else {
                tok_end
            };
            let has_words = pragma_words(rest).next().is_some();
            first_pragma = Some((insert_at, has_words));
        }
    }

    if already_suppressed {
        return;
    }

    if let Some((insert_at, has_words)) = first_pragma {
        let append = if has_words {
            format!(", {code}")
        } else {
            format!(" {code}")
        };
        out.push(Fix {
            title: format!("Suppress `{code}` in this file"),
            span: Span::new(insert_at, insert_at),
            new_text: append,
        });
    } else {
        // No existing pragma: insert a new one at the top (after any BOM).
        let bom_offset = if text.starts_with('\u{feff}') {
            3u32
        } else {
            0u32
        };
        out.push(Fix {
            title: format!("Suppress `{code}` in this file"),
            span: Span::new(bom_offset, bom_offset),
            new_text: format!("; zerosyntax-disable: {code}\n"),
        });
    }
}

// ── Edit-distance helper ─────────────────────────────────────────────────────

/// Case-insensitive Levenshtein distance, early-exiting via the band bound
/// implied by `ED_MAX` callers use (small inputs; plain DP is fine).
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().map(|c| c.to_ascii_uppercase()).collect();
    let b: Vec<char> = b.chars().map(|c| c.to_ascii_uppercase()).collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::diagnose;
    use crate::index::{definitions_in, WorkspaceIndex};
    use crate::Analyzer;

    fn all_fixes(src: &str) -> Vec<Fix> {
        let a = Analyzer::embedded();
        let parse = a.parse(src);
        let mut idx = WorkspaceIndex::new();
        idx.set_file("test.ini", definitions_in(&a, &parse, "test.ini"));
        let diags = diagnose(&a, &parse, Some(&idx), Some("test.ini"));
        fixes(
            &a,
            &parse,
            src,
            Span::new(0, src.len() as u32),
            &diags,
            Some(&idx),
        )
    }

    #[test]
    fn offers_missing_end() {
        let a = Analyzer::embedded();
        let src = "Object Tank\n  MaxHealth = 1\n";
        let parse = a.parse(src);
        let fx = fixes(&a, &parse, src, Span::new(0, src.len() as u32), &[], None);
        assert_eq!(fx.len(), 1, "{fx:?}");
        assert!(fx[0].title.contains("`End`") && fx[0].title.contains("Object"));
        assert_eq!(fx[0].new_text, "End\n");
        assert_eq!(fx[0].span, Span::new(src.len() as u32, src.len() as u32));
    }

    #[test]
    fn suggests_close_enum_and_flag_members() {
        let a = Analyzer::embedded();
        // `Appearance` is enum locomotor_appearance; TREDS ≈ TREADS.
        let src = "Locomotor L\n  Appearance = TREDS\nEnd\n";
        let parse = a.parse(src);
        let fx = fixes(&a, &parse, src, Span::new(0, src.len() as u32), &[], None);
        assert!(fx.iter().any(|f| f.new_text == "TREADS"), "{fx:?}");

        // Bitflag with prefix op: the `+` is preserved in the replacement.
        let src2 = "Object T\n  Behavior = SlowDeathBehavior ModuleTag_01\n    DeathTypes = NONE +EXPLODDED\n  End\nEnd\n";
        let parse2 = a.parse(src2);
        let fx2 = fixes(
            &a,
            &parse2,
            src2,
            Span::new(0, src2.len() as u32),
            &[],
            None,
        );
        assert!(fx2.iter().any(|f| f.new_text == "+EXPLODED"), "{fx2:?}");
    }

    #[test]
    fn valid_members_get_no_fixes() {
        let a = Analyzer::embedded();
        let src = "Locomotor L\n  Appearance = TREADS\nEnd\n";
        let parse = a.parse(src);
        assert!(fixes(&a, &parse, src, Span::new(0, src.len() as u32), &[], None).is_empty());
    }

    // ── unreachable-set fixes ────────────────────────────────────────────────

    #[test]
    fn unreachable_set_offers_remove_and_insert() {
        let src = concat!(
            "Object Tank\n",
            "  WeaponSet\n",
            "    Conditions = PLAYER_UPGRADE\n",
            "    Weapon = PRIMARY MyGun\n",
            "  End\n",
            "End\n",
            "Weapon MyGun\n",
            "  PrimaryDamage = 10.0\n",
            "End\n",
        );
        let fx = all_fixes(src);
        assert!(
            fx.iter()
                .any(|f| f.title.contains("Remove unreachable") && f.title.contains("WeaponSet")),
            "missing remove fix: {fx:?}"
        );
        assert!(
            fx.iter().any(|f| f.title.contains("WeaponSetUpgrade")),
            "missing insert fix: {fx:?}"
        );
        // Apply remove fix: the WeaponSet sub-block should disappear.
        let remove = fx
            .iter()
            .find(|f| f.title.contains("Remove unreachable"))
            .unwrap();
        let mut result = src.to_string();
        result.replace_range(
            remove.span.start as usize..remove.span.end as usize,
            &remove.new_text,
        );
        assert!(
            !result.contains("WeaponSet"),
            "WeaponSet not removed: {result}"
        );
        assert!(
            result.contains("End\n"),
            "Object End should survive: {result}"
        );

        // Apply insert fix: scaffold appears before the Object's End.
        let insert = fx
            .iter()
            .find(|f| f.title.contains("WeaponSetUpgrade"))
            .unwrap();
        let mut result2 = src.to_string();
        result2.replace_range(
            insert.span.start as usize..insert.span.end as usize,
            &insert.new_text,
        );
        assert!(
            result2.contains("Behavior = WeaponSetUpgrade"),
            "scaffold missing: {result2}"
        );
        assert!(result2.contains("ModuleTag_01"), "tag missing: {result2}");
        assert!(
            result2.contains("TriggeredBy = Upgrade_TODO"),
            "TriggeredBy missing: {result2}"
        );
    }

    #[test]
    fn unused_weapon_set_upgrade_offers_player_upgrade_weapon_set() {
        let src = concat!(
            "Object Tank\n",
            "  WeaponSet\n",
            "    Conditions = NONE\n",
            "    Weapon = PRIMARY MyGun\n",
            "  End\n",
            "  Behavior = WeaponSetUpgrade ModuleTag_01\n",
            "    TriggeredBy = Upgrade_MyGun\n",
            "  End\n",
            "End\n",
            "Weapon MyGun\n",
            "  PrimaryDamage = 10.0\n",
            "End\n",
        );
        let fx = all_fixes(src);
        let insert = fx
            .iter()
            .find(|f| f.title.contains("PLAYER_UPGRADE") && f.title.contains("WeaponSet"))
            .expect("missing WeaponSet scaffold fix");
        let mut result = src.to_string();
        result.replace_range(
            insert.span.start as usize..insert.span.end as usize,
            &insert.new_text,
        );
        assert!(result.contains(
            "  WeaponSet\n    Conditions = PLAYER_UPGRADE\n    Weapon = PRIMARY NONE\n  End\n  Behavior = WeaponSetUpgrade"
        ));
    }

    #[test]
    fn unreachable_set_tag_skips_collisions() {
        // Object already has ModuleTag_01 and MODULETAG_02 (case variant) → should generate ModuleTag_03.
        let src = concat!(
            "Object Tank\n",
            "  WeaponSet\n",
            "    Conditions = PLAYER_UPGRADE\n",
            "    Weapon = PRIMARY MyGun\n",
            "  End\n",
            "  Behavior = SlowDeathBehavior ModuleTag_01\n",
            "    DeathTypes = NONE\n",
            "  End\n",
            "  Behavior = SlowDeathBehavior MODULETAG_02\n",
            "    DeathTypes = NONE\n",
            "  End\n",
            "End\n",
            "Weapon MyGun\n",
            "  PrimaryDamage = 10.0\n",
            "End\n",
        );
        let fx = all_fixes(src);
        let insert = fx
            .iter()
            .find(|f| f.title.contains("WeaponSetUpgrade"))
            .unwrap();
        assert!(
            insert.new_text.contains("ModuleTag_03"),
            "wrong tag: {}",
            insert.new_text
        );
    }

    #[test]
    fn unreachable_set_armor_variant() {
        let src = concat!(
            "Object Tank\n",
            "  ArmorSet\n",
            "    Conditions = PLAYER_UPGRADE\n",
            "    Armor = MyArmor\n",
            "  End\n",
            "End\n",
            "Armor MyArmor\n",
            "  Armor = Default 100%\n",
            "End\n",
        );
        let fx = all_fixes(src);
        assert!(
            fx.iter().any(|f| f.title.contains("ArmorUpgrade")),
            "ArmorUpgrade scaffold not offered: {fx:?}"
        );
        assert!(
            fx.iter()
                .any(|f| f.title.contains("Remove unreachable") && f.title.contains("ArmorSet")),
            "ArmorSet remove not offered: {fx:?}"
        );
    }

    #[test]
    fn unreachable_set_removal_when_set_is_last_child() {
        // The WeaponSet is the only child; Object's End line must survive intact.
        let src = concat!(
            "Object Tank\n",
            "  WeaponSet\n",
            "    Conditions = PLAYER_UPGRADE\n",
            "    Weapon = PRIMARY MyGun\n",
            "  End\n",
            "End\n",
            "Weapon MyGun\n",
            "  PrimaryDamage = 10.0\n",
            "End\n",
        );
        let fx = all_fixes(src);
        let remove = fx
            .iter()
            .find(|f| f.title.contains("Remove unreachable"))
            .unwrap();
        let mut result = src.to_string();
        result.replace_range(
            remove.span.start as usize..remove.span.end as usize,
            &remove.new_text,
        );
        // Object End must be present.
        assert!(
            result.contains("Object Tank\nEnd\n"),
            "Object End corrupted: {result}"
        );
    }

    #[test]
    fn unreachable_set_dedupes_scaffold_per_object() {
        // Two dead WeaponSets → 2 remove fixes, 1 insert fix per object.
        let src = concat!(
            "Object Tank\n",
            "  WeaponSet\n",
            "    Conditions = PLAYER_UPGRADE\n",
            "    Weapon = PRIMARY MyGun\n",
            "  End\n",
            "  WeaponSet\n",
            "    Conditions = PLAYER_UPGRADE\n",
            "    Weapon = PRIMARY MyGun\n",
            "  End\n",
            "End\n",
            "Weapon MyGun\n",
            "  PrimaryDamage = 10.0\n",
            "End\n",
        );
        let fx = all_fixes(src);
        let removes: Vec<_> = fx
            .iter()
            .filter(|f| f.title.contains("Remove unreachable"))
            .collect();
        let inserts: Vec<_> = fx
            .iter()
            .filter(|f| f.title.contains("WeaponSetUpgrade"))
            .collect();
        assert_eq!(removes.len(), 2, "expected 2 remove fixes: {fx:?}");
        assert_eq!(inserts.len(), 1, "expected 1 insert fix (deduped): {fx:?}");
    }

    // ── stub definition fixes ────────────────────────────────────────────────

    #[test]
    fn stub_fix_appends_definition() {
        // Weapon block with FireFX referencing a non-existent FXList.
        let src = "Weapon MyWeapon\n  FireFX = NoSuchFX\nEnd\n";
        let fx = all_fixes(src);
        let stub = fx
            .iter()
            .find(|f| f.title.contains("Create stub"))
            .expect("stub fix not offered");
        assert!(
            stub.new_text.contains("FXList NoSuchFX"),
            "wrong stub: {}",
            stub.new_text
        );
        assert!(
            stub.new_text.ends_with("End\n"),
            "stub not terminated: {}",
            stub.new_text
        );
        // Applied at EOF.
        assert_eq!(stub.span.start, src.len() as u32);

        // File without trailing newline: stub gets a leading blank line.
        let src2 = "Weapon MyWeapon\n  FireFX = NoSuchFX\nEnd";
        let fx2 = all_fixes(src2);
        let stub2 = fx2
            .iter()
            .find(|f| f.title.contains("Create stub"))
            .unwrap();
        assert!(
            stub2.new_text.starts_with('\n'),
            "missing lead newline: {:?}",
            stub2.new_text
        );
    }

    #[test]
    fn stub_fix_kind_disambiguation() {
        // Upgrade ref → exactly one block keyword "Upgrade" → stub offered.
        let src_upgrade = concat!(
            "Object Foo\n",
            "  Behavior = WeaponSetUpgrade ModuleTag_01\n",
            "    TriggeredBy = NoSuchUpgrade\n",
            "  End\n",
            "End\n",
        );
        let fx = all_fixes(src_upgrade);
        assert!(
            fx.iter()
                .any(|f| f.title.contains("Create stub") && f.title.contains("Upgrade")),
            "Upgrade stub not offered: {fx:?}"
        );

        // AudioEvent ref → ambiguous (AudioEvent/DialogEvent/MusicTrack) → no stub.
        let src_audio = "Weapon MyWeapon\n  FireSound = NoSuchSound\nEnd\n";
        let fx_audio = all_fixes(src_audio);
        assert!(
            !fx_audio.iter().any(|f| f.title.contains("Create stub")),
            "stub should not be offered for ambiguous AudioEvent kind: {fx_audio:?}"
        );
    }

    // ── suppress fixes ───────────────────────────────────────────────────────

    #[test]
    fn suppress_fix_inserts_pragma_at_top() {
        // Warning diagnostic, no existing pragma → inserts at offset 0.
        let src = "Weapon MyWeapon\n  FireFX = NoSuchFX\nEnd\n";
        let fx = all_fixes(src);
        let sup = fx
            .iter()
            .find(|f| f.title.contains("Suppress"))
            .expect("suppress not offered");
        assert_eq!(sup.span.start, 0, "should insert at top");
        assert!(
            sup.new_text.starts_with("; zerosyntax-disable:"),
            "wrong pragma: {}",
            sup.new_text
        );
        assert!(sup.new_text.ends_with('\n'), "pragma must end with newline");

        // No existing pragma → inserts at offset 0 (suppress_fix has a BOM guard
        // that would shift to offset 3 if needed, but the lexer doesn't currently
        // strip BOMs so that path is exercised only by the code guard, not a test).
        assert_eq!(sup.span.start, 0, "should insert at top of file");
    }

    #[test]
    fn suppress_fix_appends_to_existing_pragma() {
        // Existing pragma with another code → appends ", <code>".
        let src =
            "; zerosyntax-disable: unknown-module\nWeapon MyWeapon\n  FireFX = NoSuchFX\nEnd\n";
        let fx = all_fixes(src);
        let sup = fx
            .iter()
            .find(|f| f.title.contains("Suppress"))
            .expect("suppress not offered: {fx:?}");
        assert!(
            sup.new_text.starts_with(", "),
            "should append with comma: {:?}",
            sup.new_text
        );
        assert!(
            sup.new_text.contains("unresolved-reference"),
            "wrong code: {:?}",
            sup.new_text
        );

        // Bare pragma (colon only, no codes yet) → appends " <code>".
        let src2 = "; zerosyntax-disable:\nWeapon MyWeapon\n  FireFX = NoSuchFX\nEnd\n";
        let fx2 = all_fixes(src2);
        let sup2 = fx2
            .iter()
            .find(|f| f.title.contains("Suppress"))
            .expect("suppress not offered for bare pragma");
        assert!(
            sup2.new_text.starts_with(' '),
            "bare pragma: should prepend space: {:?}",
            sup2.new_text
        );

        // Code already listed → no action.
        let src3 = "; zerosyntax-disable: unresolved-reference\nWeapon MyWeapon\n  FireFX = NoSuchFX\nEnd\n";
        let fx3 = all_fixes(src3);
        assert!(
            !fx3.iter()
                .any(|f| f.title.contains("Suppress") && f.title.contains("unresolved-reference")),
            "should not offer suppress for already-listed code: {fx3:?}"
        );
    }

    #[test]
    fn suppress_fix_errors_and_dedupes() {
        // Error diagnostics get the same Suppress action as warnings.
        let src_err = "Weapon W\n  ScaleWeaponSpeed = Maybe\nEnd\n";
        let fx_err = all_fixes(src_err);
        let has_suppress_for_error = fx_err
            .iter()
            .any(|f| f.title.contains("Suppress") && f.title.contains("bad-bool"));
        assert!(has_suppress_for_error, "error missing suppress: {fx_err:?}");

        // Two unresolved references with the same code → only one Suppress action.
        let src_dup = "Weapon W\n  FireFX = NoSuchFX\n  FireFX = NoSuchFX2\nEnd\n";
        let fx_dup = all_fixes(src_dup);
        let suppress_count = fx_dup
            .iter()
            .filter(|f| f.title.contains("Suppress") && f.title.contains("unresolved-reference"))
            .count();
        assert_eq!(suppress_count, 1, "duplicate suppress actions: {fx_dup:?}");
    }

    #[test]
    fn fixes_ignore_diags_outside_range() {
        // The diagnostic is at EOF; the range covers only the first byte.
        // No diagnostic fix should be generated.
        let src = "Weapon MyWeapon\n  FireFX = NoSuchFX\nEnd\n";
        let a = Analyzer::embedded();
        let parse = a.parse(src);
        let mut idx = WorkspaceIndex::new();
        idx.set_file("test.ini", definitions_in(&a, &parse, "test.ini"));
        let diags = diagnose(&a, &parse, Some(&idx), Some("test.ini"));
        // Range = first byte only (nowhere near the unresolved-reference token).
        let fx = fixes(&a, &parse, src, Span::new(0, 1), &diags, Some(&idx));
        assert!(
            !fx.iter()
                .any(|f| f.title.contains("Suppress") || f.title.contains("Create stub")),
            "diagnostic fixes should be out of range: {fx:?}"
        );
    }
}
