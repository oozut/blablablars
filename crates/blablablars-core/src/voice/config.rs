//! The YAML config: one file that says how recording, transcription, cleanup
//! and typing are done.
//!
//! Everything here has a default that works on a fresh machine apart from the
//! model itself, so a user can start with `blablablars config init`, edit the
//! model name, and go. Unknown keys are rejected: a config is read by humans,
//! and a silently ignored `outptu:` is a bug report waiting to happen.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::cleanup::CleanupOptions;
use super::platform;
use crate::error::CoreError;

/// Environment variable pointing at an alternative config file.
pub const ENV_CONFIG: &str = "BLABLABLARS_CONFIG";

/// Where capture audio comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureKind {
    /// Use `parecord`, `pw-record` or `arecord`, whichever is installed.
    #[default]
    Auto,
    /// Run `argv`, which must write raw 16-bit mono 16 kHz PCM to stdout.
    Command,
}

/// How audio is captured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    pub kind: CaptureKind,
    /// Device name, passed to the capture program as it expects it.
    pub device: Option<String>,
    /// Used by `kind: command`: the program that writes raw PCM to stdout.
    pub argv: Vec<String>,
    /// Safety stop, so a forgotten session cannot fill memory.
    pub max_seconds: f64,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            kind: CaptureKind::Auto,
            device: None,
            argv: Vec::new(),
            max_seconds: 600.0,
        }
    }
}

/// Where models live and which one to use when nothing overrides it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelsConfig {
    /// A folder scanned for `*.bin` model files. `~` is expanded.
    pub directory: PathBuf,
    /// Model name, resolved inside [`ModelsConfig::directory`].
    pub default: String,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            directory: default_model_directory(),
            default: "whisper-large-v3-turbo".to_string(),
        }
    }
}

/// Which transcription backend runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TranscriberKind {
    /// The `whisper-cli` binary from whisper.cpp: ggml `*.bin` models, CPU or
    /// whatever GPU that build was compiled for.
    #[default]
    WhisperCli,
    /// The `parakeet-cli` binary from the same ggml toolchain: NVIDIA Parakeet
    /// TDT models, which are markedly faster than Whisper on a CPU. Its ggml
    /// files are read exactly like whisper's.
    Parakeet,
    /// `sherpa-onnx`, the ONNX Runtime toolchain: Whisper ONNX, NVIDIA Parakeet
    /// ONNX and the other transducer families. The model is a *folder* holding
    /// `tokens.txt` plus encoder/decoder (and for transducers a joiner).
    SherpaOnnx,
    /// Any program: `argv` is a template with `{audio}`, `{model}`,
    /// `{model_name}` and `{language}` placeholders, and its stdout is the
    /// transcript. This is how `faster-whisper`, a Python `onnxruntime` script,
    /// a remote API or a streaming engine is wired in without touching this
    /// crate.
    Command,
}

impl TranscriberKind {
    /// Every backend, in the order `blablablars engines` lists them.
    pub const ALL: [TranscriberKind; 4] = [
        Self::WhisperCli,
        Self::Parakeet,
        Self::SherpaOnnx,
        Self::Command,
    ];

    /// The program that runs this kind when the config does not name one.
    pub fn default_binary(self) -> &'static str {
        match self {
            Self::WhisperCli => "whisper-cli",
            Self::Parakeet => "parakeet-cli",
            Self::SherpaOnnx => "sherpa-onnx-offline",
            Self::Command => "",
        }
    }

    /// Whether the backend needs a real model on disk before it can run.
    ///
    /// A command backend may name something that is not a local file (a remote
    /// model id, a streaming engine's own notion of a model), so it is allowed
    /// to go unresolved.
    pub fn needs_model_file(self) -> bool {
        !matches!(self, Self::Command)
    }

    /// The word `blablablars engines` prints beside each backend.
    pub fn label(self) -> &'static str {
        match self {
            Self::WhisperCli => "whisper.cpp (ggml models, CPU or GPU)",
            Self::Parakeet => "NVIDIA Parakeet TDT through the ggml toolchain",
            Self::SherpaOnnx => "ONNX Runtime through sherpa-onnx (Whisper, Parakeet, transducers)",
            Self::Command => "any program that prints the transcript",
        }
    }
}

