"""Extensions that run inside the same process as the Rust core.

Covers the two in-process transports reachable from Python — a compiled Rust
extension loaded with ``dlopen`` and a plugin served by a child process — from
the same :class:`blablablars.Pipeline`.
"""

from __future__ import annotations

import shutil
import sys
import tempfile
import unittest
from pathlib import Path

import blablablars

ROOT = Path(__file__).resolve().parents[2]
NAME = "libblablablars_example_rust_plugin.so"


def example_library() -> Path:
    for profile in ("debug", "release"):
        candidate = ROOT / "target" / "plugin-fixture" / profile / NAME
        if candidate.is_file():
            return candidate
    raise unittest.SkipTest("build the example extension first: make example")


class RustExtensionTests(unittest.TestCase):
    def test_a_rust_extension_runs_in_process(self) -> None:
        pipeline = blablablars.Pipeline()
        pipeline.register_rust(example_library())

        self.assertEqual(pipeline.extensions, ["shout"])
        run = pipeline.run("hello from python")
        self.assertEqual(run.analysis.text, "[shouted] HELLO FROM PYTHON")
        self.assertEqual(
            run.trace, ["shout: pipeline.input", "shout: pipeline.output"]
        )

    def test_a_directory_of_rust_extensions_can_be_loaded(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            shutil.copy(example_library(), Path(directory) / NAME)
            pipeline = blablablars.Pipeline()
            self.assertEqual(pipeline.register_rust_directory(directory), ["shout"])
            self.assertEqual(pipeline.run("x").analysis.text, "[shouted] X")

    def test_a_missing_library_is_an_error_not_a_crash(self) -> None:
        with self.assertRaises(RuntimeError) as caught:
            blablablars.Pipeline().register_rust("/nonexistent/extension.so")
        self.assertIn("could not load the shared library", str(caught.exception))

    def test_a_duplicate_name_is_refused(self) -> None:
        pipeline = blablablars.Pipeline()
        pipeline.register_rust(example_library())
        with self.assertRaises(RuntimeError) as caught:
            pipeline.register_rust(example_library())
        self.assertIn("already registered", str(caught.exception))


class ProcessExtensionTests(unittest.TestCase):
    def test_a_plugin_can_run_in_a_child_process(self) -> None:
        plugin = ROOT / "examples" / "python" / "hello_plugin.py"
        pipeline = blablablars.Pipeline()
        name = pipeline.register_process(
            f"{sys.executable} -m blablablars.runtime {plugin}"
        )
        self.assertEqual(name, "hello_plugin")

        run = pipeline.run("hello child")
        self.assertEqual(run.analysis.text, "[shouted] HELLO CHILD")
        self.assertEqual(
            run.trace, ["hello_plugin: pipeline.input", "hello_plugin: pipeline.output"]
        )

    def test_a_bad_command_fails_with_a_message(self) -> None:
        with self.assertRaises(RuntimeError) as caught:
            blablablars.Pipeline().register_process("definitely-not-a-program-xyz")
        self.assertIn("could not start plugin", str(caught.exception))


class MixedTransportTests(unittest.TestCase):
    def test_rust_and_python_callbacks_share_a_pipeline(self) -> None:
        pipeline = blablablars.Pipeline()
        pipeline.register_rust(example_library())

        pipeline.register(_BracketPlugin())

        run = pipeline.run("mixed")
        self.assertEqual(run.analysis.text, "<[shouted] MIXED>")
        self.assertEqual(len(run.trace), 3)

    def test_order_is_registration_order(self) -> None:
        pipeline = blablablars.Pipeline()
        pipeline.register(_UpperPlugin())
        pipeline.register_rust(example_library())
        self.assertEqual(pipeline.extensions, ["upper", "shout"])
        # `upper` runs first, then the Rust extension's input hook.
        self.assertEqual(pipeline.run("abc").analysis.text, "[shouted] ABC")


class SpecTests(unittest.TestCase):
    """The spec grammar is shared with the binary, so it must parse the same."""

    def test_a_spec_names_its_transport(self) -> None:
        pipeline = blablablars.Pipeline()
        names = pipeline.load(
            f"rust:{example_library()}",
            f"process:{sys.executable} -m blablablars.runtime "
            f"{ROOT / 'examples' / 'python' / 'hello_plugin.py'}",
        )
        self.assertEqual(names, ["shout", "hello_plugin"])

    def test_a_bare_value_is_inferred_like_the_command_line(self) -> None:
        pipeline = blablablars.Pipeline()
        self.assertEqual(pipeline.load(str(example_library())), ["shout"])

    def test_a_python_spec_is_imported_and_attached(self) -> None:
        plugin = ROOT / "examples" / "python" / "hello_plugin.py"
        pipeline = blablablars.Pipeline()
        self.assertEqual(pipeline.load(f"python:{plugin}"), ["hello_plugin"])
        self.assertEqual(pipeline.run("hi").analysis.text, "[shouted] HI")

    def test_a_bad_spec_is_a_value_error(self) -> None:
        with self.assertRaises(ValueError):
            blablablars.Pipeline().load("python:")


class _BracketPlugin(blablablars.Plugin):
    name = "bracket"
    hooks = (blablablars.HOOK_OUTPUT,)

    def call(self, hook_name, payload):
        payload["analysis"]["text"] = f"<{payload['analysis']['text']}>"
        return payload


class _UpperPlugin(blablablars.Plugin):
    name = "upper"
    hooks = (blablablars.HOOK_INPUT,)

    def call(self, hook_name, payload):
        payload["text"] = payload["text"].upper()
        return payload


if __name__ == "__main__":
    unittest.main()
