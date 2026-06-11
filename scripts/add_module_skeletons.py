# Phase 5: add a ModuleType skeleton (empty field list = lenient) for every
# module the engine's ModuleFactory registers, so `unknown-module` fires only
# for names the engine genuinely does not know. Field typing is grown by hand
# afterwards, highest corpus frequency first.
import json
import pathlib
import re

root = pathlib.Path(__file__).resolve().parents[1]
schema_path = root / "crates/schema/schema.json"
schema = json.loads(schema_path.read_text(encoding="utf-8"))

SECTION_IFACE = {
    "behavior": "Behavior",
    "die": "Die",
    "update": "Update",
    "upgrade": "Upgrade",
    "create": "Create",
    "damage": "Damage",
    "collide": "Collide",
    "body": "Body",
    "contain": "Contain",
    "destroy": "Destroy",
    "draw": "Draw",
    "client update": "ClientUpdate",
}

factories = [
    root / "GeneralsCode/GameEngine/Source/Common/Thing/ModuleFactory.cpp",
    root
    / "GeneralsCode/GameEngineDevice/Source/W3DDevice/Common/Thing/W3DModuleFactory.cpp",
]

found: dict[str, str] = {}
for path in factories:
    iface = "Draw" if "W3D" in path.name else "Behavior"
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        m = re.match(r"\s*//\s*([a-z ]+?)\s+modules", line)
        if m and m.group(1).strip() in SECTION_IFACE:
            iface = SECTION_IFACE[m.group(1).strip()]
            continue
        m = re.match(r"\s*addModule\s*\(\s*(\w+)\s*\)", line)
        if m:
            found.setdefault(m.group(1), iface)

existing = {m["name"] for m in schema["modules"]}
added = 0
for name, iface in sorted(found.items()):
    if name in existing:
        continue
    schema["modules"].append({
        "name": name,
        "interfaces": [iface],
        "fields": [],
        "sub_blocks": [],
        "doc": None,
    })
    added += 1

schema_path.write_text(json.dumps(schema, indent=2) + "\n", encoding="utf-8")
print(f"registered {len(found)} engine modules; added {added} skeletons "
      f"({len(existing)} already present)")