/// Whether to ask the backend for GPU compute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GpuMode {
    /// Let the backend decide; whisper.cpp uses the GPU it was built for.
    #[default]
    Auto,
    /// Explicitly disable GPU compute.
    Off,
}

/// How the transcript is produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TranscriberConfig {
    pub kind: TranscriberKind,
    /// Program name or path. Unset means the one this kind ships with
    /// (`whisper-cli`, `parakeet-cli`, `sherpa-onnx-offline`).
    pub binary: Option<String>,
    /// Full template for `kind: command`.
    pub argv: Vec<String>,
    /// Extra arguments appended after the built-in ones.
    pub extra_args: Vec<String>,
    pub gpu: GpuMode,
    /// CPU threads for the ggml backends and sherpa-onnx. Unset lets the backend
    /// decide, which is a safe default; dictation on an older CPU is noticeably
    /// faster with it set to the number of physical cores.
    pub threads: Option<u32>,
    /// A hung model must not hang the keyboard shortcut for ever.
    pub timeout_seconds: u64,
}

impl Default for TranscriberConfig {
    fn default() -> Self {
        Self {
            kind: TranscriberKind::WhisperCli,
            binary: None,
            argv: Vec::new(),
            extra_args: Vec::new(),
            gpu: GpuMode::Auto,
            threads: None,
            timeout_seconds: 180,
        }
    }
}

impl TranscriberConfig {
    /// The program this config runs, defaulted per kind.
    pub fn program(&self) -> String {
        self.binary
            .clone()
            .filter(|binary| !binary.trim().is_empty())
            .unwrap_or_else(|| self.kind.default_binary().to_string())
    }
}

/// Where the text goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputKind {
    /// Type into the focused window, choosing the tool from the session type:
    /// `xdotool` on X11, `wtype` on Wayland.
    #[default]
    Auto,
    /// Print it, do not type it. Useful in a terminal and in scripts.
    Stdout,
    /// Run `argv`, which is a template with a `{text}` placeholder.
    Command,
    /// `xdotool type`, for X11 sessions.
    Xdotool,
    /// `wtype`, for Wayland compositors that support the virtual-keyboard
    /// protocol (sway, hyprland, river, …).
    Wtype,
    /// Put the text on the clipboard and send the paste keystroke: `pbcopy` +
    /// AppleScript on macOS, `clip.exe` + PowerShell on Windows, `wl-copy` +
    /// `wtype` or `xclip` + `xdotool` on Linux. `auto` picks this on macOS and
    /// Windows, where no command can type text without a helper.
    Paste,
}

/// How the text is delivered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    pub kind: OutputKind,
    /// Template for `kind: command`.
    pub argv: Vec<String>,
    /// Appended after every dictation, e.g. `" "` to keep typing after a word.
    pub trailing: String,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            kind: OutputKind::Auto,
            argv: Vec::new(),
            trailing: String::new(),
        }
    }
}

/// Optional audiovisual feedback, so the user knows a session started.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FeedbackConfig {
    /// Template with a `{state}` placeholder, run with `recording`,
    /// `transcribing`, `done` and `error`. Failures are ignored.
    pub command: Vec<String>,
}

/// `true`/`false` or a table of rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CleanupSetting {
    Enabled(bool),
    Rules(CleanupOptions),
}

impl Default for CleanupSetting {
    fn default() -> Self {
        Self::Rules(CleanupOptions::default())
    }
}

impl CleanupSetting {
    /// The rules to apply, or [`CleanupOptions::none`] when disabled.
    pub fn options(&self) -> CleanupOptions {
        match self {
            Self::Enabled(true) => CleanupOptions::default(),
            Self::Enabled(false) => CleanupOptions::none(),
            Self::Rules(rules) => rules.clone(),
        }
    }
}

/// The whole dictation config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VoiceConfig {
    /// Language code for the model, or `auto` to detect it.
    pub language: String,
    pub capture: CaptureConfig,
    pub models: ModelsConfig,
    pub transcriber: TranscriberConfig,
    pub cleanup: CleanupSetting,
    pub output: OutputConfig,
    pub feedback: FeedbackConfig,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            language: "auto".to_string(),
            capture: CaptureConfig::default(),
            models: ModelsConfig::default(),
            transcriber: TranscriberConfig::default(),
            cleanup: CleanupSetting::default(),
            output: OutputConfig::default(),
            feedback: FeedbackConfig::default(),
        }
    }
}

