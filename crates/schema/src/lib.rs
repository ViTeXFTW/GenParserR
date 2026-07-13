//! Data model for the C&C Generals: Zero Hour INI schema.
//!
//! This crate defines the serde types that describe how the game engine
//! interprets INI files. The schema itself is the hand-written
//! `schema.json` (modeled on the engine's `FieldParse` tables in the bundled
//! C++ source), and is consumed by `analysis` (diagnostics, completions,
//! semantic tokens) and embedded into the language `server` binary.
//!
//! The schema is intentionally a flat, self-contained document: blocks and
//! modules reference value sets and each other by string id, so it round-trips
//! cleanly through JSON and needs no post-load fixups beyond building lookup
//! maps (see [`Schema::index`]).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The complete engine schema. Root of the hand-written embedded `schema.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schema {
    /// Schema format version (this crate's contract), bumped on breaking changes.
    pub format_version: u32,
    /// Identifier of the engine source snapshot this schema was extracted from
    /// (e.g. a git revision or tag). Lets the server report what it mirrors.
    #[serde(default)]
    pub engine_revision: String,
    /// Top-level block types, keyed by their INI keyword (e.g. `Object`, `Weapon`).
    pub blocks: Vec<BlockType>,
    /// Nested module types (e.g. `W3DModelDraw`, `ActiveBody`) that appear inside
    /// blocks under a [`ModuleSlot`].
    pub modules: Vec<ModuleType>,
    /// Named value sets used by enum / bitflag fields, keyed by id.
    pub value_sets: Vec<ValueSet>,
    /// Definitions the engine synthesizes at runtime rather than reading from
    /// INI (e.g. the `Upgrade_Veterancy_*` upgrades) — valid reference targets
    /// that exist in no file.
    #[serde(default)]
    pub builtins: Vec<BuiltinDef>,
}

/// An engine-synthesized definition (see [`Schema::builtins`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuiltinDef {
    pub ref_kind: RefKind,
    pub name: String,
    #[serde(default)]
    pub doc: Option<String>,
}

/// A top-level INI block, e.g. `Object GLATankMarauder` ... `End`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockType {
    /// The INI keyword that introduces the block (`Object`, `Weapon`, ...).
    pub name: String,
    /// The C++ parse function this block dispatches to (provenance / debugging).
    #[serde(default)]
    pub parse_fn: String,
    /// Whether the block is named (`Object Foo`) or anonymous (`GameData`).
    #[serde(default = "default_true")]
    pub named: bool,
    /// Whether the block is `End`-terminated. A few typeTable entries are
    /// single-line directives (`BenchProfile = ...`, `ReallyLowMHz = 600`)
    /// whose parse functions consume only their own line.
    #[serde(default = "default_true")]
    pub terminated: bool,
    /// The kind of symbol a named block of this type defines, for the cross-file
    /// reference index. `None` means it is not referenceable by name.
    #[serde(default)]
    pub defines: Option<RefKind>,
    /// Fields legal directly inside this block.
    pub fields: Vec<Field>,
    /// Module slots this block exposes (e.g. `Object` exposes `Draw`, `Body`,
    /// `Behavior`, ...). Empty for leaf blocks.
    #[serde(default)]
    pub module_slots: Vec<ModuleSlot>,
    /// Structural sub-block scopes the engine parses recursively inside this
    /// block via custom parse functions (e.g. `FXList`'s `ParticleSystem`
    /// nugget). These open `End`-terminated scopes only inside their declaring
    /// scope — unlike module slots they carry no module name after `=`.
    #[serde(default)]
    pub sub_blocks: Vec<SubBlock>,
    /// Optional human-readable documentation.
    #[serde(default)]
    pub doc: Option<String>,
}

/// A structural sub-block scope (see [`BlockType::sub_blocks`]). Sub-blocks
/// nest: e.g. `AIData` → `SideInfo` → `SkillSet1`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubBlock {
    /// The keyword opening the scope (`Sound`, `CreateObject`, `Mission`, ...).
    pub keyword: String,
    /// Fields legal inside this sub-block (empty = lenient, not yet modeled).
    #[serde(default)]
    pub fields: Vec<Field>,
    /// Sub-blocks nested inside this one.
    #[serde(default)]
    pub sub_blocks: Vec<SubBlock>,
    /// The scope re-enters the *parent's* field table: the engine parse
    /// function calls `ini->initFromINI(self, self->getFieldParse())` back on
    /// the enclosing template (e.g. `Object`'s `AddModule`/`ReplaceModule`/
    /// `InheritableModule`/`OverrideableByLikeKind`), so everything legal in
    /// the parent — fields, module slots, other sub-blocks — is legal here.
    /// When set, `fields`/`sub_blocks` need not be declared.
    #[serde(default)]
    pub reenters_parent: bool,
    /// Type of the header argument that follows `=` when opening this sub-block
    /// (e.g. `BitFlags { model_condition }` for ConditionState). `None` when
    /// the sub-block has no header argument or the type is not yet modeled.
    #[serde(default)]
    pub argument_type: Option<ValueType>,
    #[serde(default)]
    pub doc: Option<String>,
}

