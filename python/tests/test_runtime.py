"""Tests for the stdio plugin runtime and the plugin loader."""

from __future__ import annotations

import io
import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

import blablablars
from blablablars import runtime
from blablablars.plugins import ENV_VAR, load_plugin_file, load_plugins

PLUGIN_SOURCE = textwrap.dedent(
    """
    from blablablars import HOOK_INPUT, hook

    @hook(HOOK_INPUT)
    def upper(payload):
        payload["text"] = payload["text"].upper()
        return payload
    """
)


def write_plugin(directory: str, name: str = "upper_plugin", source: str = PLUGIN_SOURCE) -> Path:
    path = Path(directory) / f"{name}.py"
    path.write_text(source, encoding="utf-8")
    return path


def exchange(lines: list[dict], plugin: object) -> list[dict]:
    instream = io.StringIO("".join(json.dumps(line) + "\n" for line in lines))
    outstream = io.StringIO()
    runtime.serve(plugin, instream=instream, outstream=outstream)
    return [json.loads(line) for line in outstream.getvalue().splitlines()]


class RuntimeTests(unittest.TestCase):
    def test_describe_then_call_then_shutdown(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            plugin = load_plugin_file(write_plugin(directory))

        responses = exchange(
            [
                {"id": 1, "method": "describe", "params": {}},
                {
                    "id": 2,
                    "method": "call",
                    "params": {"hook": "pipeline.input", "payload": {"text": "hi"}},
                },
                {"id": 3, "method": "shutdown", "params": {}},
                {"id": 4, "method": "describe", "params": {}},
            ],
            plugin,
        )

        self.assertEqual(len(responses), 3, "shutdown must stop the loop")
        self.assertEqual(responses[0]["result"]["protocol"], 1)
        self.assertEqual(responses[0]["result"]["name"], "upper_plugin")
        self.assertEqual(responses[0]["result"]["hooks"], ["pipeline.input"])
        self.assertEqual(responses[1]["result"], {"text": "HI"})
        self.assertEqual(responses[2]["result"], {})

    def test_unknown_method_and_unsubscribed_hook_are_errors(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            plugin = load_plugin_file(write_plugin(directory))

        responses = exchange(
            [
                {"id": 1, "method": "teleport", "params": {}},
                {
                    "id": 2,
                    "method": "call",
                    "params": {"hook": "pipeline.output", "payload": {"analysis": {}}},
                },
            ],
            plugin,
        )

        self.assertEqual(responses[0]["error"]["code"], "unknown_method")
        self.assertEqual(responses[1]["error"]["code"], "hook_not_subscribed")

    def test_bad_json_is_reported_and_does_not_kill_the_server(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            plugin = load_plugin_file(write_plugin(directory))

        instream = io.StringIO("not json\n" + json.dumps({"id": 9, "method": "describe"}) + "\n")
        outstream = io.StringIO()
        runtime.serve(plugin, instream=instream, outstream=outstream)

        responses = [json.loads(line) for line in outstream.getvalue().splitlines()]
        self.assertEqual(responses[0]["error"]["code"], "bad_request")
        self.assertEqual(responses[1]["result"]["protocol"], 1)

    def test_an_exception_in_a_handler_becomes_an_error_response(self) -> None:
        class Angry(blablablars.Plugin):
            name = "angry"
            hooks = (blablablars.HOOK_INPUT,)

            def call(self, hook_name: str, payload: dict) -> dict:
                raise RuntimeError("kaboom")

        responses = exchange(
            [
                {
                    "id": 1,
                    "method": "call",
                    "params": {"hook": "pipeline.input", "payload": {"text": "hi"}},
                }
            ],
            Angry(),
        )
        self.assertEqual(responses[0]["error"]["code"], "extension_error")
        self.assertIn("kaboom", responses[0]["error"]["message"])


class SubprocessRuntimeTests(unittest.TestCase):
    """The real path the Rust binary uses: ``python -m blablablars.runtime``."""

    def test_runtime_module_speaks_the_protocol(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = write_plugin(directory)
            requests = (
                json.dumps({"id": 1, "method": "describe", "params": {}})
                + "\n"
                + json.dumps(
                    {
                        "id": 2,
                        "method": "call",
                        "params": {"hook": "pipeline.input", "payload": {"text": "hi"}},
                    }
                )
                + "\n"
                + json.dumps({"id": 3, "method": "shutdown", "params": {}})
                + "\n"
            )
            completed = subprocess.run(
                [sys.executable, "-m", "blablablars.runtime", str(path)],
                input=requests,
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        responses = [json.loads(line) for line in completed.stdout.splitlines()]
        self.assertEqual(responses[0]["result"]["name"], "upper_plugin")
        self.assertEqual(responses[1]["result"], {"text": "HI"})

    def test_runtime_module_reports_a_missing_plugin(self) -> None:
        completed = subprocess.run(
            [sys.executable, "-m", "blablablars.runtime", "/nonexistent/plugin.py"],
            input="",
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        self.assertEqual(completed.returncode, 1)
        self.assertIn("cannot load", completed.stderr)


class LoaderTests(unittest.TestCase):
    def test_load_plugin_file_returns_the_module(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            module = load_plugin_file(write_plugin(directory))
        self.assertTrue(hasattr(module, "upper"))
        plugin = blablablars.coerce_plugin(module)
        self.assertEqual(plugin.hooks, (blablablars.HOOK_INPUT,))

    def test_load_plugins_honours_the_environment_variable(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = write_plugin(directory)
            previous = os.environ.get(ENV_VAR)
            os.environ[ENV_VAR] = str(path)
            try:
                plugins = load_plugins(entry_points=[])
            finally:
                if previous is None:
                    os.environ.pop(ENV_VAR, None)
                else:
                    os.environ[ENV_VAR] = previous

        self.assertEqual(len(plugins), 1)
        self.assertEqual(plugins[0].name, "upper_plugin")

    def test_load_plugins_accepts_entry_points(self) -> None:
        class FakeEntryPoint:
            name = "fake"

            @staticmethod
            def load() -> object:
                class Fake(blablablars.Plugin):
                    name = "fake"

                return Fake()

        plugins = load_plugins(entry_points=[FakeEntryPoint()])
        self.assertEqual([plugin.name for plugin in plugins], ["fake"])

    def test_a_missing_file_is_reported(self) -> None:
        with self.assertRaises(FileNotFoundError):
            load_plugin_file("/nonexistent/plugin.py")


if __name__ == "__main__":
    unittest.main()
