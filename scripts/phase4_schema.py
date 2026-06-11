# Phase 4 schema edits: TokenList for Armor coefficients, WeaponSet/ArmorSet
# sub-block fields, MultiplayerColor + FXList Tracer field typing, value sets.
# One-shot helper; the committed schema.json remains the source of truth.
import json
import pathlib

path = pathlib.Path(__file__).resolve().parents[1] / "crates/schema/schema.json"
schema = json.loads(path.read_text(encoding="utf-8"))

schema["format_version"] = 2

blocks = {b["name"]: b for b in schema["blocks"]}
sets = {v["id"]: v for v in schema["value_sets"]}


def field(name, vt, parse_fn="", doc=None):
    return {"name": name, "value_type": vt, "parse_fn": parse_fn, "doc": doc}


def members(names):
    return [{"name": n, "value": i} for i, n in enumerate(names)]


# --- value sets -------------------------------------------------------------
# Armor damage types: the engine's damage_type set plus the special `Default`
# row that sets every coefficient (Armor.cpp parseArmorCoefficients).
armor_dt = [{"name": "Default", "value": -1}] + [
    dict(m) for m in sets["damage_type"]["members"]
]
schema["value_sets"].append({"id": "armor_damage_type", "members": armor_dt})

# WeaponSet.cpp WeaponSetFlags::s_bitNameList
schema["value_sets"].append({
    "id": "weapon_set_conditions",
    "members": members([
        "VETERAN", "ELITE", "HERO", "PLAYER_UPGRADE", "CRATEUPGRADE_ONE",
        "CRATEUPGRADE_TWO", "VEHICLE_HIJACK", "CARBOMB",
        "MINE_CLEARING_DETAIL", "WEAPON_RIDER1", "WEAPON_RIDER2",
        "WEAPON_RIDER3", "WEAPON_RIDER4", "WEAPON_RIDER5", "WEAPON_RIDER6",
        "WEAPON_RIDER7", "WEAPON_RIDER8",
    ]),
})

# BitFlags.cpp ArmorSetFlags::s_bitNameList
schema["value_sets"].append({
    "id": "armor_set_conditions",
    "members": members([
        "VETERAN", "ELITE", "HERO", "PLAYER_UPGRADE",
        "WEAK_VERSUS_BASEDEFENSES", "SECOND_LIFE", "CRATE_UPGRADE_ONE",
        "CRATE_UPGRADE_TWO",
    ]),
})

# WeaponSet.h TheWeaponSlotTypeNames
schema["value_sets"].append({
    "id": "weapon_slot",
    "members": members(["PRIMARY", "SECONDARY", "TERTIARY"]),
})

# --- Armor coefficients -----------------------------------------------------
armor_field = next(f for f in blocks["Armor"]["fields"] if f["name"] == "Armor")
armor_field["value_type"] = {
    "kind": "token_list",
    "tokens": [
        {"kind": "enum", "value_set": "armor_damage_type"},
        {"kind": "percent"},
    ],
}

# --- WeaponSet / ArmorSet sub-blocks on Object and ObjectReskin --------------
weapon_set = {
    "keyword": "WeaponSet",
    "doc": "A conditional weapon loadout (WeaponSet.cpp parseWeaponTemplateSet).",
    "fields": [
        field("Conditions",
              {"kind": "bit_flags", "value_set": "weapon_set_conditions"},
              "WeaponSetFlags::parseFromINI"),
        field("Weapon",
              {"kind": "token_list", "tokens": [
                  {"kind": "enum", "value_set": "weapon_slot"},
                  {"kind": "reference", "ref_kind": "weapon"},
              ]},
              "WeaponTemplateSet::parseWeapon"),
        field("AutoChooseSources",
              {"kind": "unknown",
               "parse_fn": "WeaponTemplateSet::parseAutoChoose"},
              "WeaponTemplateSet::parseAutoChoose"),
        field("PreferredAgainst",
              {"kind": "unknown",
               "parse_fn": "WeaponTemplateSet::parsePreferredAgainst"},
              "WeaponTemplateSet::parsePreferredAgainst"),
        field("ShareWeaponReloadTime", {"kind": "bool"}, "INI::parseBool"),
        field("WeaponLockSharedAcrossSets", {"kind": "bool"}, "INI::parseBool"),
    ],
    "sub_blocks": [],
}
armor_set = {
    "keyword": "ArmorSet",
    "doc": "A conditional armor selection (ThingTemplate.cpp parseArmorTemplateSet).",
    "fields": [
        field("Conditions",
              {"kind": "bit_flags", "value_set": "armor_set_conditions"},
              "ArmorSetFlags::parseFromINI"),
        field("Armor", {"kind": "reference", "ref_kind": "armor"},
              "ArmorStore::parseArmorTemplate"),
        field("DamageFX", {"kind": "reference", "ref_kind": "damage_fx"},
              "DamageFXStore::parseDamageFX"),
    ],
    "sub_blocks": [],
}
for name in ("Object", "ObjectReskin"):
    blocks[name]["sub_blocks"].extend(
        [json.loads(json.dumps(weapon_set)), json.loads(json.dumps(armor_set))]
    )

# --- MultiplayerColor (MultiplayerSettings.cpp m_colorFieldParseTable) -------
blocks["MultiplayerColor"]["fields"] = [
    field("TooltipName", {"kind": "ascii_string"}, "INI::parseAsciiString"),
    field("RGBColor", {"kind": "color"}, "INI::parseRGBColor"),
    field("RGBNightColor", {"kind": "color"}, "INI::parseRGBColor"),
]

# --- FXList Tracer nugget (FXList.cpp TracerFXNugget) ------------------------
tracer = next(s for s in blocks["FXList"]["sub_blocks"] if s["keyword"] == "Tracer")
tracer["fields"] = [
    field("TracerName", {"kind": "ascii_string"}, "INI::parseAsciiString"),
    field("BoneName", {"kind": "ascii_string"}, "INI::parseAsciiString"),
    field("Speed", {"kind": "velocity"}, "INI::parseVelocityReal"),
    field("DecayAt", {"kind": "real"}, "INI::parseReal"),
    field("Length", {"kind": "real"}, "INI::parseReal"),
    field("Width", {"kind": "real"}, "INI::parseReal"),
    field("Color", {"kind": "color"}, "INI::parseRGBColor"),
    field("Probability", {"kind": "real"}, "INI::parseReal"),
]

# --- Weapon.ScatterTarget (Weapon.cpp parseScatterTarget -> parseCoord2D) ----
weapon = blocks["Weapon"]
if not any(f["name"] == "ScatterTarget" for f in weapon["fields"]):
    weapon["fields"].append(
        field("ScatterTarget", {"kind": "coord2_d"},
              "WeaponTemplate::parseScatterTarget",
              "A scatter offset (repeatable); X:/Y: coordinate.")
    )

path.write_text(json.dumps(schema, indent=2) + "\n", encoding="utf-8")
print("schema.json updated")
