"""Plugin discovery.

Three sources, in order:

1. explicitly passed entry points (used by tests),
2. the ``blablablars.plugins`` entry-point group,
3. the specs listed in ``$BLABLABLARS_PLUGINS``, one per line.

An entry point must load either a plugin object (anything with ``name``,
``hooks`` and ``call``) or a module whose functions are decorated with
:func:`blablablars.hook`.

``$BLABLABLARS_PLUGINS`` holds the same lines the ``blablablars`` binary reads,
so one line feeds both: ``python:my_plugin.py``, ``python:my_company.audit``
and a bare ``my_plugin.py`` are understood here. Specs for other transports
(``rust:``, ``process:``) belong to the binary and are skipped.
"""

from __future__ import annotations

import importlib.util
import os
from pathlib import Path
from typing import Any, Iterable

from ._native import plugin_spec as _plugin_spec
from .plugin import coerce_plugin

ENTRY_POINT_GROUP = "blablablars.plugins"
ENV_VAR = "BLABLABLARS_PLUGINS"


def load_plugin_file(path: str | os.PathLike[str], *, name: str | None = None) -> Any:
    """Import a ``.py`` file as a plugin module.

    Modules are imported under a private name so two plugins cannot collide in
    ``sys.modules``.
    """
    file = Path(path).expanduser().resolve()
    if not file.is_file():
        raise FileNotFoundError(f"plugin file not found: {file}")
    module_name = name or f"_blablablars_plugin_{abs(hash(str(file)))}"
    spec = importlib.util.spec_from_file_location(module_name, file)
    if spec is None or spec.loader is None:
        raise ImportError(f"{file} is not an importable Python module")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    # The private module name would otherwise leak into the plugin's `name`;
    # the file stem is what a user recognises in `blablablars plugins`.
    module.__blablablars_name__ = name or file.stem
    return module


def load_plugins(entry_points: Iterable[Any] | None = None) -> list[Any]:
    """Discover and load every installed plugin, plus ``$BLABLABLARS_PLUGINS``."""
    found: list[Any] = []

    if entry_points is None:
        from importlib.metadata import entry_points as discover

        discovered = discover()
        selected = (
            discovered.select(group=ENTRY_POINT_GROUP)
            if hasattr(discovered, "select")
            else discovered.get(ENTRY_POINT_GROUP, ())  # Python < 3.10
        )
    else:
        selected = entry_points

    for entry_point in selected:
        loaded = entry_point.load()
        found.append(coerce_plugin(loaded, name=getattr(entry_point, "name", None)))

    for raw in os.environ.get(ENV_VAR, "").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        kind, target = _plugin_spec(line)
        if kind != "python":
            continue
        found.append(coerce_plugin(load_plugin_file(target)))

    return found


__all__ = ["ENTRY_POINT_GROUP", "ENV_VAR", "load_plugin_file", "load_plugins"]