/// A slot inside a block that hosts nested modules, e.g. `Behavior = <Module> <Tag>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleSlot {
    /// The field keyword introducing the slot (`Draw`, `Body`, `Behavior`, ...).
    pub keyword: String,
    /// The module interfaces that are accepted in this slot. A module is valid
    /// here when `module.interfaces` and `slot.accepts` share at least one entry.
    /// Driven by the engine's `findModuleInterfaceMask` / interface-mask checks
    /// in `ThingTemplate::parseModuleName`.
    pub accepts: Vec<String>,
    #[serde(default)]
    pub doc: Option<String>,
}

/// A nested module definition, e.g. `Draw = W3DModelDraw ModuleTag_01` ... `End`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleType {
    /// Module name as written in INI (`W3DModelDraw`, `ActiveBody`, ...).
    pub name: String,
    /// Interfaces this module implements; must intersect a slot's `interface`.
    #[serde(default)]
    pub interfaces: Vec<String>,
    /// Fields legal inside this module.
    pub fields: Vec<Field>,
    /// Structural sub-block scopes inside this module (e.g. `W3DModelDraw`'s
    /// `ConditionState`).
    #[serde(default)]
    pub sub_blocks: Vec<SubBlock>,
    #[serde(default)]
    pub doc: Option<String>,
}

/// A single `Key = value [value...]` field within a block or module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Field {
    /// The field keyword (left-hand side of `=`).
    pub name: String,
    /// The value type, determining lexing/validation of the right-hand side.
    pub value_type: ValueType,
    /// The C++ parse function name (provenance / debugging).
    #[serde(default)]
    pub parse_fn: String,
    #[serde(default)]
    pub doc: Option<String>,
}

/// The type of a field's value, derived from its engine parse function.
///
/// The variant determines how the value tokens are validated and which
/// completions/semantic tokens apply. `value_set` ids refer to entries in
/// [`Schema::value_sets`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValueType {
    /// `Yes` / `No`.
    Bool,
    /// Signed integer.
    Int,
    /// Unsigned integer.
    UInt,
    /// Floating-point real.
    Real,
    /// Real constrained > 0.
    PositiveReal,
    /// Angle in degrees (engine converts to radians).
    AngleReal,
    /// Percentage literal (e.g. `50%`) mapped to a real.
    Percent,
    /// Duration in milliseconds (int or real ms).
    Duration,
    /// Velocity (dist/sec).
    Velocity,
    /// Acceleration (dist/sec^2).
    Acceleration,
    /// A bare (unquoted, single-token) ASCII string.
    AsciiString,
    /// A quoted ASCII string.
    QuotedString,
    /// A list of ASCII strings (variadic).
    AsciiStringList,
    /// A W3D model asset name, backed by indexed `.w3d` files.
    W3dModel,
    /// A bone, subobject, mesh, or other member of a W3D model asset.
    W3dModelMember,
    /// `R:r G:g B:b [A:a]` color.
    Color,
    /// `X:x Y:y` coordinate.
    Coord2D,
    /// `X:x Y:y Z:z` coordinate.
    Coord3D,
    /// One name drawn from a value set (an enum).
    Enum { value_set: String },
    /// One or more flag names from a value set, with optional `+`/`-` modifiers
    /// and the special `NONE` / `ALL` tokens.
    BitFlags { value_set: String },
    /// A reference to a named definition elsewhere (cross-file index target).
    Reference { ref_kind: RefKind },
    /// One or more references of the same kind (e.g. `TriggeredBy` upgrade
    /// lists, `Science` vectors): every token resolves against the index.
    ReferenceList { ref_kind: RefKind },
    /// A fixed sequence of individually-typed tokens, for engine parse
    /// functions that consume several tokens in order (e.g. Armor
    /// coefficients: `Armor = <DamageType> <percent>`, or WeaponSet's
    /// `Weapon = <slot> <weapon name>`). Each listed token is required;
    /// tokens beyond the list are ignored (lenient).
    TokenList { tokens: Vec<ValueType> },
    /// Anything we could not map precisely; treated leniently (token soup).
    /// Carries the originating parse-fn so it can be refined later.
    Unknown { parse_fn: String },
}

