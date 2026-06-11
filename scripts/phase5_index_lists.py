# Phase 5 batch 3b: type the remaining INI::parseIndexList fields. Each such
# FieldParse entry carries its name array as userData (single-token enum, not
# flags); this script extracts each array from the engine and retypes the
# (owner, field) pair to `enum` over a matching value set. Targets are matched
# by owner + field name + parse fn, so re-running is a no-op. Engine sources
# verified per entry (file:line of the FieldParse entry in the comment).
import json
import pathlib
import re

root = pathlib.Path(__file__).resolve().parents[1]
schema_path = root / "crates/schema/schema.json"
schema = json.loads(schema_path.read_text(encoding="utf-8"))
set_ids = {v["id"] for v in schema["value_sets"]}


def names_from(rel_path, array_name):
    text = (root / rel_path).read_text(encoding="utf-8", errors="replace")
    text = re.sub(r"#ifdef ALLOW_SURRENDER.*?#endif", "", text, flags=re.S)
    m = re.search(re.escape(array_name) + r"\[\]\s*=\s*\{(.*?)\};", text, re.S)
    assert m, f"{array_name} not found in {rel_path}"
    names = re.findall(r'"([A-Za-z0-9_]+)"', m.group(1))
    assert names, f"{array_name} matched but no members parsed"
    return names


def add_set(set_id, rel_path, array_name):
    if set_id in set_ids:
        return
    names = names_from(rel_path, array_name)
    schema["value_sets"].append(
        {
            "id": set_id,
            "members": [{"name": n, "value": i} for i, n in enumerate(names)],
            "doc": f"{array_name} ({pathlib.PurePosixPath(rel_path).name})",
        }
    )
    set_ids.add(set_id)


GE = "GeneralsCode/GameEngine"
add_set("academy_classify", f"{GE}/Source/Common/RTS/AcademyStats.cpp", "TheAcademyClassificationTypeNames")
add_set("radius_cursor", f"{GE}/Include/GameClient/InGameUI.h", "TheRadiusCursorNames")
add_set("locomotor_appearance", f"{GE}/Include/GameLogic/Locomotor.h", "TheLocomotorAppearanceNames")
add_set("locomotor_z_behavior", f"{GE}/Include/GameLogic/Locomotor.h", "TheLocomotorBehaviorZNames")
add_set("locomotor_priority", f"{GE}/Source/GameLogic/Object/Locomotor.cpp", "TheLocomotorPriorityNames")
add_set("max_health_change", f"{GE}/Include/GameLogic/Module/BodyModule.h", "TheMaxHealthChangeTypeNames")
add_set("horde_action", f"{GE}/Include/GameLogic/Module/HordeUpdate.h", "TheHordeActionTypeNames")
add_set("weapon_reload", f"{GE}/Include/GameLogic/Weapon.h", "TheWeaponReloadNames")
add_set("weapon_prefire", f"{GE}/Include/GameLogic/Weapon.h", "TheWeaponPrefireNames")
add_set("weapon_bonus_condition", f"{GE}/Include/GameLogic/Weapon.h", "TheWeaponBonusNames")
add_set("ocl_create_location", f"{GE}/Source/GameLogic/Object/SpecialPower/OCLSpecialPower.cpp", "TheOCLCreateLocTypeNames")
add_set("gunship_create_location", f"{GE}/Source/GameLogic/Object/Update/SpectreGunshipDeploymentUpdate.cpp", "TheGunshipCreateLocTypeNames")

# (owner, field) -> value set; owner is a block name or module name.
TARGETS = {
    ("CommandButton", "RadiusCursorType"): "radius_cursor",  # ControlBar.cpp:119
    ("Locomotor", "ZAxisBehavior"): "locomotor_z_behavior",  # Locomotor.cpp:458
    ("Locomotor", "Appearance"): "locomotor_appearance",  # Locomotor.cpp:459
    ("Locomotor", "GroupMovementPriority"): "locomotor_priority",  # Locomotor.cpp:460
    ("Upgrade", "AcademyClassify"): "academy_classify",  # Upgrade.cpp:125
    ("Weapon", "AutoReloadsClip"): "weapon_reload",  # Weapon.cpp:221
    ("Weapon", "PreAttackType"): "weapon_prefire",  # Weapon.cpp:237
    ("BattlePlanUpdate", "StrategyCenterHoldTheLineMaxHealthChangeType"): "max_health_change",  # BattlePlanUpdate.cpp:122
    ("HordeUpdate", "Action"): "horde_action",  # HordeUpdate.cpp:142
    ("JetAIUpdate", "AttackLocomotorType"): "locomotor_set",  # JetAIUpdate.cpp:1818
    ("JetAIUpdate", "ReturnForAmmoLocomotorType"): "locomotor_set",  # JetAIUpdate.cpp:1821
    ("MaxHealthUpgrade", "ChangeType"): "max_health_change",  # MaxHealthUpgrade.cpp:58
    ("OCLSpecialPower", "CreateLocation"): "ocl_create_location",  # OCLSpecialPower.cpp:99
    ("ParticleUplinkCannonUpdate", "DeathType"): "death_type",  # ParticleUplinkCannonUpdate.cpp:144
    ("RiderChangeContain", "ScuttleStatus"): "model_condition",  # RiderChangeContain.cpp:111 (getBitNames)
    ("SpectreGunshipDeploymentUpdate", "CreateLocation"): "gunship_create_location",  # SpectreGunshipDeploymentUpdate.cpp:102
    ("VeterancyGainCreate", "StartingLevel"): "veterancy_level",  # VeterancyGainCreate.cpp:56
    ("WeaponBonusUpdate", "BonusConditionType"): "weapon_bonus_condition",  # WeaponBonusUpdate.cpp:87
}

done = set()
for coll in ("blocks", "modules"):
    for it in schema[coll]:
        owner = it.get("name", "")
        for f in it.get("fields", []):
            key = (owner, f["name"])
            set_id = TARGETS.get(key)
            if not set_id:
                continue
            vt = f.get("value_type", {})
            if vt.get("kind") == "unknown" and vt.get("parse_fn") == "INI::parseIndexList":
                f["value_type"] = {"kind": "enum", "value_set": set_id}
                f["parse_fn"] = f.get("parse_fn") or "INI::parseIndexList"
            assert set_id in set_ids, set_id
            done.add(key)

missing = set(TARGETS) - done
assert not missing, f"targets not found in schema: {sorted(missing)}"
schema_path.write_text(json.dumps(schema, indent=2) + "\n", encoding="utf-8")
print(f"retyped {len(done)} fields; value sets now {len(schema['value_sets'])}")
