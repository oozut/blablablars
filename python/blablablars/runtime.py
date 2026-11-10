"""Stdio runtime: serve one Python plugin to the Rust application.

Usage::

    python -m blablablars.runtime path/to/plugin.py

The process reads newline-delimited JSON requests on stdin and writes exactly one
JSON response per request on stdout. Nothing else may be written to stdout —
logging must go to stderr — because the host parses stdout as the protocol.

Methods: ``describe``, ``call`` and ``shutdown``; see ``docs/plugin-protocol.md``.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any, TextIO

from ._native import PROTOCOL_VERSION
from .plugin import HOOKS, coerce_plugin
from .plugins import load_plugin_file


def describe(plugin: Any) -> dict:
    name = getattr(plugin, "name", type(plugin).__name__)
    hooks = list(getattr(plugin, "hooks", HOOKS))
    description = {
        "protocol": PROTOCOL_VERSION,
        "name": name,
        "hooks": hooks,
    }
    version = getattr(plugin, "version", None)
    if version:
        description["version"] = version
    return description


def serve(
    plugin: Any,
    instream: TextIO | None = None,
    outstream: TextIO | None = None,
) -> int:
    """Serve ``plugin`` until ``shutdown`` or end of input. Returns an exit code."""
    plugin = coerce_plugin(plugin)
    instream = instream if instream is not None else sys.stdin
    outstream = outstream if outstream is not None else sys.stdout
    hooks = tuple(getattr(plugin, "hooks", HOOKS))
    call = getattr(plugin, "call", None)

    def write(payload: dict) -> None:
        json.dump(payload, outstream)
        outstream.write("\n")
        outstream.flush()

    def fail(request_id: Any, code: str, message: str) -> None:
        write({"id": request_id, "error": {"code": code, "message": message}})

    for line in instream:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except json.JSONDecodeError as error:
            fail(0, "bad_request", f"stdin was not JSON: {error}")
            continue

        request_id = request.get("id", 0)
        method = request.get("method")
        params = request.get("params") or {}

        if method == "describe":
            write({"id": request_id, "result": describe(plugin)})
        elif method == "call":
            hook_name = params.get("hook")
            if hook_name not in hooks:
                fail(
                    request_id,
                    "hook_not_subscribed",
                    f"{getattr(plugin, 'name', plugin)!r} does not handle {hook_name!r} "
                    f"(subscribed: {list(hooks)})",
                )
                continue
            if call is None:
                fail(request_id, "extension_error", "plugin has no `call` method")
                continue
            payload = params.get("payload")
            try:
                result = call(hook_name, payload)
            except Exception as error:  # noqa: BLE001 - the host must see the message
                fail(
                    request_id,
                    "extension_error",
                    f"{type(error).__name__}: {error}",
                )
                continue
            write({"id": request_id, "result": payload if result is None else result})
        elif method == "shutdown":
            write({"id": request_id, "result": {}})
            break
        else:
            fail(request_id, "unknown_method", f"unknown method {method!r}")
    return 0


def main(argv: list[str] | None = None) -> int:
    arguments = list(sys.argv[1:] if argv is None else argv)
    if not arguments or arguments[0] in {"-h", "--help"}:
        print(
            "usage: python -m blablablars.runtime PLUGIN.py\n\n"
            "Serves one plugin over the blablablars stdio protocol.",
            file=sys.stderr,
        )
        return 0 if arguments else 2
    path = Path(arguments[0])
    try:
        plugin = load_plugin_file(path)
        coerce_plugin(plugin)
    except Exception as error:  # noqa: BLE001 - report startup failures plainly
        print(f"blablablars runtime: cannot load {path}: {error}", file=sys.stderr)
        return 1
    return serve(plugin)


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
