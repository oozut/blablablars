# Dictation

Press a shortcut, say something, and the words appear in whatever has focus.

```bash
blablablars dictate --toggle      # the shortcut: press to start, press to stop
blablablars dictate --seconds 10  # record ten seconds, then transcribe
blablablars dictate --input take.wav --print
```

Everything below the shortcut is four replaceable pieces joined in one place:

```
capture ──► transcribe ──► clean ──► deliver
(child       (whisper-cli   (pure       (xdotool / wtype /
 process,     or any        functions)   your command / stdout)
 raw PCM)     command)
```

Each piece is behind a trait or a config entry, so a different microphone
backend, model family or output tool never touches the others. The audio
contract between them is one shape everywhere: **mono 16-bit PCM at 16 kHz**.

## Quick start

```bash
# 1. a model, in the standard location
mkdir -p ~/.local/share/blablablars/models
curl -L -o ~/.local/share/blablablars/models/ggml-small.bin \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin

# 2. a config with every setting written out (optional — defaults apply anyway)
blablablars config init

# 3. what it found
blablablars models

# 4. try it without a microphone, or with one
blablablars dictate --input sample.wav --print
blablablars dictate            # records until Enter
```

Then bind `blablablars dictate --toggle` to a key:

| desktop | how |
| --- | --- |
| GNOME | Settings → Keyboard → Keyboard Shortcuts → Custom Shortcuts → `+` |
| KDE | System Settings → Shortcuts → Custom Shortcuts → `+` |
| sway | `bindsym $mod+d exec blablablars dictate --toggle` |
| Hyprland | `bind = SUPER, D, exec, blablablars dictate --toggle` |
| i3 | `bindsym $mod+d exec --no-startup-id blablablars dictate --toggle` |
| Windows / macOS | any "run a command" shortcut tool; `--toggle` is still just a command |

The first press starts a detached recorder and returns immediately, so the
shortcut is responsive. The second press stops it, transcribes, types, and exits.
The session lives on a Unix socket in `$XDG_RUNTIME_DIR/blablablars/`, so a
crash cannot leave a stale pid file behind.

## The config file

`$XDG_CONFIG_HOME/blablablars/config.yml` (`%APPDATA%\blablablars\config.yml` on
Windows, `~/.config/blablablars/config.yml` otherwise), or `$BLABLABLARS_CONFIG`,
or `--config FILE`. Unknown keys are an error: a silent `outptu:` is a bug
report waiting to happen.

```yaml
language: en                    # or `auto` to detect it

capture:
  kind: auto                    # auto | command
  argv: []                      # for `kind: command`

models:
  directory: ~/.local/share/blablablars/models
  default: whisper-large-v3-turbo

transcriber:
  kind: whisper-cli             # whisper-cli | command
  binary: whisper-cli
  argv: []                      # for `kind: command`
  extra_args: []
  gpu: auto                     # auto | off
  threads: null                 # whisper-cli `-t`; unset leaves it to the backend
  timeout_seconds: 180

cleanup: true                   # true | false | a table of individual switches

output:
  kind: auto                    # auto | stdout | command | xdotool | wtype
  argv: []                      # for `kind: command`, with a {text} placeholder
  trailing: "\n"                # appended after the text (empty string: none)

feedback:
  command: []                   # a program run as recording/transcribing/done/error
```

`blablablars config show` prints the config as it is in effect, defaults
included; `blablablars config path` prints the file that would be read.

### Cleanup

`cleanup: true` runs the default rules; `cleanup: false` types the transcript as
the model produced it; a table switches them one by one:

```yaml
cleanup:
  artifacts: true          # [BLANK_AUDIO], [Music], ♪, >>, (applause)
  fillers: true            # um, uh, erm…
  repeated_words: true     # "the the" — a stutter or a duplicated prediction
  punctuation: true        # spacing around punctuation, sentence capitals
  spoken_punctuation: false # "comma", "full stop" typed as punctuation
  terminal: "."            # ensure the text ends like a sentence; "" to leave it
  preserve_case: false     # keep the model's casing
```

Fillers are kept to words that are never also real words: `like`, `so` and
`right` are left alone. A bracketed marker is only removed when it is a shouty
tag (`[BLANK_AUDIO]`) or a known non-speech word — `[1]` and `[a side note]`
survive.

## Models

Any `*.bin` whisper.cpp model in `models.directory` is discovered, in name
order:

```bash
blablablars models          # name, size, file; the default is marked with *
blablablars models --json   # {"directory": …, "default": …, "models": [...]}
```

Names are matched leniently: `whisper-large-v3-turbo`, `large-v3-turbo` and
`ggml-large-v3-turbo-q5_0.bin` all reach the same file, an exact match beats a
prefix (`small.en` never silently becomes `small.en-tdrz`), and an ambiguous name
is an error that lists the candidates. `--model NAME` overrides
`models.default` for one run, and a path works too.

