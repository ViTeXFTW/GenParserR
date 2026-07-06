#!/usr/bin/env python3
"""End-to-end smoke test: drive the zerosyntax-lsp binary over stdio.

Sends initialize -> initialized -> didOpen (a Weapon block with a bad value and
an unknown field), then asserts we get publishDiagnostics, completions, and
semantic tokens back. Also drives INCREMENTAL didChange deltas (multi-change
batches, CRLF documents, EOF edits) and asserts the resulting diagnostics are
identical to a full-text baseline of the same final text. Exits non-zero on
failure.
"""
import json
import subprocess
import sys
import threading
import queue


def frame(obj: dict) -> bytes:
    body = json.dumps(obj).encode("utf-8")
    return b"Content-Length: %d\r\n\r\n%s" % (len(body), body)


def reader(stream, q: "queue.Queue"):
    buf = b""
    while True:
        chunk = stream.read1(4096) if hasattr(stream, "read1") else stream.read(1)
        if not chunk:
            q.put(None)
            return
        buf += chunk
        while True:
            sep = buf.find(b"\r\n\r\n")
            if sep == -1:
                break
            header = buf[:sep].decode("ascii", "replace")
            length = 0
            for line in header.split("\r\n"):
                if line.lower().startswith("content-length:"):
                    length = int(line.split(":", 1)[1].strip())
            start = sep + 4
            if len(buf) < start + length:
                break
            body = buf[start : start + length]
            buf = buf[start + length :]
            try:
                q.put(json.loads(body.decode("utf-8")))
            except Exception as e:  # noqa
                q.put({"_parse_error": str(e)})


