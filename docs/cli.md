# The command line

`blablablars` has one way to name a plugin, one place to keep a list of them, and
no other discovery. This page is the reference for all three; the extension
contracts themselves are in [`extensions.md`](extensions.md).

```bash
blablablars run [TEXT] [--json] [attachment flags]   # analyse text
blablablars hooks [--json]                           # the hook vocabulary
blablablars plugins [attachment flags] [--json]       # what this run attaches
blablablars plugin add|list|remove|path               # the plugin list
blablablars dictate [--toggle|--seconds N|--input F]  # record, transcribe, type
blablablars config init|show|path                     # the dictation config
blablablars models [--json]                           # models that were found
blablablars engines [--json]                          # backends, installed or not
blablablars version [--json]
```

`plugins` (plural) shows what is *attached to this invocation*; `plugin`
(singular) manages what *every invocation* reads. Both accept `--json`.

## Plugin specs

A spec is what you type, paste into an environment variable, commit in a plugin
list, or pass to `Pipeline.load` in Python:

```
SPEC ::= [ KIND ":" ] TARGET
KIND ::= rust | python | process
```

| spec | loads | target is |
| --- | --- | --- |
| `rust:PATH` | a `cdylib` into this process | `.so` / `.dylib` / `.dll` |
| `python:PATH` | a `.py` file in the embedded interpreter | a file |
| `python:MODULE` | an installed module in the embedded interpreter | e.g. `my_company.audit` |
| `process:COMMAND` | a child process speaking JSON on stdio | a command line |

`KIND` is case-insensitive and only those three words are kinds, so a target
that merely contains a colon — `C:\plugins\x.dll`, `https://…/p.py` — is left
alone.

**A spec without a prefix** is read by its shape:

| target | read as |
| --- | --- |
| contains whitespace | `process` (it is a command line) |
| ends in `.so`, `.dylib`, `.dll` | `rust` |
| ends in `.py` | `python` |
| ends in `.exe`, `.bat`, `.cmd`, `.sh`, `.ps1` | `process` |
| a dotted module name, no `/` or spaces | `python` |
| anything else | `process` |

Inference is a convenience for one-off flags. `blablablars plugin add` stores the
canonical prefixed form, so a plugin list never depends on it, and a path
containing spaces must use a prefix.

## Attaching plugins

`run` and `plugins` take the same flags:

| flag | meaning |
| --- | --- |
| `--plugin SPEC`, `-p SPEC` | attach one plugin (repeatable) |
| `--plugin-path DIR` | attach every `*.so`/`*.dylib`/`*.dll`/`*.py` in a directory, in name order (repeatable) |
| `--plugin-config FILE` | read this plugin list instead of the user's |
| `--no-plugins` | ignore the environment and the plugin list |
| `--python-path DIR` | add to the embedded interpreter's `sys.path` (repeatable) |

Sources are attached in this order, and the order matters because extensions run
in registration order:

1. `--plugin` specs, as given.
2. `$BLABLABLARS_PLUGINS`, one spec per line.
3. The plugin list (`blablablars plugin add` writes it).
4. `--plugin-path` directories.
5. `$BLABLABLARS_PLUGIN_PATH` directories (`PATH`-separated).

Steps 2, 3 and 5 are the *implicit* sources; `--no-plugins` skips them and keeps
steps 1 and 4. Nothing is ever discovered by walking the current directory: a
checkout cannot execute code just because you ran `blablablars` inside it. A
project that wants a fixed plugin list commits one and passes it:

```bash
blablablars plugin add --config ./blablablars.plugins python:tools/blablablars_plugin.py
blablablars run "..." --plugin-config ./blablablars.plugins
```

| environment variable | meaning |
| --- | --- |
| `BLABLABLARS_PLUGINS` | specs, one per line (same format as the plugin list) |
| `BLABLABLARS_PLUGIN_PATH` | directories to scan, `PATH`-separated |
| `BLABLABLARS_PLUGIN_CONFIG` | the plugin list to read instead of the user's |
| `BLABLABLARS_PYTHON_PATH` | extra `sys.path` entries, `PATH`-separated |

