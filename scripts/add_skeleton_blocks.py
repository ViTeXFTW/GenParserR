# SPDX-License-Identifier: GPL-3.0-or-later
"""One-shot helper: add structural skeleton blocks for every entry of the
engine's INI typeTable (INI.cpp theTypeTable) to crates/schema/schema.json.

Skeletons carry no field schema (lenient validation) but do carry the
sub-block structure needed by the OpenerOracle, hand-checked against the
engine's custom parse functions. Existing blocks are left untouched.
Idempotent; review the diff after running.
"""
import json
import pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
SCHEMA = ROOT / "crates/schema/schema.json"


def sb(keyword, *nested):
    out = {"keyword": keyword}
    if nested:
        out["sub_blocks"] = list(nested)
    return out


# (name, parse_fn, named, defines, sub_blocks)
# Sub-block structure sources (engine):
#   FXList.cpp:772-779, ObjectCreationList.cpp:1517-1522 (+ DeliveryDecal in
#   DeliverPayloadNugget), Eva.cpp:215, CampaignManager.cpp:74, AI.cpp:175-178
#   (SideInfo -> SkillSet1..5, SkirmishBuildList -> Structure),
#   ControlBarScheme.cpp:76-77 + 896 (AnimatingPart -> ImagePart),
#   ShellMenuScheme.cpp:66-67, GameWindowTransitions.cpp:65,
#   ChallengeGenerals.cpp:99-113.
BLOCKS = [
    ("AIData", "INI::parseAIDataDefinition", False, None, [
        sb("SideInfo", sb("SkillSet1"), sb("SkillSet2"), sb("SkillSet3"),
           sb("SkillSet4"), sb("SkillSet5")),
        sb("SkirmishBuildList", sb("Structure")),
    ]),
    ("Animation", "INI::parseAnim2DDefinition", True, "anim2_d", []),
    ("AudioEvent", "INI::parseAudioEventDefinition", True, None, []),
    ("AudioSettings", "INI::parseAudioSettingsDefinition", False, None, []),
    ("BenchProfile", "INI::parseBenchProfile", False, None, []),
    ("Bridge", "INI::parseTerrainBridgeDefinition", True, None, []),
    ("Campaign", "INI::parseCampaignDefinition", True, None, [sb("Mission")]),
    ("ChallengeGenerals", "INI::parseChallengeModeDefinition", False, None,
     [sb(f"GeneralPersona{i}") for i in range(12)]),
    ("CommandButton", "INI::parseCommandButtonDefinition", True, "command_button", []),
    ("CommandMap", "INI::parseMetaMapDefinition", True, None, []),
    ("CommandSet", "INI::parseCommandSetDefinition", True, "command_set", []),
    ("ControlBarResizer", "INI::parseControlBarResizerDefinition", True, None, []),
    ("ControlBarScheme", "INI::parseControlBarSchemeDefinition", True, None, [
        sb("ImagePart"),
        sb("AnimatingPart", sb("ImagePart")),
    ]),
    ("CrateData", "INI::parseCrateTemplateDefinition", True, None, []),
    ("Credits", "INI::parseCredits", False, None, []),
    ("DamageFX", "INI::parseDamageFXDefinition", True, "damage_fx", []),
    ("DialogEvent", "INI::parseDialogDefinition", True, None, []),
    ("DrawGroupInfo", "INI::parseDrawGroupNumberDefinition", False, None, []),
    ("DynamicGameLOD", "INI::parseDynamicGameLODDefinition", True, None, []),
    ("EvaEvent", "INI::parseEvaEvent", True, None, [sb("SideSounds")]),
    ("FXList", "INI::parseFXListDefinition", True, "fx_list", [
        sb("Sound"), sb("RayEffect"), sb("Tracer"), sb("LightPulse"),
        sb("ViewShake"), sb("TerrainScorch"), sb("ParticleSystem"),
        sb("FXListAtBonePos"),
    ]),
    ("GameData", "INI::parseGameDataDefinition", False, None, []),
    ("HeaderTemplate", "INI::parseHeaderTemplateDefinition", True, None, []),
    ("InGameUI", "INI::parseInGameUIDefinition", False, None, []),
    ("LODPreset", "INI::parseLODPreset", True, None, []),
    ("Language", "INI::parseLanguageDefinition", False, None, []),
    ("MapCache", "INI::parseMapCacheDefinition", True, None, []),
    ("MapData", "INI::parseMapDataDefinition", True, None, []),
    ("MappedImage", "INI::parseMappedImageDefinition", True, "mapped_image", []),
    ("MiscAudio", "INI::parseMiscAudio", False, None, []),
    ("Mouse", "INI::parseMouseDefinition", False, None, []),
    ("MouseCursor", "INI::parseMouseCursorDefinition", True, None, []),
    ("MultiplayerColor", "INI::parseMultiplayerColorDefinition", True, None, []),
    ("MultiplayerSettings", "INI::parseMultiplayerSettingsDefinition", False, None, []),
    ("MultiplayerStartingMoneyChoice", "INI::parseMultiplayerStartingMoneyChoiceDefinition",
     False, None, []),
    ("MusicTrack", "INI::parseMusicTrackDefinition", True, None, []),
    ("ObjectCreationList", "INI::parseObjectCreationListDefinition", True,
     "object_creation_list", [
        sb("CreateObject"), sb("CreateDebris"), sb("ApplyRandomForce"),
        sb("DeliverPayload", sb("DeliveryDecal")), sb("FireWeapon"), sb("Attack"),
    ]),
    ("ObjectReskin", "INI::parseObjectReskinDefinition", True, "object", []),
    ("OnlineChatColors", "INI::parseOnlineChatColorDefinition", False, None, []),
    ("ParticleSystem", "INI::parseParticleSystemDefinition", True, "particle_system", []),
    ("PlayerTemplate", "INI::parsePlayerTemplateDefinition", True, "player_template", []),
    ("Rank", "INI::parseRankDefinition", True, None, []),
    ("ReallyLowMHz", "parseReallyLowMHz", False, None, []),
    ("Road", "INI::parseTerrainRoadDefinition", True, None, []),
    ("Science", "INI::parseScienceDefinition", True, "science", []),
    ("ScriptAction", "ScriptEngine::parseScriptAction", True, None, []),
    ("ScriptCondition", "ScriptEngine::parseScriptCondition", True, None, []),
    ("ShellMenuScheme", "INI::parseShellMenuSchemeDefinition", True, None, [
        sb("ImagePart"), sb("LinePart"),
    ]),
    ("SpecialPower", "INI::parseSpecialPowerDefinition", True, "special_power", []),
    ("StaticGameLOD", "INI::parseStaticGameLODDefinition", True, None, []),
    ("Terrain", "INI::parseTerrainDefinition", True, None, []),
    ("Video", "INI::parseVideoDefinition", True, None, []),
    ("WaterSet", "INI::parseWaterSettingDefinition", True, None, []),
    ("WaterTransparency", "INI::parseWaterTransparencyDefinition", False, None, []),
    ("Weather", "INI::parseWeatherDefinition", False, None, []),
    ("WebpageURL", "INI::parseWebpageURLDefinition", True, None, []),
    ("WindowTransition", "INI::parseWindowTransitions", True, None, [sb("Window")]),
]


def main():
    schema = json.loads(SCHEMA.read_text(encoding="utf-8"))
    have = {b["name"] for b in schema["blocks"]}
    added = []
    for name, parse_fn, named, defines, sub_blocks in BLOCKS:
        if name in have:
            continue
        block = {
            "name": name,
            "parse_fn": parse_fn,
            "named": named,
            "defines": defines,
            "doc": None,
            "fields": [],
        }
        if sub_blocks:
            block["sub_blocks"] = sub_blocks
        schema["blocks"].append(block)
        added.append(name)
    schema["blocks"].sort(key=lambda b: b["name"])
    SCHEMA.write_text(json.dumps(schema, indent=2) + "\n", encoding="utf-8")
    print(f"added {len(added)} blocks: {', '.join(added)}")


if __name__ == "__main__":
    main()
