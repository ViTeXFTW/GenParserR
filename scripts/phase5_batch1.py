# Phase 5 batch 1: complete the Object (ThingTemplate) field table, extract
# its enum/bitflag name arrays from the engine, and diff-add Weapon's missing
# fields from Weapon.cpp (unknown{parse_fn} fallback keeps everything new
# lenient). One-shot helper; schema.json stays the source of truth.
import json
import pathlib
import re

root = pathlib.Path(__file__).resolve().parents[1]
schema_path = root / "crates/schema/schema.json"
schema = json.loads(schema_path.read_text(encoding="utf-8"))
blocks = {b["name"]: b for b in schema["blocks"]}
set_ids = {v["id"] for v in schema["value_sets"]}


def names_from(rel_path, array_name):
    """Collect the quoted strings of `static const char* X[] = { ... }`."""
    text = (root / rel_path).read_text(encoding="utf-8", errors="replace")
    m = re.search(re.escape(array_name) + r"\[\]\s*=\s*\{(.*?)\};", text, re.S)
    assert m, f"{array_name} not found in {rel_path}"
    return re.findall(r'"([A-Za-z0-9_]+)"', m.group(1))


def add_set(set_id, names):
    if set_id in set_ids:
        return
    schema["value_sets"].append(
        {"id": set_id, "members": [{"name": n, "value": i} for i, n in enumerate(names)]}
    )
    set_ids.add(set_id)


def field(name, vt, parse_fn="", doc=None):
    return {"name": name, "value_type": vt, "parse_fn": parse_fn, "doc": doc}


# --- value sets from engine name arrays --------------------------------------
add_set("shadow_type", names_from("GeneralsCode/GameEngine/Include/GameClient/Shadow.h", "TheShadowNames"))
add_set("radar_priority", names_from("GeneralsCode/Core/GameEngine/Include/Common/Radar.h", "RadarPriorityNames"))
add_set("geometry_type", names_from("GeneralsCode/GameEngine/Include/Common/Geometry.h", "GeometryNames"))
add_set("build_completion", names_from("GeneralsCode/GameEngine/Include/Common/ThingTemplate.h", "BuildCompletionNames"))
add_set("buildable_status", names_from("GeneralsCode/GameEngine/Include/Common/ThingTemplate.h", "BuildableStatusNames"))
add_set("editor_sorting", names_from("GeneralsCode/GameEngine/Include/Common/ThingSort.h", "EditorSortingNames"))
add_set("kind_of", names_from("GeneralsCode/GameEngine/Source/Common/System/KindOf.cpp", "KindOfMaskType::s_bitNameList"))
add_set("locomotor_set", names_from("GeneralsCode/GameEngine/Include/GameLogic/Module/AIUpdate.h", "TheLocomotorSetNames"))

