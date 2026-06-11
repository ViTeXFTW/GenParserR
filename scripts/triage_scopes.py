# SPDX-License-Identifier: GPL-3.0-or-later
"""Corpus triage: emulate the OpenerOracle scope model over the corpus and
report exactly where `End` underflows (closes more scopes than opened) or a
file ends with scopes still open. Each report shows the suspect line and the
scope stack, pointing at the keyword that should have opened a scope.

Usage: python scripts/triage_scopes.py [max_reports_per_file]
"""
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
CORPUS = ROOT / "corpus/GeneralsGamePatch2/GeneralsZH/Data/INI"

CURATED = {
    "ConditionState", "DefaultConditionState", "TransitionState",
    "AnimationState", "DefaultAnimationState", "IdleAnimationState",
    "ArmorSet", "WeaponSet", "AttackContactPoint", "Turret", "AltTurret",
    "UnitSpecificSounds", "InheritableModule", "OverrideableByLikeKind",
}


def children_map():
    schema = json.loads((ROOT / "crates/schema/schema.json").read_text(encoding="utf-8"))
    out = {}

    def add_subs(parent, subs):
        for s in subs:
            out.setdefault(parent, set()).add(s["keyword"])
            add_subs(s["keyword"], s.get("sub_blocks", []))

    for b in schema["blocks"]:
        out.setdefault(b["name"], set()).update(
            s["keyword"] for s in b.get("module_slots", []))
        add_subs(b["name"], b.get("sub_blocks", []))
    for m in schema["modules"]:
        add_subs(m["name"], m.get("sub_blocks", []))
    return out


def head_of(line):
    code = line.split(";", 1)[0]
    toks = code.replace("=", " = ").split()
    if not toks:
        return None, False
    return toks[0], "=" in toks


def main():
    limit = int(sys.argv[1]) if len(sys.argv) > 1 else 3
    children = children_map()
    total = 0
    for path in sorted(CORPUS.rglob("*.ini")):
        rel = path.relative_to(CORPUS)
        text = path.read_bytes().decode("utf-8", "replace")
        stack = []  # (head, lineno)
        reports = []
        for lineno, line in enumerate(text.splitlines(), 1):
            head, has_eq = head_of(line)
            if head is None:
                continue
            if head.lower() == "end":
                if stack:
                    stack.pop()
                else:
                    reports.append((lineno, line.rstrip(), "stray End", list(stack)))
            elif not stack:
                stack.append((head, lineno))
            elif head in CURATED or head in children.get(stack[-1][0], set()):
                stack.append((head, lineno))
        for head, lineno in stack:
            reports.append((lineno, head, "unterminated scope", []))
        if reports:
            print(f"== {rel} ({len(reports)} problems)")
            for lineno, line, why, st in reports[:limit]:
                print(f"  L{lineno}: {why}: {line!r}")
                if why == "stray End":
                    # show preceding non-blank lines for context
                    lines = text.splitlines()
                    ctx = [l.rstrip() for l in lines[max(0, lineno - 6):lineno - 1]]
                    for c in ctx:
                        print(f"        | {c}")
            total += len(reports)
    print(f"\ntotal problems: {total}")


if __name__ == "__main__":
    main()
