# Phase 5: replace over-broad CURATED_SUBBLOCKS entries with context-keyed
# schema declarations. UnitSpecificSounds/UnitSpecificFX are Object scopes;
# Turret/AltTurret are scopes only inside AIUpdate-family Behavior modules
# (and plain fields inside ConditionStates - the collision this fixes).
import json
import pathlib
import re

root = pathlib.Path(__file__).resolve().parents[1]
schema_path = root / "crates/schema/schema.json"
schema = json.loads(schema_path.read_text(encoding="utf-8"))
blocks = {b["name"]: b for b in schema["blocks"]}

# --- Object / ObjectReskin: per-unit sound/FX scopes (ThingTemplate.cpp) -----
for name in ("Object", "ObjectReskin"):
    sb = blocks[name]["sub_blocks"]
    have = {s["keyword"] for s in sb}
    for kw, fn in (
        ("UnitSpecificSounds", "ThingTemplate::parsePerUnitSounds"),
        ("UnitSpecificFX", "ThingTemplate::parsePerUnitFX"),
    ):
        if kw not in have:
            sb.append({
                "keyword": kw,
                "fields": [],  # arbitrary per-unit keys; intentionally lenient
                "sub_blocks": [],
                "doc": f"Engine scope ({fn}); keys are unit-specific names.",
            })

# --- Turret / AltTurret on the AIUpdate module family ------------------------
# TurretAIData's field table, translated from TurretAI.cpp.
aiupdate = (root / "GeneralsCode/GameEngine/Source/GameLogic/AI/TurretAI.cpp").read_text(
    encoding="utf-8", errors="replace"
)
tables = re.findall(r"FieldParse\s+[\w:]+(?:\[\])?\s*=?\s*\{(.*?)\n\s*\};", aiupdate, re.S)
turret_table = next(t for t in tables if '"TurretTurnRate"' in t)
FN_MAP = {
    "INI::parseBool": {"kind": "bool"},
    "INI::parseInt": {"kind": "int"},
    "INI::parseUnsignedInt": {"kind": "uint"},
    "INI::parseReal": {"kind": "real"},
    "INI::parseAngleReal": {"kind": "angle_real"},
    "INI::parseAngularVelocityReal": {"kind": "angle_real"},
    "INI::parseDurationReal": {"kind": "duration"},
    "INI::parseDurationUnsignedInt": {"kind": "duration"},
    "INI::parsePercentToReal": {"kind": "percent"},
}
turret_fields = [
    {"name": n, "value_type": FN_MAP.get(fn, {"kind": "unknown", "parse_fn": fn}),
     "parse_fn": fn, "doc": None}
    for n, fn in re.findall(r'\{\s*"(\w+)"\s*,\s*([\w:]+)\s*,', turret_table)
]


def turret_sub(kw):
    return {
        "keyword": kw,
        "fields": json.loads(json.dumps(turret_fields)),
        "sub_blocks": [],
        "doc": "Turret data scope (AIUpdateModuleData::parseTurret).",
    }


count = 0
for m in schema["modules"]:
    if m["name"] == "AIUpdateInterface" or m["name"].endswith("AIUpdate"):
        have = {s["keyword"] for s in m["sub_blocks"]}
        for kw in ("Turret", "AltTurret"):
            if kw not in have:
                m["sub_blocks"].append(turret_sub(kw))
        count += 1

schema_path.write_text(json.dumps(schema, indent=2) + "\n", encoding="utf-8")
print(f"Turret/AltTurret declared on {count} AIUpdate-family modules; "
      f"{len(turret_fields)} turret fields")
