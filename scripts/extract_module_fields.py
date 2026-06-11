# Phase 5: fill the module skeletons' field lists from the engine's
# `<Module>ModuleData::buildFieldParse` definitions, resolving the
# ModuleData inheritance chain (each buildFieldParse first calls its base's),
# and translating parse functions to ValueTypes (unknown{parse_fn} fallback).
# Reference-typed parse functions get index-backed completion / unresolved
# checks. Re-runnable; merges into existing fields without clobbering.
#
# Known limitation: in-class method attribution picks the nearest preceding
# class/struct declaration, so a nested helper struct can steal ownership
# (e.g. GarrisonContainModuleData). Already-extracted fields persist in
# schema.json, and the corpus run referees the result either way.
import json
import pathlib
import re

root = pathlib.Path(__file__).resolve().parents[1]
schema_path = root / "crates/schema/schema.json"
schema = json.loads(schema_path.read_text(encoding="utf-8"))

REF = lambda k: {"kind": "reference", "ref_kind": k}
FN_MAP = {
    "INI::parseBool": {"kind": "bool"},
    "INI::parseInt": {"kind": "int"},
    "INI::parseUnsignedInt": {"kind": "u_int"},
    "INI::parseUnsignedShort": {"kind": "u_int"},
    "INI::parseUnsignedByte": {"kind": "u_int"},
    "INI::parseReal": {"kind": "real"},
    "INI::parsePositiveNonZeroReal": {"kind": "positive_real"},
    "INI::parseDurationReal": {"kind": "duration"},
    "INI::parseDurationUnsignedInt": {"kind": "duration"},
    "INI::parseVelocityReal": {"kind": "velocity"},
    "INI::parseAccelerationReal": {"kind": "acceleration"},
    "INI::parseAngleReal": {"kind": "angle_real"},
    "INI::parseAngularVelocityReal": {"kind": "angle_real"},
    "INI::parsePercentToReal": {"kind": "percent"},
    "INI::parseAsciiString": {"kind": "ascii_string"},
    "INI::parseQuotedAsciiString": {"kind": "quoted_string"},
    "INI::parseAsciiStringVector": {"kind": "ascii_string_list"},
    "INI::parseAsciiStringVectorAppend": {"kind": "ascii_string_list"},
    "INI::parseRGBColor": {"kind": "color"},
    "INI::parseColorInt": {"kind": "color"},
    "INI::parseRGBAColorInt": {"kind": "color"},
    "INI::parseCoord2D": {"kind": "coord2_d"},
    "INI::parseCoord3D": {"kind": "coord3_d"},
    # References: the engine resolves these names against its stores.
    "INI::parseFXList": REF("fx_list"),
    "INI::parseObjectCreationList": REF("object_creation_list"),
    "INI::parseParticleSystemTemplate": REF("particle_system"),
    "INI::parseWeaponTemplate": REF("weapon"),
    "INI::parseUpgradeTemplate": REF("upgrade"),
    "INI::parseThingTemplate": REF("object"),
    "INI::parseSpecialPowerTemplate": REF("special_power"),
    "INI::parseScience": REF("science"),
    "INI::parseDamageFX": REF("damage_fx"),
    "INI::parseArmorTemplate": REF("armor"),
    "INI::parseAudioEventRTS": REF("audio_event"),
    "INI::parseDynamicAudioEventRTS": REF("audio_event"),
    "INI::parseScienceVector": {"kind": "reference_list", "ref_kind": "science"},
    "KindOfMaskType::parseFromINI": {"kind": "bit_flags", "value_set": "kind_of"},
    "DamageTypeFlags::parseFromINI": {"kind": "bit_flags", "value_set": "damage_type"},
}


def body_at(text, brace_pos):
    """Extract the {...} body starting at brace_pos (brace matching)."""
    depth = 0
    for i in range(brace_pos, len(text)):
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
            if depth == 0:
                return text[brace_pos : i + 1]
    return text[brace_pos:]


# class name -> {"parent": str|None, "entries": [(name, fn)], "path": Path}
classes: dict[str, dict] = {}
# module class name -> base class name (e.g. WeaponSetUpgrade -> UpgradeModule),
# for modules that have no own ModuleData and use an ancestor's.
bases: dict[str, str] = {}