# --- Object: complete s_objectFieldParseTable (ThingTemplate.cpp) ------------
AUDIO = "INI::parseDynamicAudioEventRTS"
voice_sound = [
    "VoiceSelect", "VoiceGroupSelect", "VoiceMove", "VoiceAttack", "VoiceEnter",
    "VoiceFear", "VoiceSelectElite", "VoiceCreated", "VoiceTaskUnable",
    "VoiceTaskComplete", "VoiceMeetEnemy", "VoiceGarrison", "VoiceDefect",
    "VoiceAttackSpecial", "VoiceAttackAir", "VoiceGuard",
    "SoundMoveStart", "SoundMoveStartDamaged", "SoundMoveLoop",
    "SoundMoveLoopDamaged", "SoundAmbient", "SoundAmbientDamaged",
    "SoundAmbientReallyDamaged", "SoundAmbientRubble", "SoundStealthOn",
    "SoundStealthOff", "SoundCreated", "SoundOnDamaged", "SoundOnReallyDamaged",
    "SoundEnter", "SoundExit", "SoundPromotedVeteran", "SoundPromotedElite",
    "SoundPromotedHero", "SoundFallingFromPlane",
]
object_new = [
    field("RadarPriority", {"kind": "enum", "value_set": "radar_priority"}, "INI::parseByteSizedIndexList"),
    field("SkillPointValue", {"kind": "unknown", "parse_fn": "ThingTemplate::parseIntList"}, "ThingTemplate::parseIntList"),
    field("ExperienceValue", {"kind": "unknown", "parse_fn": "ThingTemplate::parseIntList"}, "ThingTemplate::parseIntList"),
    field("ExperienceRequired", {"kind": "unknown", "parse_fn": "ThingTemplate::parseIntList"}, "ThingTemplate::parseIntList"),
    field("Buildable", {"kind": "enum", "value_set": "buildable_status"}, "INI::parseByteSizedIndexList"),
    field("BuildCompletion", {"kind": "enum", "value_set": "build_completion"}, "INI::parseByteSizedIndexList"),
    field("DisplayColor", {"kind": "color"}, "INI::parseColorInt"),
    field("EditorSorting", {"kind": "enum", "value_set": "editor_sorting"}, "INI::parseByteSizedIndexList"),
    field("Geometry", {"kind": "enum", "value_set": "geometry_type"}, "GeometryInfo::parseGeometryType"),
    field("GeometryMajorRadius", {"kind": "real"}, "GeometryInfo::parseGeometryMajorRadius"),
    field("GeometryMinorRadius", {"kind": "real"}, "GeometryInfo::parseGeometryMinorRadius"),
    field("GeometryHeight", {"kind": "real"}, "GeometryInfo::parseGeometryHeight"),
    field("GeometryIsSmall", {"kind": "bool"}, "GeometryInfo::parseGeometryIsSmall"),
    field("Shadow", {"kind": "bit_flags", "value_set": "shadow_type"}, "INI::parseBitString8"),
    field("MaxSimultaneousOfType", {"kind": "unknown", "parse_fn": "ThingTemplate::parseMaxSimultaneous"}, "ThingTemplate::parseMaxSimultaneous"),
    field("MaxSimultaneousLinkKey", {"kind": "ascii_string"}, "NameKeyGenerator::parseStringAsNameKeyType"),
] + [field(n, {"kind": "ascii_string"}, AUDIO) for n in voice_sound]

obj = blocks["Object"]
existing = {f["name"] for f in obj["fields"]}
added_obj = [f for f in object_new if f["name"] not in existing]
obj["fields"].extend(added_obj)

# Upgrade two existing fields now that their sets exist.
for f in obj["fields"]:
    if f["name"] == "KindOf":
        f["value_type"] = {"kind": "bit_flags", "value_set": "kind_of"}
        f["parse_fn"] = "KindOfMaskType::parseFromINI"
    if f["name"] == "Locomotor":
        # `Locomotor = <set> <locomotor name> [more names...]`
        f["value_type"] = {"kind": "token_list", "tokens": [
            {"kind": "enum", "value_set": "locomotor_set"},
            {"kind": "reference", "ref_kind": "locomotor"},
        ]}
        f["parse_fn"] = "AIUpdateModuleData::parseLocomotorSet"

# ObjectReskin mirrors Object's body (the engine keeps both tables in tandem).
reskin = blocks["ObjectReskin"]
reskin["fields"] = json.loads(json.dumps(obj["fields"]))

# --- Weapon: diff-add from Weapon.cpp's field table ---------------------------
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
}

weapon_cpp = (root / "GeneralsCode/GameEngine/Source/GameLogic/Object/Weapon.cpp").read_text(
    encoding="utf-8", errors="replace"
)
tables = re.findall(r"FieldParse\s+[\w:]+\[\]\s*=\s*\{(.*?)\n\};", weapon_cpp, re.S)
table = next(t for t in tables if '"PrimaryDamage"' in t)
entries = re.findall(r'\{\s*"(\w+)"\s*,\s*([\w:]+)\s*,', table)

weapon = blocks["Weapon"]
wexisting = {f["name"] for f in weapon["fields"]}
added_wpn = []
for name, fn in entries:
    if name in wexisting:
        continue
    vt = FN_MAP.get(fn, {"kind": "unknown", "parse_fn": fn})
    added_wpn.append(field(name, vt, fn))
weapon["fields"].extend(added_wpn)

schema_path.write_text(json.dumps(schema, indent=2) + "\n", encoding="utf-8")
print(f"Object: +{len(added_obj)} fields (now {len(obj['fields'])}); "
      f"Weapon: +{len(added_wpn)} fields (now {len(weapon['fields'])}): "
      f"{[f['name'] for f in added_wpn]}")
