#!/usr/bin/env python3
"""End-to-end smoke test: drive the genparser-lsp binary over stdio.

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

    # 1) initialize
    send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
          "params": {"capabilities": {}, "workspaceFolders": None, "rootUri": None}})
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

    # 4) semantic tokens.
    send({"jsonrpc": "2.0", "id": 3, "method": "textDocument/semanticTokens/full",
          "params": {"textDocument": {"uri": uri}}})
    sem = wait_for(lambda m: m.get("id") == 3 and "result" in m, "semantic tokens result")
    assert sem and sem["result"], "no semantic tokens"
    data = sem["result"]["data"]
    assert len(data) % 5 == 0 and len(data) > 0, "malformed semantic token data"
    print(f"OK: semantic tokens returned {len(data)//5} tokens")

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

    # 6) shutdown
    send({"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": None})
    wait_for(lambda m: m.get("id") == 99, "shutdown")
    send({"jsonrpc": "2.0", "method": "exit", "params": None})
    try:
        proc.wait(timeout=5)
    except Exception:
        proc.kill()

    print("\nALL E2E CHECKS PASSED")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except AssertionError as e:
        print("E2E FAILURE:", e, file=sys.stderr)
        sys.exit(1)
