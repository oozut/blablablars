"""Tests for the native bindings and the Python pipeline wrapper."""

from __future__ import annotations

import unittest

import blablablars


class NativeModuleTests(unittest.TestCase):
    def test_version_and_protocol_are_exposed(self) -> None:
        self.assertRegex(blablablars.__version__, r"^\d+\.\d+\.\d+")
        self.assertEqual(blablablars.PROTOCOL_VERSION, 1)
        self.assertEqual(
            blablablars.hooks(),
            [
                "pipeline.input",
                "pipeline.output",
                "transcription.raw",
                "transcription.text",
            ],
        )

    def test_analyse_is_pure_and_unicode_aware(self) -> None:
        analysis = blablablars.analyse("héllo wörld")
        self.assertEqual(analysis.words, 2)
        self.assertEqual(analysis.characters, 11)
        self.assertEqual(len(analysis.digest), 64)
        self.assertEqual(analysis.text, "héllo wörld")
        self.assertEqual(analysis.to_dict()["words"], 2)

    def test_analysis_equality(self) -> None:
        self.assertEqual(blablablars.analyse("a b"), blablablars.analyse("a b"))
        self.assertNotEqual(blablablars.analyse("a b"), blablablars.analyse("a c"))


class PipelineTests(unittest.TestCase):
    def test_a_plain_run_has_no_extensions(self) -> None:
        result = blablablars.Pipeline().run("one two three")
        self.assertEqual(result.analysis.text, "one two three")
        self.assertEqual(result.analysis.words, 3)
        self.assertEqual(result.trace, [])

    def test_run_json_matches_the_rust_and_cli_shape(self) -> None:
        payload = blablablars.Pipeline().run_json({"text": "hello world"})
        self.assertEqual(payload["analysis"]["words"], 2)
        self.assertEqual(payload["analysis"]["characters"], 11)
        self.assertEqual(payload["trace"], [])
        self.assertEqual(len(payload["analysis"]["digest"]), 64)

    def test_run_json_rejects_payload_without_text(self) -> None:
        with self.assertRaises(ValueError):
            blablablars.Pipeline().run_json({"not_text": 1})

    def test_hooks_and_describe(self) -> None:
        pipeline = blablablars.Pipeline()
        self.assertEqual(
            pipeline.hooks,
            (
                "pipeline.input",
                "pipeline.output",
                "transcription.raw",
                "transcription.text",
            ),
        )
        self.assertEqual(pipeline.describe(), [])

    def test_register_and_unregister(self) -> None:
        class Noop(blablablars.Plugin):
            name = "noop"

        pipeline = blablablars.Pipeline([Noop()])
        self.assertEqual(pipeline.extensions, ["noop"])
        self.assertEqual(pipeline.describe(), [("noop", ["pipeline.input", "pipeline.output"])])
        self.assertTrue(pipeline.unregister("noop"))
        self.assertFalse(pipeline.unregister("noop"))
        self.assertEqual(pipeline.extensions, [])

    def test_duplicate_names_are_rejected(self) -> None:
        class First(blablablars.Plugin):
            name = "same"

        pipeline = blablablars.Pipeline([First()])
        with self.assertRaises(RuntimeError):
            pipeline.register(First())

    def test_unknown_hooks_are_rejected(self) -> None:
        with self.assertRaises(Exception) as raised:
            blablablars.Pipeline().register(
                blablablars.FunctionPlugin("bad", {"pipeline.nope": [lambda p: p]})
            )
        self.assertIn("pipeline.nope", str(raised.exception))


class ExtensionTests(unittest.TestCase):
    def test_decorated_methods_are_chained_in_definition_order(self) -> None:
        class Shout(blablablars.Plugin):
            name = "shout"
            hooks = (blablablars.HOOK_INPUT,)

            @blablablars.hook(blablablars.HOOK_INPUT)
            def upper(self, payload: dict) -> dict:
                payload["text"] = payload["text"].upper()
                return payload

        pipeline = blablablars.Pipeline([Shout()])
        result = pipeline.run("hi there")

        self.assertEqual(result.analysis.text, "HI THERE")
        self.assertEqual(result.trace, ["shout: pipeline.input"])

    def test_module_style_plugin_from_namespace(self) -> None:
        class Namespace:
            @staticmethod
            @blablablars.hook(blablablars.HOOK_INPUT)
            def prefix(payload: dict) -> dict:
                payload["text"] = ">> " + payload["text"]
                return payload

        plugin = blablablars.plugin_from_namespace(Namespace, name="prefixer")
        self.assertEqual(plugin.name, "prefixer")
        self.assertEqual(plugin.hooks, (blablablars.HOOK_INPUT,))

        result = blablablars.Pipeline([plugin]).run("hey")
        self.assertEqual(result.analysis.text, ">> hey")

    def test_handler_returning_none_leaves_the_payload_alone(self) -> None:
        class Observer(blablablars.Plugin):
            name = "observer"
            hooks = (blablablars.HOOK_OUTPUT,)
            seen: list[str] = []

            def call(self, hook_name: str, payload: dict) -> None:
                self.seen.append(hook_name)
                return None

        observer = Observer()
        result = blablablars.Pipeline([observer]).run("unchanged")

        self.assertEqual(result.analysis.text, "unchanged")
        self.assertEqual(result.trace, ["observer: pipeline.output"])
        self.assertEqual(observer.seen, ["pipeline.output"])

    def test_output_extensions_can_rewrite_the_analysis(self) -> None:
        class Redact(blablablars.Plugin):
            name = "redact"
            hooks = (blablablars.HOOK_OUTPUT,)

            def call(self, hook_name: str, payload: dict) -> dict:
                payload["analysis"]["text"] = "[redacted]"
                return payload

        result = blablablars.Pipeline([Redact()]).run("secret")
        self.assertEqual(result.analysis.text, "[redacted]")

    def test_an_exception_in_a_handler_surfaces_as_a_runtime_error(self) -> None:
        class Broken(blablablars.Plugin):
            name = "broken"
            hooks = (blablablars.HOOK_INPUT,)

            def call(self, hook_name: str, payload: dict) -> dict:
                raise ValueError("nope")

        pipeline = blablablars.Pipeline([Broken()])
        with self.assertRaises(RuntimeError) as raised:
            pipeline.run("hi")
        self.assertIn("nope", str(raised.exception))

    def test_a_handler_returning_invalid_payload_is_reported(self) -> None:
        class DropsText(blablablars.Plugin):
            name = "drops-text"
            hooks = (blablablars.HOOK_INPUT,)

            def call(self, hook_name: str, payload: dict) -> dict:
                return {"text": 42}

        with self.assertRaises(RuntimeError):
            blablablars.Pipeline([DropsText()]).run("hi")


if __name__ == "__main__":
    unittest.main()