def main() -> int:
    exe = sys.argv[1]

    # A throwaway workspace with an *uppercase* .INI definition file: the
    # scan must index it case-insensitively (real game data mixes casing).
    import pathlib
    import tempfile

    workspace = pathlib.Path(tempfile.mkdtemp(prefix="zerosyntax-e2e-"))
    (workspace / "Images.INI").write_text("MappedImage TestScanImage\nEnd\n")
    root_uri = workspace.as_uri()

    proc = subprocess.Popen(
        [exe],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        bufsize=0,
    )
    q: "queue.Queue" = queue.Queue()
    threading.Thread(target=reader, args=(proc.stdout, q), daemon=True).start()

    def send(obj):
        proc.stdin.write(frame(obj))
        proc.stdin.flush()

    def wait_for(pred, what, timeout=15.0):
        import time

        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                msg = q.get(timeout=deadline - time.time())
            except queue.Empty:
                break
            if msg is None:
                break
            if pred(msg):
                return msg
        print(f"TIMEOUT waiting for {what}", file=sys.stderr)
        return None

    # 1) initialize (with a workspace root so scan_workspace runs). Formatting
    #    is opt-in via initializationOptions; this session opts in so the
    #    formatting checks below run, and step 10 verifies the default is off.
    send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
          "params": {"capabilities": {}, "workspaceFolders": None, "rootUri": root_uri,
                     "initializationOptions": {"format": {"enable": True}}}})
    init = wait_for(lambda m: m.get("id") == 1 and "result" in m, "initialize result")
    assert init, "no initialize result"
    caps = init["result"]["capabilities"]
    assert "completionProvider" in caps, "missing completionProvider"
    assert "semanticTokensProvider" in caps, "missing semanticTokensProvider"
    sync = caps.get("textDocumentSync")
    assert sync == 2, f"expected INCREMENTAL sync (2), got {sync!r}"
    # We offered no positionEncodings, so the server must stay on the baseline.
    assert caps.get("positionEncoding", "utf-16") == "utf-16", caps.get("positionEncoding")
    print("OK: initialize advertised capabilities (incremental sync, utf-16)")

    send({"jsonrpc": "2.0", "method": "initialized", "params": {}})

    # 2) didOpen with a Weapon block: bad bool + unknown field.
    uri = "file:///test/Weapon.ini"
    text = "Weapon AK47\n  ScaleWeaponSpeed = Maybe\n  Bogus = 1\nEnd\n"
    send({"jsonrpc": "2.0", "method": "textDocument/didOpen",
          "params": {"textDocument": {"uri": uri, "languageId": "generals-ini",
                                       "version": 1, "text": text}}})

    diag = wait_for(
        lambda m: m.get("method") == "textDocument/publishDiagnostics"
        and m["params"]["uri"] == uri,
        "publishDiagnostics",
    )
    assert diag, "no diagnostics published"
    codes = [d.get("code") for d in diag["params"]["diagnostics"]]
    print("OK: diagnostics:", codes)
    assert "bad-bool" in codes, f"expected bad-bool in {codes}"
    assert "unknown-field" in codes, f"expected unknown-field in {codes}"

    # 3) completion inside the block on a fresh field position.
    #    Put cursor after "Weapon AK47\n  " (line 1, char 2) by editing to a blank line.
    text2 = "Weapon AK47\n  \nEnd\n"
    send({"jsonrpc": "2.0", "method": "textDocument/didChange",
          "params": {"textDocument": {"uri": uri, "version": 2},
                     "contentChanges": [{"text": text2}]}})
    wait_for(lambda m: m.get("method") == "textDocument/publishDiagnostics", "diags v2")
    send({"jsonrpc": "2.0", "id": 2, "method": "textDocument/completion",
          "params": {"textDocument": {"uri": uri},
                     "position": {"line": 1, "character": 2}}})
    comp = wait_for(lambda m: m.get("id") == 2 and "result" in m, "completion result")
    assert comp, "no completion result"
    items = comp["result"]
    if isinstance(items, dict):
        items = items.get("items", [])
    labels = [i["label"] for i in items]
    assert "PrimaryDamage" in labels, f"expected PrimaryDamage in {labels[:10]}..."
    print(f"OK: completion returned {len(labels)} items incl. field names")

    # 4) semantic tokens (full + range; the server must advertise range).
    assert caps["semanticTokensProvider"].get("range") is True, "range tokens not advertised"
    send({"jsonrpc": "2.0", "id": 3, "method": "textDocument/semanticTokens/full",
          "params": {"textDocument": {"uri": uri}}})
    sem = wait_for(lambda m: m.get("id") == 3 and "result" in m, "semantic tokens result")
    assert sem and sem["result"], "no semantic tokens"
    data = sem["result"]["data"]
    assert len(data) % 5 == 0 and len(data) > 0, "malformed semantic token data"
    print(f"OK: semantic tokens returned {len(data)//5} tokens")

    # The whole-document range must encode exactly the same data as full.
    send({"jsonrpc": "2.0", "id": 4, "method": "textDocument/semanticTokens/range",
          "params": {"textDocument": {"uri": uri},
                     "range": {"start": {"line": 0, "character": 0},
                               "end": {"line": 3, "character": 0}}}})
    sem_r = wait_for(lambda m: m.get("id") == 4 and "result" in m, "range tokens result")
    assert sem_r and sem_r["result"], "no range semantic tokens"
    assert sem_r["result"]["data"] == data, "range(whole doc) differs from full"
    print("OK: semanticTokens/range(whole doc) == full")

    # 5) incremental deltas must produce the same diagnostics as the full text.
    def norm(diags):
        return sorted(
            (d.get("code"), d["range"]["start"]["line"], d["range"]["start"]["character"],
             d["range"]["end"]["line"], d["range"]["end"]["character"], d["message"])
            for d in diags
        )

    def open_doc(doc_uri, doc_text, version=1):
        send({"jsonrpc": "2.0", "method": "textDocument/didOpen",
              "params": {"textDocument": {"uri": doc_uri, "languageId": "generals-ini",
                                          "version": version, "text": doc_text}}})
        msg = wait_for(
            lambda m: m.get("method") == "textDocument/publishDiagnostics"
            and m["params"]["uri"] == doc_uri,
            f"diagnostics for {doc_uri}",
        )
        assert msg, f"no diagnostics for {doc_uri}"
        return msg["params"]

    def change_doc(doc_uri, version, changes):
        send({"jsonrpc": "2.0", "method": "textDocument/didChange",
              "params": {"textDocument": {"uri": doc_uri, "version": version},
                         "contentChanges": changes}})
        msg = wait_for(
            lambda m: m.get("method") == "textDocument/publishDiagnostics"
            and m["params"]["uri"] == doc_uri
            and m["params"].get("version") == version,
            f"diagnostics v{version} for {doc_uri}",
        )
        assert msg, f"no v{version} diagnostics for {doc_uri}"
        return msg["params"]

    cases = [
        # (name, initial text, [(range, newText)], final text)
        ("value edit + field insert (multi-change batch)",
         "Weapon AK47\n  PrimaryDamage = 50.0\nEnd\n",
         [({"start": {"line": 1, "character": 18}, "end": {"line": 1, "character": 22}},
           "Maybe"),
          ({"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 0}},
           "  Bogus = 1\n")],
         "Weapon AK47\n  PrimaryDamage = Maybe\n  Bogus = 1\nEnd\n"),
        ("CRLF document edit",
         "Weapon M16\r\n  PrimaryDamage = 25.0\r\nEnd\r\n",
         [({"start": {"line": 1, "character": 18}, "end": {"line": 1, "character": 22}},
           "Nope")],
         "Weapon M16\r\n  PrimaryDamage = Nope\r\nEnd\r\n"),
        ("append block at EOF",
         "Weapon Pistol\nEnd\n",
         [({"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 0}},
           "Weapon Rifle\n  ScaleWeaponSpeed = Maybe\nEnd\n")],
         "Weapon Pistol\nEnd\nWeapon Rifle\n  ScaleWeaponSpeed = Maybe\nEnd\n"),
        ("delete spanning lines",
         "Weapon A\n  Bogus = 1\n  Bogus2 = 2\nEnd\n",
         [({"start": {"line": 1, "character": 0}, "end": {"line": 3, "character": 0}},
           "")],
         "Weapon A\nEnd\n"),
        ("delete an End line (splice fallback path)",
         "Weapon A\n  PrimaryDamage = 1\nEnd\nWeapon B\nEnd\n",
         [({"start": {"line": 2, "character": 0}, "end": {"line": 3, "character": 0}},
           "")],
         "Weapon A\n  PrimaryDamage = 1\nWeapon B\nEnd\n"),
        # >8 changes takes the server's bulk path (one full reparse instead of
        # per-change incremental) — the formatter-on-save shape.
        ("bulk change batch (full-parse path)",
         "Weapon A\nEnd\n",
         [({"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 0}},
           f"  Bogus{i} = 1\n") for i in range(1, 10)],
         "Weapon A\n" + "".join(f"  Bogus{i} = 1\n" for i in range(9, 0, -1)) + "End\n"),
    ]
    for i, (name, initial, changes, final) in enumerate(cases):
        inc_uri = f"file:///test/inc{i}.ini"
        base_uri = f"file:///test/base{i}.ini"
        open_doc(inc_uri, initial)
        got = change_doc(inc_uri, 2, [{"range": rng, "text": txt} for rng, txt in changes])
        want = open_doc(base_uri, final)
        assert norm(got["diagnostics"]) == norm(want["diagnostics"]), (
            f"[{name}] incremental diagnostics diverge from full-text baseline:\n"
            f"  incremental: {norm(got['diagnostics'])}\n"
            f"  baseline:    {norm(want['diagnostics'])}"
        )
        assert got.get("version") == 2, f"[{name}] missing/wrong version stamp"
        print(f"OK: incremental == baseline: {name}")

    # 6) workspace-scan references: the uppercase Images.INI was indexed, so
    #    `ButtonImage = TestScanImage` resolves and completion offers it.
    scan_uri = "file:///test/scan.ini"
    scan = open_doc(scan_uri, "Object ScanTest\n  ButtonImage = \nEnd\n")
    send({"jsonrpc": "2.0", "id": 7, "method": "textDocument/completion",
          "params": {"textDocument": {"uri": scan_uri},
                     "position": {"line": 1, "character": 16}}})
    comp = wait_for(lambda m: m.get("id") == 7 and "result" in m, "scan completion")
    items = comp["result"]
    if isinstance(items, dict):
        items = items.get("items", [])
    labels = [i["label"] for i in items]
    assert "TestScanImage" in labels, f"expected TestScanImage from .INI scan, got {labels[:10]}"
    scan2 = change_doc(scan_uri, 2, [
        {"range": {"start": {"line": 1, "character": 16}, "end": {"line": 1, "character": 16}},
         "text": "TestScanImage"}])
    codes = [d.get("code") for d in scan2["diagnostics"]]
    assert "unresolved-reference" not in codes, f"scan-indexed image should resolve: {codes}"
    print("OK: workspace scan indexed uppercase .INI (completion + resolution)")

    # 7) Phase-6 LSP breadth: outline, folding, workspace symbols, references,
    #    rename — exercised over a doc that defines and references an Upgrade.
    assert caps.get("documentSymbolProvider"), "missing documentSymbolProvider"
    assert caps.get("foldingRangeProvider"), "missing foldingRangeProvider"
    assert caps.get("workspaceSymbolProvider"), "missing workspaceSymbolProvider"
    assert caps.get("referencesProvider"), "missing referencesProvider"
    assert caps.get("renameProvider"), "missing renameProvider"

    p6_uri = "file:///test/phase6.ini"
    p6_text = ("Upgrade Upgrade_E2EPhase6\n  BuildTime = 1.0\nEnd\n"
               "Object Phase6Tank\n"
               "  Behavior = WeaponSetUpgrade ModuleTag_01\n"
               "    TriggeredBy = Upgrade_E2EPhase6\n"
               "  End\n"
               "End\n")
    open_doc(p6_uri, p6_text)

    send({"jsonrpc": "2.0", "id": 10, "method": "textDocument/documentSymbol",
          "params": {"textDocument": {"uri": p6_uri}}})
    syms = wait_for(lambda m: m.get("id") == 10 and "result" in m, "documentSymbol")
    names = [s["name"] for s in syms["result"]]
    assert names == ["Upgrade_E2EPhase6", "Phase6Tank"], names
    kids = [c["name"] for c in syms["result"][1].get("children", [])]
    assert "WeaponSetUpgrade" in kids, kids
    print("OK: documentSymbol returns nested outline")

    send({"jsonrpc": "2.0", "id": 11, "method": "textDocument/foldingRange",
          "params": {"textDocument": {"uri": p6_uri}}})
    folds = wait_for(lambda m: m.get("id") == 11 and "result" in m, "foldingRange")
    spans = {(f["startLine"], f["endLine"]) for f in folds["result"]}
    assert (0, 2) in spans and (3, 7) in spans and (4, 6) in spans, spans
    print("OK: foldingRange folds blocks and modules")

    send({"jsonrpc": "2.0", "id": 12, "method": "workspace/symbol",
          "params": {"query": "e2ephase6"}})
    wsym = wait_for(lambda m: m.get("id") == 12 and "result" in m, "workspace/symbol")
    assert any(s["name"] == "Upgrade_E2EPhase6" for s in wsym["result"]), wsym["result"][:3]
    print("OK: workspace/symbol matches case-insensitive substring")

    # references from the definition name (line 0 "Upgrade_E2EPhase6"),
    # including the declaration: expect the TriggeredBy site + the def.
    send({"jsonrpc": "2.0", "id": 13, "method": "textDocument/references",
          "params": {"textDocument": {"uri": p6_uri},
                     "position": {"line": 0, "character": 12},
                     "context": {"includeDeclaration": True}}})
    refs = wait_for(lambda m: m.get("id") == 13 and "result" in m, "references")
    lines = sorted(r["range"]["start"]["line"] for r in refs["result"])
    assert lines == [0, 5], f"expected def line 0 + site line 5, got {lines}"
    print("OK: references finds the TriggeredBy site and the declaration")

    # rename from the *reference* end (line 5) must edit both occurrences.
    send({"jsonrpc": "2.0", "id": 14, "method": "textDocument/rename",
          "params": {"textDocument": {"uri": p6_uri},
                     "position": {"line": 5, "character": 20},
                     "newName": "Upgrade_E2ERenamed"}})
    ren = wait_for(lambda m: m.get("id") == 14 and "result" in m, "rename")
    edits = ren["result"]["changes"][p6_uri]
    assert len(edits) == 2 and all(e["newText"] == "Upgrade_E2ERenamed" for e in edits), edits
    # An invalid new name (embedded space) must be rejected.
    send({"jsonrpc": "2.0", "id": 15, "method": "textDocument/rename",
          "params": {"textDocument": {"uri": p6_uri},
                     "position": {"line": 5, "character": 20},
                     "newName": "two words"}})
    bad = wait_for(lambda m: m.get("id") == 15, "rename rejection")
    assert "error" in bad, f"expected error for invalid name, got {bad}"
    print("OK: rename edits definition + references; invalid names rejected")

    # 8) Phase-6 batch 2: semanticTokens delta, formatting, code actions.
    assert caps["semanticTokensProvider"]["full"] == {"delta": True}, \
        caps["semanticTokensProvider"]["full"]
    assert caps.get("documentFormattingProvider"), "missing documentFormattingProvider"
    assert caps.get("codeActionProvider"), "missing codeActionProvider"

    # full (grab the resultId) -> edit -> delta must splice, not resend all.
    send({"jsonrpc": "2.0", "id": 20, "method": "textDocument/semanticTokens/full",
          "params": {"textDocument": {"uri": p6_uri}}})
    full = wait_for(lambda m: m.get("id") == 20 and "result" in m, "tokens full")
    rid = full["result"]["resultId"]
    assert rid, "full response carries no resultId"
    change_doc(p6_uri, 2, [
        {"range": {"start": {"line": 1, "character": 14}, "end": {"line": 1, "character": 17}},
         "text": "2.5"}])
    send({"jsonrpc": "2.0", "id": 21, "method": "textDocument/semanticTokens/full/delta",
          "params": {"textDocument": {"uri": p6_uri}, "previousResultId": rid}})
    delta = wait_for(lambda m: m.get("id") == 21 and "result" in m, "tokens delta")
    assert "edits" in delta["result"], f"expected a delta, got {list(delta['result'])}"
    edits = delta["result"]["edits"]
    assert len(edits) == 1 and len(edits[0].get("data", [])) < len(full["result"]["data"]), \
        "delta should splice less than the full token stream"
    # A bogus previousResultId falls back to a full response.
    send({"jsonrpc": "2.0", "id": 22, "method": "textDocument/semanticTokens/full/delta",
          "params": {"textDocument": {"uri": p6_uri}, "previousResultId": "no-such-id"}})
    fallback = wait_for(lambda m: m.get("id") == 22 and "result" in m, "delta fallback")
    assert "data" in fallback["result"], "stale id must fall back to full tokens"
    print("OK: semanticTokens/full/delta splices (and falls back on stale id)")

    # formatting: a misindented doc comes back normalized to scope depth.
    # Nearby per-line edits are coalesced server-side, so verify by applying
    # the edits rather than pinning their shapes.
    fmt_uri = "file:///test/fmt.ini"
    fmt_src = "Object FmtTank\nMaxHealth = 1\n      Behavior = AutoHealBehavior ModuleTag_01\nHealingAmount = 5\n      End\nEnd\n"
    open_doc(fmt_uri, fmt_src)
    send({"jsonrpc": "2.0", "id": 23, "method": "textDocument/formatting",
          "params": {"textDocument": {"uri": fmt_uri},
                     "options": {"tabSize": 2, "insertSpaces": True}}})
    fmt = wait_for(lambda m: m.get("id") == 23 and "result" in m, "formatting")

    def pos_off(text, pos):  # ASCII docs: utf-16 char == byte offset
        lines = text.split("\n")
        return sum(len(l) + 1 for l in lines[:pos["line"]]) + pos["character"]

    fmt_doc = fmt_src
    for e in sorted(fmt["result"], key=lambda e: pos_off(fmt_src, e["range"]["start"]),
                    reverse=True):
        s, t = pos_off(fmt_src, e["range"]["start"]), pos_off(fmt_src, e["range"]["end"])
        fmt_doc = fmt_doc[:s] + e["newText"] + fmt_doc[t:]
    assert fmt_doc == ("Object FmtTank\n  MaxHealth = 1\n"
                       "  Behavior = AutoHealBehavior ModuleTag_01\n"
                       "    HealingAmount = 5\n  End\nEnd\n"), fmt_doc
    assert len(fmt["result"]) == 1, \
        f"adjacent reindent edits should coalesce, got {len(fmt['result'])}"
    print("OK: formatting normalizes indentation to scope depth (coalesced)")

    # code actions: a misspelled enum member offers did-you-mean; an
    # unterminated block offers the missing End.
    ca_uri = "file:///test/actions.ini"
    open_doc(ca_uri, "Locomotor FixLoco\n  Appearance = TREDS\n")
    send({"jsonrpc": "2.0", "id": 24, "method": "textDocument/codeAction",
          "params": {"textDocument": {"uri": ca_uri},
                     "range": {"start": {"line": 0, "character": 0},
                               "end": {"line": 2, "character": 0}},
                     "context": {"diagnostics": []}}})
    ca = wait_for(lambda m: m.get("id") == 24 and "result" in m, "codeAction")
    titles = [a["title"] for a in ca["result"]]
    assert any("TREADS" in t for t in titles), titles
    assert any("`End`" in t for t in titles), titles
    fix = next(a for a in ca["result"] if "TREADS" in a["title"])
    new_texts = [e["newText"] for e in fix["edit"]["changes"][ca_uri]]
    assert new_texts == ["TREADS"], new_texts
    print("OK: code actions offer did-you-mean + missing End quickfixes")

    # 9) shutdown
    send({"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": None})
    wait_for(lambda m: m.get("id") == 99, "shutdown")
    send({"jsonrpc": "2.0", "method": "exit", "params": None})
    try:
        proc.wait(timeout=5)
    except Exception:
        proc.kill()

    # 10) a default-initialized server (no initializationOptions) must not
    #     advertise formatting and must answer the request with null.
    proc2 = subprocess.Popen(
        [exe],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        bufsize=0,
    )
    q2: "queue.Queue" = queue.Queue()
    threading.Thread(target=reader, args=(proc2.stdout, q2), daemon=True).start()

    def send2(obj):
        proc2.stdin.write(frame(obj))
        proc2.stdin.flush()

    def wait_for2(pred, what, timeout=15.0):
        import time

        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                msg = q2.get(timeout=deadline - time.time())
            except queue.Empty:
                break
            if msg is None:
                break
            if pred(msg):
                return msg
        print(f"TIMEOUT waiting for {what}", file=sys.stderr)
        return None

    send2({"jsonrpc": "2.0", "id": 1, "method": "initialize",
           "params": {"capabilities": {}, "workspaceFolders": None, "rootUri": None}})
    init2 = wait_for2(lambda m: m.get("id") == 1 and "result" in m, "default initialize")
    assert init2, "no default initialize result"
    caps2 = init2["result"]["capabilities"]
    assert "documentFormattingProvider" not in caps2, \
        "formatting must be off by default (no initializationOptions)"
    send2({"jsonrpc": "2.0", "method": "initialized", "params": {}})
    send2({"jsonrpc": "2.0", "method": "textDocument/didOpen",
           "params": {"textDocument": {"uri": fmt_uri, "languageId": "generals-ini",
                                       "version": 1, "text": "Object T\nMaxHealth = 1\nEnd\n"}}})
    send2({"jsonrpc": "2.0", "id": 2, "method": "textDocument/formatting",
           "params": {"textDocument": {"uri": fmt_uri},
                      "options": {"tabSize": 2, "insertSpaces": True}}})
    fmt2 = wait_for2(lambda m: m.get("id") == 2, "disabled formatting response")
    assert fmt2 and fmt2.get("result") is None, \
        f"disabled formatting must return null, got {fmt2}"
    print("OK: formatting is opt-in (capability withheld + request answers null)")
    send2({"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": None})
    wait_for2(lambda m: m.get("id") == 99, "shutdown 2")
    send2({"jsonrpc": "2.0", "method": "exit", "params": None})
    try:
        proc2.wait(timeout=5)
    except Exception:
        proc2.kill()

    print("\nALL E2E CHECKS PASSED")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except AssertionError as e:
        print("E2E FAILURE:", e, file=sys.stderr)
        sys.exit(1)
