# Phase 5 batch 3: reference-type the fields that name other definitions, so
# the index drives completion and unresolved checks for them (the
# `UpgradeCameo1` treatment, applied everywhere it is engine-faithful):
# images, FX lists, OCLs, particle systems, audio events, command buttons.
import json
import pathlib

root = pathlib.Path(__file__).resolve().parents[1]
schema_path = root / "crates/schema/schema.json"
schema = json.loads(schema_path.read_text(encoding="utf-8"))
blocks = {b["name"]: b for b in schema["blocks"]}
set_ids = {v["id"] for v in schema["value_sets"]}

schema["format_version"] = 3


def ref(kind):
    return {"kind": "reference", "ref_kind": kind}


def vet_ref(kind):
    # `Veterancy* = <level> <name>` (Weapon.cpp parsePerVetLevel*).
    return {"kind": "token_list", "tokens": [
        {"kind": "enum", "value_set": "veterancy_level"},
        ref(kind),
    ]}


def retype(block, mapping):
    for f in blocks[block]["fields"]:
        if f["name"] in mapping:
            f["value_type"] = mapping.pop(f["name"])
    assert not mapping, f"{block}: fields not found: {list(mapping)}"


# GameCommon.cpp TheVeterancyNames
if "veterancy_level" not in set_ids:
    schema["value_sets"].append({
        "id": "veterancy_level",
        "members": [{"name": n, "value": i}
                    for i, n in enumerate(["REGULAR", "VETERAN", "ELITE", "HEROIC"])],
    })

# Audio events are defined by three block types sharing one namespace.
for name in ("AudioEvent", "DialogEvent", "MusicTrack"):
    blocks[name]["defines"] = "audio_event"

# --- Weapon: FX / OCL / particle-system / sound references -------------------
retype("Weapon", {
    "FireFX": ref("fx_list"),
    "ProjectileDetonationFX": ref("fx_list"),
    "FireOCL": ref("object_creation_list"),
    "ProjectileDetonationOCL": ref("object_creation_list"),
    "ProjectileExhaust": ref("particle_system"),
    "VeterancyFireFX": vet_ref("fx_list"),
    "VeterancyProjectileDetonationFX": vet_ref("fx_list"),
    "VeterancyFireOCL": vet_ref("object_creation_list"),
    "VeterancyProjectileDetonationOCL": vet_ref("object_creation_list"),
    "VeterancyProjectileExhaust": vet_ref("particle_system"),
    "FireSound": ref("audio_event"),
})

# --- Object / ObjectReskin: images + every Voice*/Sound* audio event ---------
obj_map = {"ButtonImage": ref("mapped_image"), "SelectPortrait": ref("mapped_image")}
for f in blocks["Object"]["fields"]:
    if f["parse_fn"] == "INI::parseDynamicAudioEventRTS":
        obj_map[f["name"]] = ref("audio_event")
retype("Object", dict(obj_map))
blocks["ObjectReskin"]["fields"] = json.loads(json.dumps(blocks["Object"]["fields"]))

# --- Upgrade: research sound ---------------------------------------------------
for f in blocks["Upgrade"]["fields"]:
    if f["name"] in ("ResearchSound", "UnitSpecificSound"):
        f["value_type"] = ref("audio_event")

# --- CommandButton: the full ControlBar.cpp table ------------------------------
AS = {"kind": "ascii_string"}
blocks["CommandButton"]["fields"] = [
    {"name": n, "value_type": vt, "parse_fn": fn, "doc": None}
    for n, vt, fn in [
        ("Command", {"kind": "unknown", "parse_fn": "CommandButton::parseCommand"}, "CommandButton::parseCommand"),
        ("Options", {"kind": "unknown", "parse_fn": "INI::parseBitString32"}, "INI::parseBitString32"),
        ("Object", ref("object"), "INI::parseThingTemplate"),
        ("Upgrade", ref("upgrade"), "INI::parseUpgradeTemplate"),
        ("WeaponSlot", {"kind": "enum", "value_set": "weapon_slot"}, "INI::parseLookupList"),
        ("MaxShotsToFire", {"kind": "int"}, "INI::parseInt"),
        ("Science", {"kind": "unknown", "parse_fn": "INI::parseScienceVector"}, "INI::parseScienceVector"),
        ("SpecialPower", ref("special_power"), "INI::parseSpecialPowerTemplate"),
        ("TextLabel", AS, "INI::parseAsciiString"),
        ("DescriptLabel", AS, "INI::parseAsciiString"),
        ("PurchasedLabel", AS, "INI::parseAsciiString"),
        ("ConflictingLabel", AS, "INI::parseAsciiString"),
        ("ButtonImage", ref("mapped_image"), "INI::parseAsciiString"),
        ("CursorName", AS, "INI::parseAsciiString"),
        ("InvalidCursorName", AS, "INI::parseAsciiString"),
        ("ButtonBorderType", {"kind": "unknown", "parse_fn": "INI::parseLookupList"}, "INI::parseLookupList"),
        ("RadiusCursorType", {"kind": "unknown", "parse_fn": "INI::parseIndexList"}, "INI::parseIndexList"),
        ("UnitSpecificSound", ref("audio_event"), "INI::parseAudioEventRTS"),
    ]
]

# --- CommandSet: numbered slots reference command buttons ----------------------
blocks["CommandSet"]["fields"] = [
    {"name": str(i), "value_type": ref("command_button"),
     "parse_fn": "CommandSet::parseCommandButton", "doc": None}
    for i in range(1, 19)
]

schema_path.write_text(json.dumps(schema, indent=2) + "\n", encoding="utf-8")
print("reference typing applied")
