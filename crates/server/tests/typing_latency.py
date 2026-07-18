#!/usr/bin/env python3
"""Measure typing responsiveness through the real LSP binary.

Usage: python typing_latency.py <zerosyntax-lsp> <large-map.ini>
"""

import json
import pathlib
import queue
import statistics
import subprocess
import sys
import threading
import time

sys.dont_write_bytecode = True
from e2e import frame, reader


def main() -> int:
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} <zerosyntax-lsp> <map.ini>", file=sys.stderr)
        return 2

    exe, filename = sys.argv[1:]
    path = pathlib.Path(filename).resolve()
    text = path.read_text(encoding="utf-8", errors="replace")
    lines = text.splitlines()
    uri = path.as_uri()
    proc = subprocess.Popen(
        [exe],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        bufsize=0,
    )
    messages: "queue.Queue" = queue.Queue()
    threading.Thread(target=reader, args=(proc.stdout, messages), daemon=True).start()
    diagnostics = []

    def send(message):
        proc.stdin.write(frame(message))
        proc.stdin.flush()

    def receive(timeout=30.0):
        message = messages.get(timeout=timeout)
        if message is None:
            raise RuntimeError("language server exited")
        if message.get("method") == "textDocument/publishDiagnostics":
            params = message["params"]
            diagnostics.append(
                (time.perf_counter(), params.get("version"), len(params["diagnostics"]))
            )
        return message

    def wait_id(request_id):
        try:
            while True:
                message = receive()
                if message.get("id") == request_id:
                    return message
        except queue.Empty as error:
            raise RuntimeError(f"timed out waiting for response {request_id}") from error

    def wait_diagnostics(version):
        try:
            while True:
                match = next((item for item in diagnostics if item[1] == version), None)
                if match:
                    return match
                receive()
        except queue.Empty as error:
            seen = [item[1] for item in diagnostics]
            raise RuntimeError(
                f"timed out waiting for diagnostics {version}; saw {seen}"
            ) from error

    send({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": None,
            "rootUri": None,
            "capabilities": {"general": {"positionEncodings": ["utf-8"]}},
        },
    })
    wait_id(1)
    send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
    opened = time.perf_counter()
    send({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": uri,
                "languageId": "generals-ini",
                "version": 1,
                "text": text,
            }
        },
    })
    first_diag = wait_diagnostics(1)

    line_number, line = next(
        (i, line)
        for i, line in enumerate(lines)
        if "=" in line and not line.lstrip().startswith(";") and line.split("=", 1)[1].strip()
    )
    value_column = line.index("=") + 1
    while line[value_column].isspace():
        value_column += 1
    original = line[value_column]
    replacement = "X" if original != "X" else "Y"
    position = {"line": line_number, "character": value_column}
    end = {"line": line_number, "character": value_column + 1}
    completion_position = {"line": line_number, "character": len(line)}
    version = 1
    request_id = 10

    def change(character):
        nonlocal version
        version += 1
        send({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": {"uri": uri, "version": version},
                "contentChanges": [{
                    "range": {"start": position, "end": end},
                    "text": character,
                }],
            },
        })

    def complete():
        nonlocal request_id
        request_id += 1
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "textDocument/completion",
            "params": {"textDocument": {"uri": uri}, "position": completion_position},
        })
        wait_id(request_id)

    idle = []
    for _ in range(20):
        started = time.perf_counter()
        complete()
        idle.append((time.perf_counter() - started) * 1000)

    results = []
    current = original
    for burst in (1, 4, 8, 16):
        started = time.perf_counter()
        for _ in range(burst):
            current = replacement if current == original else original
            change(current)
        latest = version
        complete()
        completion_ms = (time.perf_counter() - started) * 1000
        latest_diag = wait_diagnostics(latest)
        results.append({
            "edits": burst,
            "completion_ms": round(completion_ms, 1),
            "diagnostics_ms": round((latest_diag[0] - started) * 1000, 1),
            "published_versions": [
                item[1] for item in diagnostics if started <= item[0] <= latest_diag[0]
            ],
        })

    report = {
        "file": str(path),
        "file_mib": round(len(text.encode("utf-8")) / 1024 / 1024, 2),
        "lines": len(lines),
        "diagnostics": first_diag[2],
        "open_to_diagnostics_ms": round((first_diag[0] - opened) * 1000, 1),
        "idle_completion_median_ms": round(statistics.median(idle), 2),
        "idle_completion_p95_ms": round(sorted(idle)[-2], 2),
        "bursts": results,
    }
    print(json.dumps(report, indent=2))

    send({"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": None})
    wait_id(99)
    send({"jsonrpc": "2.0", "method": "exit", "params": None})
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (AssertionError, RuntimeError, queue.Empty) as error:
        print(f"typing benchmark failed: {error}", file=sys.stderr)
        sys.exit(1)