/// The kind of named definition a reference points at (or a block defines).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    Object,
    Weapon,
    Armor,
    DamageFx,
    FxList,
    ObjectCreationList,
    ParticleSystem,
    Locomotor,
    Upgrade,
    Science,
    CommandButton,
    CommandSet,
    SpecialPower,
    MappedImage,
    Anim2D,
    PlayerTemplate,
    /// A named audio event (`AudioEvent` / `DialogEvent` / `MusicTrack`).
    AudioEvent,
}

/// A named set of enum / bitflag members.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueSet {
    /// Stable id referenced by [`ValueType::Enum`] / [`ValueType::BitFlags`].
    pub id: String,
    /// Member name → integer value (value is informational; names drive validation).
    pub members: Vec<ValueMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueMember {
    pub name: String,
    #[serde(default)]
    pub value: i64,
}

fn default_true() -> bool {
    true
}

/// The hand-written schema JSON, embedded at compile time. This is the single
/// artifact the language server ships with — it has no runtime dependency on
/// the engine C++ source.
pub const EMBEDDED_SCHEMA_JSON: &str = include_str!("../schema.json");

/// Parse the embedded schema. Panics only if the committed `schema.json` is
/// malformed, which a build-time test guards against.
pub fn embedded() -> Schema {
    Schema::from_json(EMBEDDED_SCHEMA_JSON).expect("embedded schema.json is valid")
}

/// Indexed views over a [`Schema`] for fast lookup by name. Built once after load.
pub struct SchemaIndex<'a> {
    pub schema: &'a Schema,
    pub blocks: HashMap<&'a str, &'a BlockType>,
    pub modules: HashMap<&'a str, &'a ModuleType>,
    pub value_sets: HashMap<&'a str, &'a ValueSet>,
}

impl Schema {
    /// Parse a schema from JSON text (e.g. the embedded `schema.json`).
    pub fn from_json(text: &str) -> serde_json::Result<Self> {
        serde_json::from_str(text)
    }

    /// Serialize the schema to pretty JSON (round-trip / tooling helper).
    pub fn to_json_pretty(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// Build name → item lookup maps for O(1) resolution.
    pub fn index(&self) -> SchemaIndex<'_> {
        SchemaIndex {
            schema: self,
            blocks: self.blocks.iter().map(|b| (b.name.as_str(), b)).collect(),
            modules: self.modules.iter().map(|m| (m.name.as_str(), m)).collect(),
            value_sets: self.value_sets.iter().map(|v| (v.id.as_str(), v)).collect(),
        }
    }
}

impl<'a> SchemaIndex<'a> {
    pub fn block(&self, name: &str) -> Option<&'a BlockType> {
        self.blocks.get(name).copied()
    }

    pub fn module(&self, name: &str) -> Option<&'a ModuleType> {
        self.modules.get(name).copied()
    }

    pub fn value_set(&self, id: &str) -> Option<&'a ValueSet> {
        self.value_sets.get(id).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let schema = Schema {
            format_version: 1,
            engine_revision: "test".into(),
            blocks: vec![BlockType {
                name: "Weapon".into(),
                parse_fn: "parseWeaponTemplateDefinition".into(),
                named: true,
                terminated: true,
                defines: Some(RefKind::Weapon),
                fields: vec![Field {
                    name: "PrimaryDamage".into(),
                    value_type: ValueType::Real,
                    parse_fn: "parseReal".into(),
                    doc: None,
                }],
                module_slots: vec![],
                sub_blocks: vec![],
                doc: None,
            }],
            modules: vec![],
            value_sets: vec![],
            builtins: vec![],
        };
        let json = schema.to_json_pretty().unwrap();
        let back = Schema::from_json(&json).unwrap();
        assert_eq!(back.blocks.len(), 1);
        let idx = back.index();
        assert_eq!(
            idx.block("Weapon").unwrap().fields[0].value_type,
            ValueType::Real
        );
    }

    /// The committed, embedded schema must parse and contain core blocks. This
    /// guards `embedded()` against a malformed or stale `schema.json`.
    #[test]
    fn embedded_schema_is_valid() {
        let schema = embedded();
        let idx = schema.index();
        assert!(idx.block("Object").is_some(), "Object block missing");
        assert!(idx.block("Weapon").is_some(), "Weapon block missing");
        assert!(
            schema.modules.iter().any(|m| m.name == "ActiveBody"),
            "ActiveBody module missing"
        );
    }
}