def register(cls, body, path):
    # Prefer the non-Core (Zero Hour) definition when both exist.
    if cls in classes and "Core" not in str(classes[cls]["path"]):
        return
    calls = re.findall(r"(\w+)::buildFieldParse\s*\(", body)
    parent = next((c for c in calls if c != cls), None)
    # Mix-in tables included via `p.add(X::getFieldParse(), ...)` (e.g.
    # DieMuxData's DeathTypes/ExemptStatus, UpgradeMuxData's TriggeredBy).
    mixins = [c for c in re.findall(r"(\w+)::getFieldParse\s*\(\)", body) if c != cls]
    # Field names can contain more than \w (e.g. `HealthRegen%PerSec`).
    entries = re.findall(r'\{\s*"([^"]+)"\s*,\s*([\w:]+)\s*,', body)
    classes[cls] = {"parent": parent, "mixins": mixins, "entries": entries, "path": path}


for path in list(root.glob("GeneralsCode/**/*.cpp")) + list(root.glob("GeneralsCode/**/*.h")):
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        continue
    # Inheritance map first: modules without their own ModuleData (no
    # buildFieldParse anywhere in their header) still need their base chain.
    for m in re.finditer(r"\bclass\s+(\w+)\s*:\s*public\s+(\w+)", text):
        bases.setdefault(m.group(1), m.group(2))
    if "buildFieldParse" not in text and "getFieldParse" not in text:
        continue
    # Out-of-line definitions: void X::buildFieldParse(...) { ... }
    for m in re.finditer(r"void\s+(\w+)::buildFieldParse\s*\([^)]*\)\s*(?:const\s*)?\{", text):
        register(m.group(1), body_at(text, m.end() - 1), path)
    # In-class definitions: static void buildFieldParse(...) { ... } — owner is
    # the nearest preceding class declaration.
    decls = [(m.start(), m.group(1))
             for m in re.finditer(r"\b(?:class|struct)\s+(\w+)[^;{]*\{", text)]
    for m in re.finditer(r"static\s+void\s+buildFieldParse\s*\([^)]*\)\s*\{", text):
        owners = [c for pos, c in decls if pos < m.start()]
        if owners:
            register(owners[-1], body_at(text, m.end() - 1), path)
    # Mix-in table providers: const FieldParse* X::getFieldParse() { ... }
    for m in re.finditer(r"FieldParse\s*\*\s*(\w+)::getFieldParse\s*\([^)]*\)\s*\{", text):
        cls = f"{m.group(1)}::getFieldParse"
        register(cls, body_at(text, m.end() - 1), path)
    # ... and their in-class form (e.g. UpgradeMuxData in UpgradeModule.h).
    for m in re.finditer(
        r"static\s+const\s+FieldParse\s*\*\s*getFieldParse\s*\([^)]*\)\s*\{", text
    ):
        owners = [c for pos, c in decls if pos < m.start()]
        if owners:
            register(f"{owners[-1]}::getFieldParse", body_at(text, m.end() - 1), path)


def chain_entries(cls, seen=None):
    """Fields of `cls` including its base chain and mix-in tables, base-first."""
    seen = seen or set()
    if cls is None or cls in seen or cls not in classes:
        return []
    seen.add(cls)
    info = classes[cls]
    out = chain_entries(info["parent"], seen)
    for mixin in info.get("mixins", []):
        out += chain_entries(f"{mixin}::getFieldParse", seen)
    return out + info["entries"]


def data_class_for(name):
    """The ModuleData class whose fields module `name` parses with: its own,
    or — when it has none — the nearest ancestor's (e.g. WeaponSetUpgrade
    uses UpgradeModuleData via `class WeaponSetUpgrade : public UpgradeModule`)."""
    cur, hops = name, 0
    while cur and hops < 12:
        for cand in (f"{cur}ModuleData", f"{cur}Data",
                     f"{cur.removesuffix('Interface')}ModuleData"):
            if cand in classes:
                return cand
        cur = bases.get(cur)
        hops += 1
    return None


filled, missing, total_fields = [], [], 0
for module in schema["modules"]:
    name = module["name"]
    cls = data_class_for(name)
    if cls is None:
        missing.append(name)
        continue
    existing = {f["name"] for f in module["fields"]}
    added = 0
    for fname, fn in chain_entries(cls):
        if fname in existing:
            continue
        existing.add(fname)
        module["fields"].append({
            "name": fname,
            "value_type": FN_MAP.get(fn, {"kind": "unknown", "parse_fn": fn}),
            "parse_fn": fn,
            "doc": None,
        })
        added += 1
    total_fields += added
    filled.append((name, added))

schema_path.write_text(json.dumps(schema, indent=2) + "\n", encoding="utf-8")
print(f"filled {len(filled)} modules with {total_fields} fields; "
      f"{len(missing)} without a located ModuleData class:")
print(", ".join(missing))