A missing model is not a silent failure: the message names the directory and
prints the `curl` line that fills it.

### Another model family

`transcriber.kind: command` makes any program a backend, with `{audio}`,
`{model}`, `{model_name}` and `{language}` substituted:

```yaml
transcriber:
  kind: command
  argv: ["parakeet-cli", "--model", "{model}", "{audio}"]
```

stdout is the transcript. That is how a streaming engine, a remote API, or a
model that is not whisper.cpp is wired in — no new Rust code, no new flag.

## Output

| kind | what it does |
| --- | --- |
| `auto` | `wtype` on Wayland, `xdotool` on X11 |
| `stdout` | prints; use this in a terminal or a script |
| `xdotool` | `xdotool type --clearmodifiers --delay 0 -- TEXT` |
| `wtype` | `wtype -- TEXT` |
| `command` | runs `argv`, substituting `{text}`; without a placeholder the text is the last argument |

`auto` prefers Wayland *before* X11, because `DISPLAY` is set under Xwayland even
when `xdotool` cannot reach the focused window. When neither tool is installed
the run fails and says what to install and that `output.kind: stdout` always
works — no silent no-op.

A command example that pastes through the clipboard:

```yaml
output:
  kind: command
  argv: ["sh", "-c", "printf '%s' \"$1\" | wl-copy", "sh", "{text}"]
```

## Extensions

Two hooks sit around the cleanup rules, both with a `{"text": …}` payload:

| hook | sees | use it for |
| --- | --- | --- |
| `transcription.raw` | the model's text, with `audio_seconds`, `model`, `transcriber` | dictionaries, language fixes, jargon |
| `transcription.text` | the cleaned text | rewriters, an LLM pass, formatting |

Returning `None` leaves the text alone; returning a string replaces it. The order
is `audio → transcribe → transcription.raw → cleanup → transcription.text →
deliver`, and `--json` reports the trace so a plugin that changes nothing is
visible as such. Attach plugins with the same specs as everything else:

```bash
blablablars dictate --plugin rust:./libshout.so --print
blablablars plugin add python:my_company.dictionary   # for every run
```

`--json` prints the whole run instead of typing it:

```json
{
  "text": "Hello there.",
  "raw": "um, hello there",
  "model": "small",
  "transcriber": "whisper-cli (ggml-small.bin)",
  "audio_seconds": 1.0,
  "elapsed_ms": 1204,
  "trace": []
}
```

## Latency

Dictation is a latency feature, so the budget matters:

* **Model size.** `tiny.en` and `base.en` are for interactive use on a weak CPU;
  `small` is the usual sweet spot; a `large-v3-turbo` quantised to `q5_0` is the
  best quality per second if the machine can carry it.
* **Threads.** `transcriber.threads` becomes `whisper-cli -t N`. On an older CPU
  the difference is large: 4 threads → 65 s, 8 threads → 35 s on the same 11 s
  clip. Set it to the number of physical cores; leaving it unset lets
  whisper.cpp decide.
* **GPU.** `gpu: auto` passes nothing (whisper.cpp uses the GPU its build has)
  and `gpu: off` adds `-ng`. There is no separate GPU binary: a `whisper-cli`
  built with CUDA, Vulkan or Metal is picked up as it is. `blablablars version`
  and `--json` never claim a backend the binary does not have.
* **Streaming** (partial text every few seconds) is a backend property, not a
  shortcut property: run a streaming engine under `transcriber.kind: command` and
  let it print as it goes. The ships-today path is one transcript per press.

## Troubleshooting

| symptom | cause |
| --- | --- |
| `no models in …` | the directory is empty; the message prints a `curl` line |
| `nothing was recorded` | the capture program produced no bytes: no microphone, muted input, or a wrong device |
| `no soundcards found` from `arecord` | ALSA sees nothing; PipeWire/PulseAudio users need `parecord`/`pw-record`, which `capture.kind: auto` already prefers |
| `cannot type into the focused window` | neither `xdotool` nor `wtype` is installed; install one or use `output.kind: stdout` |
| the wrong words | try a bigger model, set `language`, or add a dictionary through `transcription.raw` |
| the first press says "already running" | a session is genuinely running; press the shortcut again, or remove the stale socket in `$XDG_RUNTIME_DIR/blablablars/` |
| `--toggle` on Windows | unsupported (no Unix sockets): use `--seconds N` |

`--record-child` is what `--toggle` runs; it is hidden and not part of the
interface. The detached recorder's diagnostics go to
`$XDG_STATE_HOME/blablablars/dictate.log`.
