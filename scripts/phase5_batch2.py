# Phase 5 batch 2: diff-add the Locomotor and Upgrade field tables from the
# engine source (auto-translated; unknown{parse_fn} fallback stays lenient).
import json
import pathlib
import re

root = pathlib.Path(__file__).resolve().parents[1]
schema_path = root / "crates/schema/schema.json"
schema = json.loads(schema_path.read_text(encoding="utf-8"))
blocks = {b["name"]: b for b in schema["blocks"]}

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
    "INI::parseDynamicAudioEventRTS": {"kind": "ascii_string"},
    "INI::parseAudioEventRTS": {"kind": "ascii_string"},
}


def add_from_table(block_name, cpp_rel, marker):
    text = (root / cpp_rel).read_text(encoding="utf-8", errors="replace")
    tables = re.findall(r"FieldParse\s+[\w:]+(?:\[\])?\s*=?\s*\{(.*?)\n\s*\};", text, re.S)
    table = next(t for t in tables if f'"{marker}"' in t)
    entries = re.findall(r'\{\s*"(\w+)"\s*,\s*([\w:]+)\s*,', table)
    block = blocks[block_name]
    existing = {f["name"] for f in block["fields"]}
    added = []
    for name, fn in entries:
        if name in existing:
            continue
        vt = FN_MAP.get(fn, {"kind": "unknown", "parse_fn": fn})
        added.append({"name": name, "value_type": vt, "parse_fn": fn, "doc": None})
    block["fields"].extend(added)
    print(f"{block_name}: +{len(added)} fields (now {len(block['fields'])})")


add_from_table(
    "Locomotor",
    "GeneralsCode/GameEngine/Source/GameLogic/Object/Locomotor.cpp",
    "Appearance",
)
add_from_table(
    "Upgrade",
    "GeneralsCode/GameEngine/Source/Common/System/Upgrade.cpp",
    "ResearchSound",
)

schema_path.write_text(json.dumps(schema, indent=2) + "\n", encoding="utf-8")