impl VoiceConfig {
    /// Parse a YAML document.
    pub fn from_yaml(yaml: &str) -> Result<Self, CoreError> {
        let mut config: Self = serde_norway::from_str(yaml)
            .map_err(|error| CoreError::Voice(format!("invalid config: {error}")))?;
        config.models.directory = expand_home(&config.models.directory);
        config.validate()?;
        Ok(config)
    }

    /// Serialise back to YAML, used by `config init`.
    pub fn to_yaml(&self) -> Result<String, CoreError> {
        serde_norway::to_string(self)
            .map_err(|error| CoreError::Voice(format!("cannot write the config: {error}")))
    }

    /// Load `path`, or return the defaults when it does not exist.
    pub fn load(path: &Path) -> Result<Self, CoreError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_yaml(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(CoreError::Voice(format!(
                "cannot read the config `{}`: {error}",
                path.display()
            ))),
        }
    }

    /// `--config`, then `$BLABLABLARS_CONFIG`, then the XDG default, then the
    /// built-in defaults when no file exists.
    pub fn load_resolved(explicit: Option<&Path>) -> Result<(Self, Option<PathBuf>), CoreError> {
        let path = explicit
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os(ENV_CONFIG).map(PathBuf::from))
            .or_else(default_config_path);

        match path {
            Some(path) if path.exists() => Ok((Self::load(&path)?, Some(path))),
            Some(path) if explicit.is_some() || std::env::var_os(ENV_CONFIG).is_some() => Err(
                CoreError::Voice(format!("no config file at `{}`", path.display())),
            ),
            _ => Ok((Self::default(), None)),
        }
    }

    fn validate(&self) -> Result<(), CoreError> {
        if self.models.default.trim().is_empty() {
            return Err(CoreError::Voice(
                "`models.default` is empty: name the model to use".to_string(),
            ));
        }
        if self.capture.max_seconds <= 0.0 {
            return Err(CoreError::Voice(
                "`capture.max_seconds` must be greater than zero".to_string(),
            ));
        }
        if self.transcriber.timeout_seconds == 0 {
            return Err(CoreError::Voice(
                "`transcriber.timeout_seconds` must be greater than zero".to_string(),
            ));
        }
        if self.capture.kind == CaptureKind::Command && self.capture.argv.is_empty() {
            return Err(CoreError::Voice(
                "`capture.kind: command` needs `capture.argv`".to_string(),
            ));
        }
        if self.transcriber.kind == TranscriberKind::Command && self.transcriber.argv.is_empty() {
            return Err(CoreError::Voice(
                "`transcriber.kind: command` needs `transcriber.argv`".to_string(),
            ));
        }
        if self.output.kind == OutputKind::Command && self.output.argv.is_empty() {
            return Err(CoreError::Voice(
                "`output.kind: command` needs `output.argv`".to_string(),
            ));
        }
        Ok(())
    }
}

/// `$XDG_CONFIG_HOME/blablablars/config.yml`, or the platform equivalent.
pub fn default_config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("config.yml"))
}

/// The directory that holds `config.yml` and the plugin list.
pub fn config_dir() -> Option<PathBuf> {
    platform::config_dir()
}

/// Where models are looked for by default: `data_dir()/models`.
pub fn default_model_directory() -> PathBuf {
    platform::data_dir()
        .map(|dir| dir.join("models"))
        .unwrap_or_else(|| PathBuf::from("models"))
}