## The plugin list

One spec per line; blank lines and lines starting with `#` are ignored, and
there are no inline comments (a command line may contain `#`). The user's list
lives at `$XDG_CONFIG_HOME/blablablars/plugins`, then `%APPDATA%\blablablars\plugins`
on Windows, then `~/.config/blablablars/plugins`.

```bash
blablablars plugin add rust:/opt/blablablars/libshout.so   # absolute, prefixed
blablablars plugin add python:my_company.audit             # a module
blablablars plugin add --config ./blablablars.plugins process:./blab-worker
blablablars plugin list            # one spec per line; --json for the details
blablablars plugin list --json     # {"config": ..., "plugins": [{"spec","kind","target"}]}
blablablars plugin remove rust:/opt/blablablars/libshout.so
blablablars plugin path            # the file that would be read
```

`add` canonicalises: a file target becomes an absolute path, the kind is written
out, an unreadable file is rejected immediately, and a spec already in the list
is not added twice. `remove` matches the stored form but accepts the original
(`./libshout.so` still removes `rust:/abs/libshout.so`), keeps every other line
and comment, and fails when nothing matched so scripts can notice.

`blablablars plugins` is the check: it attaches everything and reports each
extension's name and hooks, which is exactly what a start-up would see.

## Dictation

`dictate` is the voice side of the same binary: record, transcribe through a
backend, clean up, and type into the focused window. Its flags are shortcuts for
config settings, and the whole feature is documented in
[`dictation.md`](dictation.md).

| flag | meaning |
| --- | --- |
| `--toggle` | start a detached recording, or stop the one running |
| `--seconds N` | record for a fixed time |
| `--input FILE` | transcribe a WAV file instead of recording |
| `--config FILE` | the dictation config to read |
| `--model NAME` | override `models.default` for this run |
| `--language LANG` | override `language` (`auto` detects it) |
| `--output KIND` | `auto`, `paste`, `stdout`, `command`, `xdotool`, `wtype` |
| `--print` | print instead of typing (same as `--output stdout`) |
| `--no-cleanup` | type the transcript exactly as the model produced it |
| `--stream` | type the words as they settle instead of at the end |
| `--stream-step MS` | how often a streaming session transcribes |
| `--stream-window MS` | how much trailing audio a step shows the model |
| `--json` | report the run instead of typing it (always batch) |

`dictate` accepts the plugin flags above, so extensions can rewrite the
transcript through the `transcription.raw` and `transcription.text` hooks.

`config init [--path FILE] [--force]` writes a config with every default in it,
`config show` prints the config in effect, and `config path` prints the file that
would be read. `models` lists what is in `models.directory`; `--json` gives
`{"directory", "default", "models": [{"name","file","bytes","size","layout","default"}]}`.
`engines` reports the transcription backends — whether each program is installed,
which one the config selects, and how many of the downloaded models it can read;
`--json` gives `{"config", "default_model", "models_directory", "engines":
[{"kind","label","program","installed","configured","runnable_models"}]}`.

## Adding a transport (for maintainers)

The grammar, not the flag list, is the extension point:

1. Add a unit variant to `PluginKind` in `crates/blablablars-core/src/spec.rs`
   and a name to its `as_str`; the parser, `plugin list`, `--json` output and
   Python's `Pipeline.load` all follow from that one place.
2. Teach `plugins::load` in `crates/blablablars-cli/src/plugins.rs` to build that
   kind (one match arm), and `Pipeline.load` in `python/blablablars/pipeline.py`
   if Python can attach it too.
3. Add the kind to the table above and to `docs/extensions.md`.

No new flag, environment variable or config syntax is needed, which is the
point: `--plugin`, `BLABLABLARS_PLUGINS` and the plugin list keep their meaning.

## Stability

The spec grammar, the flag names in the tables above, the environment variable
names, the plugin-list format, the config keys named in
[`dictation.md`](dictation.md) and the JSON shapes of `plugins --json`,
`plugin list --json`, `models --json` and `engines --json` are the long-term
interface. New transports arrive as new kinds; new backends arrive as new
`transcriber.kind` values; existing specs and configs keep loading.
