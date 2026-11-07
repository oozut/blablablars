"""Write an extension in Python.

Two styles are supported, and they compose:

Declarative — decorate functions, and the plugin reports only the hooks it uses:

    from blablablars import HOOK_INPUT, hook

    @hook(HOOK_INPUT)
    def shout(payload):
        payload["text"] = payload["text"].upper()
        return payload

Imperative — subclass :class:`Plugin` and override :meth:`Plugin.call`:

    class Upper(Plugin):
        name = "upper"
        hooks = (HOOK_INPUT,)

        def call(self, hook_name, payload):
            ...

A handler may return ``None`` to leave the payload untouched; a returned dict
replaces the payload for the next extension in the chain.
"""

from __future__ import annotations

from typing import Any, Callable, Iterable, Mapping

from ._native import HOOK_INPUT, HOOK_OUTPUT, PROTOCOL_VERSION

HOOKS: tuple[str, ...] = (HOOK_INPUT, HOOK_OUTPUT)
"""Every hook an extension may subscribe to."""

Handler = Callable[[dict], "dict | None"]
HookHandlers = Mapping[str, Iterable[Handler]]


def _validate_hooks(names: Iterable[str]) -> tuple[str, ...]:
    validated = tuple(names)
    if not validated:
        raise ValueError("a @hook decorator needs at least one hook name")
    unknown = [name for name in validated if name not in HOOKS]
    if unknown:
        raise ValueError(f"unknown hook(s) {unknown}; known hooks are {list(HOOKS)}")
    return validated


def hook(*names: str) -> Callable[[Callable], Callable]:
    """Mark a function as a handler for one or more hooks."""

    validated = _validate_hooks(names)

    def decorator(function: Callable) -> Callable:
        existing = tuple(getattr(function, "__blablablars_hooks__", ()))
        function.__blablablars_hooks__ = existing + validated
        return function

    return decorator


class Plugin:
    """Base class for an extension.

    Subclasses that decorate methods with :func:`hook` get a working
    :meth:`call` for free; the handlers run in definition order.
    """

    name: str = "plugin"
    hooks: tuple[str, ...] = HOOKS
    version: str | None = None
    _handlers: dict[str, list[Callable]] = {}

    def __init_subclass__(cls, **kwargs: Any) -> None:
        super().__init_subclass__(**kwargs)
        handlers: dict[str, list[Callable]] = {}
        for klass in reversed(cls.__mro__):
            for value in vars(klass).values():
                for hook_name in getattr(value, "__blablablars_hooks__", ()):
                    handlers.setdefault(hook_name, []).append(value)
        cls._handlers = handlers
        if handlers:
            cls.hooks = tuple(handlers)

    def call(self, hook_name: str, payload: dict) -> dict | None:
        """Run this plugin's handlers for ``hook_name``."""
        for handler in self._handlers.get(hook_name, ()):
            result = handler(self, payload)
            if result is not None:
                payload = result
        return payload

    def describe(self) -> dict:
        """The ``describe`` result this plugin advertises over the protocol."""
        return {
            "protocol": PROTOCOL_VERSION,
            "name": self.name,
            "hooks": list(self.hooks),
            **({"version": self.version} if self.version else {}),
        }

    def __repr__(self) -> str:  # pragma: no cover - debugging aid
        return f"<{type(self).__name__} name={self.name!r} hooks={list(self.hooks)!r}>"


class FunctionPlugin(Plugin):
    """A plugin assembled from functions, usually via :func:`plugin_from_namespace`."""

    def __init__(self, name: str, handlers: HookHandlers, version: str | None = None) -> None:
        self.name = name
        self.version = version
        self._handlers = {
            hook_name: list(functions) for hook_name, functions in handlers.items()
        }
        self.hooks = tuple(self._handlers)

    def call(self, hook_name: str, payload: dict) -> dict | None:
        for function in self._handlers.get(hook_name, ()):
            result = function(payload)
            if result is not None:
                payload = result
        return payload


def collect_handlers(namespace: Any) -> dict[str, list[Callable]]:
    """Collect ``@hook``-decorated callables from a module or arbitrary object."""
    handlers: dict[str, list[Callable]] = {}
    for attribute, raw in vars(namespace).items():
        # Resolve descriptors so `@staticmethod` / `@classmethod` handlers work.
        try:
            value = getattr(namespace, attribute)
        except AttributeError:  # pragma: no cover - exotic namespaces
            value = raw
        for hook_name in getattr(value, "__blablablars_hooks__", ()):
            if callable(value):
                handlers.setdefault(hook_name, []).append(value)
    return handlers


def plugin_from_namespace(
    namespace: Any,
    *,
    name: str | None = None,
    version: str | None = None,
) -> FunctionPlugin:
    """Build a plugin from an object (usually a module) holding ``@hook`` functions."""
    handlers = collect_handlers(namespace)
    if not handlers:
        raise TypeError(
            "no @hook-decorated functions found; decorate functions with "
            "blablablars.hook(...) or subclass blablablars.Plugin"
        )
    resolved = (
        name
        or getattr(namespace, "__blablablars_name__", None)
        or getattr(namespace, "__name__", None)
        or type(namespace).__name__
    )
    return FunctionPlugin(resolved, handlers, version=version)


def coerce_plugin(candidate: Any, *, name: str | None = None) -> Plugin:
    """Accept a :class:`Plugin`, a duck-typed extension, or a module of handlers."""
    if isinstance(candidate, Plugin):
        return candidate
    if callable(getattr(candidate, "call", None)):
        return candidate  # duck-typed: anything with name/hooks/call
    return plugin_from_namespace(candidate, name=name)
