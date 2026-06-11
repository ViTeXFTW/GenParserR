# Phase 5: retype name-list fields as reference_list so every entry resolves
# against the index (validation + completion): upgrade trigger lists on all
# modules, science vectors, and Object Prerequisites.
import json
import pathlib

root = pathlib.Path(__file__).resolve().parents[1]
schema_path = root / "crates/schema/schema.json"
schema = json.loads(schema_path.read_text(encoding="utf-8"))
blocks = {b["name"]: b for b in schema["blocks"]}

schema["format_version"] = 4

REFLIST = lambda k: {"kind": "reference_list", "ref_kind": k}

# UpgradeMuxData's name lists are upgrade references (findUpgradeByName).
count = 0
for m in schema["modules"]:
    for f in m["fields"]:
        if f["name"] in ("TriggeredBy", "ConflictsWith", "RemovesUpgrades"):
            f["value_type"] = REFLIST("upgrade")
            count += 1
        elif f["parse_fn"] == "INI::parseScienceVector":
            f["value_type"] = REFLIST("science")

for f in blocks["CommandButton"]["fields"]:
    if f["name"] == "Science":
        f["value_type"] = REFLIST("science")

# Object/ObjectReskin Prerequisites: `Object = A B` (any of), `Science = S`.
for name in ("Object", "ObjectReskin"):
    prereq = next(s for s in blocks[name]["sub_blocks"] if s["keyword"] == "Prerequisites")
    prereq["fields"] = [
        {"name": "Object", "value_type": REFLIST("object"),
         "parse_fn": "ProductionPrerequisite::parsePrerequisiteUnit", "doc": None},
        {"name": "Science", "value_type": REFLIST("science"),
         "parse_fn": "ProductionPrerequisite::parsePrerequisiteScience", "doc": None},
    ]

schema_path.write_text(json.dumps(schema, indent=2) + "\n", encoding="utf-8")
print(f"retyped {count} upgrade-list fields + science vectors + Prerequisites")
