# Phase 5 batch 3: module enum/bitflag value sets. Extracts the engine's
# s_bitNameList / name-array tables and retypes every module field whose
# parse fn consumes them, replacing `unknown{parse_fn}` with real
# bit_flags/enum types so members validate and complete:
#   ObjectStatusMaskType::parseFromINI -> bit_flags object_status
#   ModelConditionFlags::parseFromINI  -> bit_flags model_condition
#   INI::parseDeathTypeFlags           -> bit_flags death_type   (ALL/NONE/+x/-x)
#   INI::parseVeterancyLevelFlags      -> bit_flags veterancy_level
#   INI::parseDamageTypeFlags          -> bit_flags damage_type
#   INI::parseStaticGameLODLevel       -> enum static_game_lod
#   parseAsciiStringLC                 -> ascii_string (engine lowercases)
# The diagnostics layer already accepts bare members, NONE/ALL, and +/-
# prefixes for bit_flags, so these retypes are engine-faithful or leniently
# wider, never stricter than the engine. Re-runnable; schema.json is the
# source of truth.
import json
import pathlib
import re

root = pathlib.Path(__file__).resolve().parents[1]
schema_path = root / "crates/schema/schema.json"
schema = json.loads(schema_path.read_text(encoding="utf-8"))
set_ids = {v["id"] for v in schema["value_sets"]}


def names_from(rel_path, array_name):
    """Collect quoted member names of `const char* X[] = { ... };`,
    dropping regions guarded by #ifdef's that the retail build leaves
    undefined (ALLOW_SURRENDER)."""
    text = (root / rel_path).read_text(encoding="utf-8", errors="replace")
    text = re.sub(r"#ifdef ALLOW_SURRENDER.*?#endif", "", text, flags=re.S)
    m = re.search(re.escape(array_name) + r"\[\]\s*=\s*\{(.*?)\};", text, re.S)
    assert m, f"{array_name} not found in {rel_path}"
    names = re.findall(r'"([A-Za-z0-9_]+)"', m.group(1))
    assert names, f"{array_name} matched but no members parsed"
    return names


def add_set(set_id, names, doc=None):
    if set_id in set_ids:
        return
    entry = {"id": set_id, "members": [{"name": n, "value": i} for i, n in enumerate(names)]}
    if doc:
        entry["doc"] = doc
    schema["value_sets"].append(entry)
    set_ids.add(set_id)


# --- value sets from engine name tables ---------------------------------------
add_set(
    "object_status",
    names_from(
        "GeneralsCode/Core/GameEngine/Source/Common/System/ObjectStatusTypes.cpp",
        "ObjectStatusMaskType::s_bitNameList",
    ),
    doc="ObjectStatusTypes — runtime status bits objects can carry/grant",
)
add_set(
    "model_condition",
    names_from(
        "GeneralsCode/GameEngine/Source/Common/BitFlags.cpp",
        "ModelConditionFlags::s_bitNameList",
    ),
    doc="ModelConditionFlagType — drives ConditionState selection in draw modules",
)
add_set(
    "static_game_lod",
    names_from("GeneralsCode/GameEngine/Source/Common/GameLOD.cpp", "StaticGameLODNames"),
    doc="StaticGameLODLevel (GameLOD.cpp)",
)

# --- retype fields by their engine parse fn -----------------------------------
RETYPE = {
    "ObjectStatusMaskType::parseFromINI": {"kind": "bit_flags", "value_set": "object_status"},
    "ModelConditionFlags::parseFromINI": {"kind": "bit_flags", "value_set": "model_condition"},
    "INI::parseDeathTypeFlags": {"kind": "bit_flags", "value_set": "death_type"},
    "INI::parseVeterancyLevelFlags": {"kind": "bit_flags", "value_set": "veterancy_level"},
    "INI::parseDamageTypeFlags": {"kind": "bit_flags", "value_set": "damage_type"},
    "INI::parseStaticGameLODLevel": {"kind": "enum", "value_set": "static_game_lod"},
    "parseAsciiStringLC": {"kind": "ascii_string"},
}


def retype_fields(item, counts):
    for f in item.get("fields", []):
        vt = f.get("value_type", {})
        if vt.get("kind") == "unknown" and vt.get("parse_fn") in RETYPE:
            fn = vt["parse_fn"]
            f["value_type"] = dict(RETYPE[fn])
            f["parse_fn"] = f.get("parse_fn") or fn
            counts[fn] = counts.get(fn, 0) + 1
    for sb in item.get("sub_blocks", []) or []:
        retype_fields(sb, counts)


counts = {}
for coll in ("blocks", "modules"):
    for it in schema[coll]:
        retype_fields(it, counts)

schema_path.write_text(json.dumps(schema, indent=2) + "\n", encoding="utf-8")
for fn, n in sorted(counts.items()):
    print(f"{n:4} {fn}")
print(f"value sets now: {len(schema['value_sets'])}")
