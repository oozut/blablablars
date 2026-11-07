"""Example blablablars plugin.

Run it through the Rust application::

    blablablars run "hello world" \
        --plugin "python -m blablablars.runtime examples/python/hello_plugin.py"

Or in-process::

    import blablablars
    pipeline = blablablars.Pipeline()
    pipeline.register(blablablars.plugin_from_namespace(__import__("hello_plugin")))
"""

from blablablars import HOOK_INPUT, HOOK_OUTPUT, hook


@hook(HOOK_INPUT)
def shout(payload: dict) -> dict:
    """Upper-case the text before the core analyses it."""
    payload["text"] = payload["text"].upper()
    return payload


@hook(HOOK_OUTPUT)
def mark_as_shouted(payload: dict) -> dict:
    """Annotate the analysis after the core produced it."""
    payload["analysis"]["text"] = f"[shouted] {payload['analysis']['text']}"
    return payload
