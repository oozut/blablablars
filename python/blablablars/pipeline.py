"""The Python-facing pipeline wrapper."""

from __future__ import annotations

import json
import os
from typing import Any, Iterable

from ._native import Pipeline as _NativePipeline
from ._native import Run
from .plugin import HOOKS, Plugin, coerce_plugin


class Pipeline:
    """Run the Rust core and attach Python extensions.

    The heavy lifting happens in ``blablablars-core``; this class only converts
    payloads to and from JSON at the native boundary, which is the same shape the
    out-of-process plugin protocol uses.
    """

    def __init__(self, plugins: Iterable[Any] | None = None) -> None:
        self._native = _NativePipeline()
        for plugin in plugins or ():
            self.register(plugin)

    @classmethod
    def with_plugins(cls, *plugins: Any) -> "Pipeline":
        return cls(plugins)

    # -- extensions -----------------------------------------------------
    def register(self, plugin: Any, name: str | None = None) -> None:
        """Register a plugin, a duck-typed extension, or a module of handlers."""
        resolved = coerce_plugin(plugin, name=name)
        extension_name = name or getattr(resolved, "name", type(resolved).__name__)
        hooks = tuple(getattr(resolved, "hooks", HOOKS))
        call = getattr(resolved, "call", None)
        if call is None:
            raise TypeError(
                f"{type(resolved).__name__} has no `call` method; subclass "
                "blablablars.Plugin or decorate functions with @hook"
            )

        def callback(hook_name: str, payload_json: str) -> str:
            payload = json.loads(payload_json)
            result = call(hook_name, payload)
            return json.dumps(payload if result is None else result)

        self._native.register(extension_name, list(hooks), callback)

    def register_rust(self, path: str | os.PathLike[str]) -> None:
        """Load a compiled Rust extension (``.so``/``.dylib``/``.dll``) in-process.

        Same loader as ``blablablars run --rust-plugin``; the extension sees the
        same hooks as a Python one.
        """
        self._native.register_rust_library(os.fspath(path))

    def register_rust_directory(self, directory: str | os.PathLike[str]) -> list[str]:
        """Load every Rust extension in ``directory``. Returns their names."""
        return list(self._native.register_rust_directory(os.fspath(directory)))

    def register_process(self, command: str) -> str:
        """Attach a plugin that runs as its own process and speaks JSON on stdio.

        ``command`` is a command line, e.g.
        ``"python3 -m blablablars.runtime my_plugin.py"``.
        """
        return self._native.register_process(command)

    def unregister(self, name: str) -> bool:
        return self._native.unregister(name)

    @property
    def extensions(self) -> list[str]:
        """Extension names in dispatch order."""
        return self._native.extensions()

    @property
    def hooks(self) -> tuple[str, ...]:
        """Hooks an extension may subscribe to."""
        return tuple(self._native.hooks())

    def describe(self) -> list[tuple[str, list[str]]]:
        return list(self._native.describe())

    # -- execution ------------------------------------------------------
    def run(self, text: str) -> Run:
        """Analyse ``text`` through the pipeline."""
        return self._native.run_text(text)

    def run_json(self, payload: dict) -> dict:
        """Analyse ``{"text": ...}`` and return ``{"analysis": ..., "trace": [...]}``."""
        return json.loads(self._native.run_json(json.dumps(payload)))

    def __repr__(self) -> str:
        return f"Pipeline(extensions={self.extensions!r})"


__all__ = ["Pipeline", "HOOKS", "Plugin", "Run"]