/// Expand a leading `~` to the user's home directory.
pub use platform::expand_home;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_document_is_the_working_defaults() {
        let config = VoiceConfig::from_yaml("").unwrap();
        assert_eq!(config.language, "auto");
        assert_eq!(config.transcriber.kind, TranscriberKind::WhisperCli);
        assert_eq!(config.models.default, "whisper-large-v3-turbo");
        assert_eq!(config.output.kind, OutputKind::Auto);
        assert_eq!(config.capture.kind, CaptureKind::Auto);
        assert_eq!(config.cleanup.options(), CleanupOptions::default());
    }

    #[test]
    fn a_minimal_config_overrides_only_what_it_names() {
        let config = VoiceConfig::from_yaml(
            r#"
            language: en
            models:
              default: ggml-small.en
            "#,
        )
        .unwrap();

        assert_eq!(config.language, "en");
        assert_eq!(config.models.default, "ggml-small.en");
        // Untouched fields keep their defaults.
        assert_eq!(config.transcriber.program(), "whisper-cli");
        assert_eq!(config.capture.max_seconds, 600.0);
    }

    #[test]
    fn the_whole_surface_parses() {
        let config = VoiceConfig::from_yaml(
            r#"
            language: fr
            capture:
              kind: command
              device: default
              argv: ["parecord", "--raw"]
              max_seconds: 30
            models:
              directory: /models
              default: turbo
            transcriber:
              kind: command
              argv: ["parakeet", "{audio}", "{model}", "{language}"]
              extra_args: ["--fast"]
              gpu: off
              timeout_seconds: 60
            cleanup:
              fillers: false
              spoken_punctuation: true
              terminal: ""
            output:
              kind: command
              argv: ["cat", "{text}"]
              trailing: " "
            feedback:
              command: ["notify-send", "{state}"]
            "#,
        )
        .unwrap();

        assert_eq!(config.capture.kind, CaptureKind::Command);
        assert_eq!(config.capture.max_seconds, 30.0);
        assert_eq!(config.models.directory, PathBuf::from("/models"));
        assert_eq!(config.transcriber.kind, TranscriberKind::Command);
        assert_eq!(config.transcriber.gpu, GpuMode::Off);
        assert!(!config.cleanup.options().fillers);
        assert!(config.cleanup.options().spoken_punctuation);
        assert_eq!(config.output.kind, OutputKind::Command);
        assert_eq!(config.output.trailing, " ");
        assert_eq!(config.feedback.command, ["notify-send", "{state}"]);
    }

    #[test]
    fn cleanup_accepts_a_plain_switch() {
        let off = VoiceConfig::from_yaml("cleanup: false").unwrap();
        assert_eq!(off.cleanup.options(), CleanupOptions::none());

        let on = VoiceConfig::from_yaml("cleanup: true").unwrap();
        assert_eq!(on.cleanup.options(), CleanupOptions::default());
    }

    #[test]
    fn a_typo_is_an_error_not_a_silent_default() {
        let error = VoiceConfig::from_yaml("outptu:\n  kind: stdout\n").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("outptu"), "{message}");
        assert!(message.contains("invalid config"), "{message}");
    }

    #[test]
    fn a_command_backend_without_argv_is_rejected_early() {
        let error = VoiceConfig::from_yaml("transcriber:\n  kind: command\n").unwrap_err();
        assert!(error.to_string().contains("transcriber.argv"), "{error}");

        let error = VoiceConfig::from_yaml("output:\n  kind: command\n").unwrap_err();
        assert!(error.to_string().contains("output.argv"), "{error}");

        let error = VoiceConfig::from_yaml("capture:\n  kind: command\n").unwrap_err();
        assert!(error.to_string().contains("capture.argv"), "{error}");
    }

    #[test]
    fn the_home_alias_is_expanded_in_the_model_directory() {
        let home = std::env::var("HOME").unwrap();
        let config = VoiceConfig::from_yaml("models:\n  directory: ~/m\n").unwrap();
        assert_eq!(config.models.directory, PathBuf::from(home).join("m"));

        assert_eq!(
            expand_home(Path::new("/absolute")),
            PathBuf::from("/absolute")
        );
        assert_eq!(
            expand_home(Path::new("relative")),
            PathBuf::from("relative")
        );
    }

    #[test]
    fn the_default_document_round_trips() {
        let yaml = VoiceConfig::default().to_yaml().unwrap();
        assert_eq!(
            VoiceConfig::from_yaml(&yaml).unwrap(),
            VoiceConfig::default()
        );
    }

    #[test]
    fn a_missing_explicit_config_is_an_error_but_a_missing_default_is_not() {
        let missing = std::env::temp_dir().join("blb-does-not-exist.yml");
        let error = VoiceConfig::load_resolved(Some(&missing)).unwrap_err();
        assert!(error.to_string().contains("no config file"), "{error}");
    }
}
