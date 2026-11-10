"""blablablars — the Rust core from Python, with plugins.

The native module ``blablablars._native`` is a PyO3 extension that wraps
``blablablars-core``. This package adds the ergonomic API:

* :class:`blablablars.Pipeline` — run the core, register extensions;
* :class:`blablablars.Plugin` / :func:`blablablars.hook` — write extensions;
* :func:`blablablars.load_plugins` — discover installed extensions;
* ``python -m blablablars.runtime plugin.py`` — run a plugin for the Rust binary.
"""

from ._native import (
    HOOK_INPUT,
    HOOK_OUTPUT,
    PROTOCOL_VERSION,
    Analysis,
    Run,
    __version__,
    analyse,
    hooks,
)
from .pipeline import Pipeline
from .plugin import FunctionPlugin, Plugin, coerce_plugin, hook, plugin_from_namespace
from .plugins import ENTRY_POINT_GROUP, ENV_VAR, load_plugin_file, load_plugins

__all__ = [
    "Analysis",
    "ENTRY_POINT_GROUP",
    "ENV_VAR",
    "FunctionPlugin",
    "HOOK_INPUT",
    "HOOK_OUTPUT",
    "PROTOCOL_VERSION",
    "Pipeline",
    "Plugin",
    "Run",
    "__version__",
    "analyse",
    "coerce_plugin",
    "hook",
    "hooks",
    "load_plugin_file",
    "load_plugins",
    "plugin_from_namespace",
]
