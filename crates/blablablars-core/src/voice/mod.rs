//! Dictation: microphone to focused text field.
//!
//! The stack is four small, replaceable pieces —
//! [`capture`] (a child process writing PCM), [`transcribe`] (a model),
//! [`cleanup`] (rules that make the transcript readable) and [`inject`]
//! (keystrokes) — joined by [`Dictation`], which owns the order and exposes the
//! two extension points on either side of the cleanup step:
//!
//! ```text
//! audio -> transcribe -> transcription.raw -> cleanup -> transcription.text -> type
//! ```
//!
//! `transcription.raw` is where a dictionary or a language fix belongs (it sees
//! what the model actually said); `transcription.text` is where a rewriter or
//! an LLM belongs (it sees the text that is about to be typed). Both take and
//! return `{"text": "..."}`, so a Rust extension and a Python plugin are the
//! same thing to the pipeline.

pub mod audio;
pub mod capture;
pub mod cleanup;
pub mod config;
pub mod inject;
pub mod models;
pub mod platform;
pub mod stream;
pub mod transcribe;

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::error::CoreError;
use crate::extension::{HOOK_TRANSCRIPT_RAW, HOOK_TRANSCRIPT_TEXT};
use crate::pipeline::Pipeline;

pub use audio::Audio;
pub use capture::Recording;
pub use config::{VoiceConfig, ENV_CONFIG};
pub use inject::{Injector, KeyInjector, StdoutInjector};
pub use models::Model;
pub use stream::{CommandEngine, Committer, LiveEngine, WindowEngine};
pub use transcribe::Transcriber;

/// One dictation, from audio to the text that was typed.
#[derive(Debug, Clone)]
pub struct DictationRun {
    /// Exactly what the model produced, before cleanup.
    pub raw: String,
    /// What was handed to the injector, after hooks and cleanup.
    pub text: String,
    /// The model that produced it.
    pub model: String,
    /// The backend that ran it.
    pub transcriber: String,
    pub audio_seconds: f64,
    pub elapsed: Duration,
    /// `extension: hook` entries in the order they ran.
    pub trace: Vec<String>,
}

impl DictationRun {
    /// The JSON shape used by `--json`.
    pub fn to_json(&self) -> Value {
        json!({
            "text": self.text,
            "raw": self.raw,
            "model": self.model,
            "transcriber": self.transcriber,
            "audio_seconds": self.audio_seconds,
            "elapsed_ms": self.elapsed.as_millis() as u64,
            "trace": self.trace,
        })
    }
}

/// Recording, transcription, cleanup and typing, configured once.
pub struct Dictation {
    config: VoiceConfig,
    model: Model,
    /// Shared so a streaming session can re-run the same backend over a window.
    transcriber: Arc<dyn Transcriber>,
    pipeline: Pipeline,
}

impl Dictation {
    /// Build from a config, with no extensions.
    pub fn new(config: VoiceConfig) -> Result<Self, CoreError> {
        Self::with_pipeline(config, None, Pipeline::new())
    }

    /// Build with extensions attached and optionally a different model.
    pub fn with_pipeline(
        config: VoiceConfig,
        model: Option<&str>,
        pipeline: Pipeline,
    ) -> Result<Self, CoreError> {
        let wanted = model.unwrap_or(&config.models.default);
        // Only a backend that runs a local model needs a real file on disk. A
        // command backend may not use a local model at all (a streaming engine,
        // a remote API), so an unresolved name is passed through as `{model}`.
        let resolved = if config.transcriber.kind.needs_model_file() {
            models::resolve(&config.models.directory, wanted)?
        } else {
            models::resolve(&config.models.directory, wanted)
                .unwrap_or_else(|_| Model::unresolved(wanted))
        };
        let transcriber: Arc<dyn Transcriber> = Arc::from(transcribe::from_config(
            &config.transcriber,
            &config.language,
            &resolved,
        )?);
        Ok(Self {
            config,
            model: resolved,
            transcriber,
            pipeline,
        })
    }

    pub fn config(&self) -> &VoiceConfig {
        &self.config
    }

    /// The model name in use.
    pub fn model(&self) -> &str {
        &self.model.name
    }

    /// What is doing the transcription, for logs.
    pub fn transcriber_description(&self) -> String {
        self.transcriber.describe()
    }

    /// Transcribe and clean `audio`.
    pub fn transcribe(&self, audio: &Audio) -> Result<DictationRun, CoreError> {
        if audio.is_empty() {
            return Err(CoreError::Voice("nothing was recorded".to_string()));
        }
        if audio.seconds() < 0.1 {
            return Err(CoreError::Voice(format!(
                "the recording is only {:.0} ms long; hold the shortcut a little longer",
                audio.seconds() * 1000.0
            )));
        }

        let started = Instant::now();
        let raw = self.transcriber.transcribe(audio)?;
        let (raw, text, trace) = self.polish(raw, audio.seconds())?;

        Ok(DictationRun {
            raw,
            text,
            model: self.model.name.clone(),
            transcriber: self.transcriber.describe(),
            audio_seconds: audio.seconds(),
            elapsed: started.elapsed(),
            trace,
        })
    }

    /// Clean up `raw` and run the two transcript hooks on it.
    ///
    /// Shared by the batch path and the streaming one, so a partial line goes
    /// through exactly the same dictionary, fillers and rewriters as a whole
    /// utterance. Returns the text after the raw hook, the final text, and the
    /// extensions that ran.
    pub fn polish(
        &self,
        raw: String,
        audio_seconds: f64,
    ) -> Result<(String, String, Vec<String>), CoreError> {
        let mut trace = Vec::new();
        let raw = self.dispatch(
            HOOK_TRANSCRIPT_RAW,
            json!({
                "text": raw,
                "audio_seconds": audio_seconds,
                "model": self.model.name,
                "transcriber": self.transcriber.describe(),
            }),
            &mut trace,
        )?;

        let text = cleanup::clean(
            &cleanup::expand_spoken_phrases(&raw),
            &self.config.cleanup.options(),
        );
        let text = self.dispatch(HOOK_TRANSCRIPT_TEXT, json!({ "text": text }), &mut trace)?;
        Ok((raw, text, trace))
    }

    /// The streaming engine this config selects.
    ///
    /// A `stream.command` that streams partial results itself wins; otherwise
    /// the ordinary backend is re-run over a trailing window, which is the only
    /// way to stream with a batch model.
    pub fn live_engine(&self) -> Result<Box<dyn LiveEngine>, CoreError> {
        if !self.config.stream.command.is_empty() {
            let timeout = Duration::from_secs(self.config.stream.command_timeout_seconds);
            return Ok(Box::new(stream::CommandEngine::start(
                &self.config.stream.command,
                &self.model,
                &self.config.language,
                timeout,
            )?));
        }
        Ok(Box::new(stream::WindowEngine::new(
            Arc::clone(&self.transcriber),
            Duration::from_millis(self.config.stream.window_ms),
        )))
    }

    /// True when this config wants the words as they are spoken.
    pub fn streaming(&self) -> bool {
        self.config.stream.enabled
    }

    /// Read a WAV file and run it through the same path as a recording.
    pub fn transcribe_file(&self, path: &Path) -> Result<DictationRun, CoreError> {
        let audio = audio::read_wav(path)?;
        self.transcribe(&audio)
    }

    /// Type `text` where the user is typing, honouring `output.trailing`.
    pub fn deliver(&self, text: &str) -> Result<String, CoreError> {
        let injector = inject::from_config(&self.config.output)?;
        let mut payload = text.to_string();
        payload.push_str(&self.config.output.trailing);
        injector.inject(&payload)?;
        Ok(injector.describe())
    }

    /// Type one streamed chunk exactly as given.
    ///
    /// `output.trailing` is not added here: a streaming session types a space
    /// after every word, so a second separator would double it.
    pub fn deliver_chunk(&self, text: &str) -> Result<String, CoreError> {
        let injector = inject::from_config(&self.config.output)?;
        injector.inject_chunk(text)?;
        Ok(injector.describe())
    }

    /// Run `feedback.command` with `{state}`, best effort.
    ///
    /// Feedback must never be the reason a dictation fails, so a missing or
    /// failing program is ignored.
    pub fn notify(&self, state: &str) {
        let mut argv: Vec<String> = self
            .config
            .feedback
            .command
            .iter()
            .map(|argument| argument.replace("{state}", state))
            .collect();
        if argv.is_empty() {
            return;
        }
        let program = platform::resolve_program(&argv.remove(0));
        let Ok(mut child) = Command::new(program)
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return;
        };
        // Reaped on a thread of its own. Feedback outlives nothing, but a child
        // nobody waits for stays a zombie until this process exits.
        let _ = std::thread::Builder::new()
            .name("blablablars-feedback".to_string())
            .spawn(move || {
                let _ = child.wait();
            });
    }

    /// Dispatch a text hook and return the payload's `text` field.
    fn dispatch(
        &self,
        hook: &str,
        payload: Value,
        trace: &mut Vec<String>,
    ) -> Result<String, CoreError> {
        let payload = if self.pipeline.registry().is_empty() {
            payload
        } else {
            let dispatch = self.pipeline.registry().dispatch(hook, payload)?;
            trace.extend(dispatch.trace);
            dispatch.payload
        };

        payload
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                CoreError::Voice(format!(
                    "an extension on `{hook}` returned no string `text` field"
                ))
            })
    }
}

impl std::fmt::Debug for Dictation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dictation")
            .field("model", &self.model)
            .field("transcriber", &self.transcriber.describe())
            .field("language", &self.config.language)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ExtensionError;
    use crate::extension::Extension;
    use std::sync::{Arc, Mutex};

    fn wav_bytes(seconds: f64) -> Audio {
        let frames = (seconds * f64::from(audio::WHISPER_RATE)) as usize;
        Audio::new(audio::WHISPER_RATE, vec![1000; frames])
    }

    /// A config that produces text from a shell command, so no model is needed.
    fn shell_config(script: &str) -> VoiceConfig {
        VoiceConfig::from_yaml(&format!(
            r#"
            language: en
            transcriber:
              kind: command
              argv: ["sh", "-c", {script:?}]
            "#
        ))
        .unwrap()
    }

    struct Recorder {
        name: &'static str,
        hooks: Vec<String>,
        seen: Arc<Mutex<Vec<(String, Value)>>>,
        rewrite: Option<&'static str>,
    }

    impl Extension for Recorder {
        fn name(&self) -> &str {
            self.name
        }

        fn hooks(&self) -> &[String] {
            &self.hooks
        }

        fn call(&self, hook: &str, payload: &Value) -> Result<Value, ExtensionError> {
            self.seen
                .lock()
                .unwrap()
                .push((hook.to_string(), payload.clone()));
            let mut next = payload.clone();
            if let (Some(from), Some(text)) = (self.rewrite, next["text"].as_str()) {
                next["text"] = json!(text.replace(from, "REWRITTEN"));
            }
            Ok(next)
        }
    }

    fn recorder(
        name: &'static str,
        hooks: &[&str],
        seen: Arc<Mutex<Vec<(String, Value)>>>,
        rewrite: Option<&'static str>,
    ) -> Arc<Recorder> {
        Arc::new(Recorder {
            name,
            hooks: hooks.iter().map(|hook| hook.to_string()).collect(),
            seen,
            rewrite,
        })
    }

    #[test]
    fn a_transcript_goes_through_cleanup() {
        let dictation = Dictation::new(shell_config("printf 'um, hello   there'")).unwrap();
        let run = dictation.transcribe(&wav_bytes(1.0)).unwrap();

        assert_eq!(run.raw, "um, hello   there");
        assert_eq!(run.text, "Hello there.");
        assert_eq!(run.audio_seconds, 1.0);
        assert_eq!(run.model, "whisper-large-v3-turbo");
        assert!(run.trace.is_empty());
    }

    #[test]
    fn the_raw_hook_sees_the_model_text_before_cleanup() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut pipeline = Pipeline::new();
        pipeline
            .register(recorder(
                "dictionary",
                &[HOOK_TRANSCRIPT_RAW],
                seen.clone(),
                Some("kubernetes"),
            ))
            .unwrap();

        let config = shell_config("printf 'i use kubernetes'");
        let dictation = Dictation::with_pipeline(config, None, pipeline).unwrap();
        let run = dictation.transcribe(&wav_bytes(0.5)).unwrap();

        assert_eq!(run.text, "I use REWRITTEN.");
        assert_eq!(run.trace, vec!["dictionary: transcription.raw"]);

        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].1["text"], "i use kubernetes");
        assert_eq!(seen[0].1["model"], "whisper-large-v3-turbo");
        assert_eq!(seen[0].1["audio_seconds"], 0.5);
    }

    #[test]
    fn the_text_hook_sees_the_cleaned_text_and_can_rewrite_it() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut pipeline = Pipeline::new();
        pipeline
            .register(recorder(
                "polish",
                &[HOOK_TRANSCRIPT_TEXT],
                seen.clone(),
                Some("Hello there."),
            ))
            .unwrap();

        let dictation =
            Dictation::with_pipeline(shell_config("printf 'um hello there'"), None, pipeline)
                .unwrap();
        let run = dictation.transcribe(&wav_bytes(0.5)).unwrap();

        assert_eq!(run.text, "REWRITTEN");
        assert_eq!(seen.lock().unwrap()[0].1["text"], "Hello there.");
    }

    #[test]
    fn both_hooks_run_in_order_and_the_trace_says_so() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut pipeline = Pipeline::new();
        pipeline
            .register(recorder(
                "first",
                &[HOOK_TRANSCRIPT_RAW],
                seen.clone(),
                None,
            ))
            .unwrap();
        pipeline
            .register(recorder(
                "second",
                &[HOOK_TRANSCRIPT_TEXT],
                seen.clone(),
                None,
            ))
            .unwrap();

        let dictation =
            Dictation::with_pipeline(shell_config("printf 'hello'"), None, pipeline).unwrap();
        let run = dictation.transcribe(&wav_bytes(0.5)).unwrap();

        assert_eq!(
            run.trace,
            vec!["first: transcription.raw", "second: transcription.text"]
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].1["text"], "hello");
        assert_eq!(seen[1].1["text"], "Hello.");
    }

    #[test]
    fn an_extension_that_drops_the_text_field_is_an_error() {
        struct Dropper(Vec<String>);
        impl Extension for Dropper {
            fn name(&self) -> &str {
                "dropper"
            }
            fn hooks(&self) -> &[String] {
                &self.0
            }
            fn call(&self, _hook: &str, _payload: &Value) -> Result<Value, ExtensionError> {
                Ok(json!({ "nope": true }))
            }
        }

        let mut pipeline = Pipeline::new();
        pipeline
            .register(Arc::new(Dropper(vec![HOOK_TRANSCRIPT_TEXT.to_string()])))
            .unwrap();

        let dictation =
            Dictation::with_pipeline(shell_config("printf 'hi'"), None, pipeline).unwrap();
        let error = dictation.transcribe(&wav_bytes(0.5)).unwrap_err();
        assert!(error.to_string().contains("no string `text`"), "{error}");
    }

    #[test]
    fn empty_audio_is_rejected_before_a_model_is_asked() {
        let dictation = Dictation::new(shell_config("echo should-not-run")).unwrap();
        let error = dictation
            .transcribe(&Audio::new(audio::WHISPER_RATE, Vec::new()))
            .unwrap_err();
        assert!(
            error.to_string().contains("nothing was recorded"),
            "{error}"
        );
    }

    #[test]
    fn a_too_short_recording_says_what_to_do() {
        let dictation = Dictation::new(shell_config("echo hello")).unwrap();
        let error = dictation
            .transcribe(&Audio::new(audio::WHISPER_RATE, vec![1; 320]))
            .unwrap_err();
        assert!(error.to_string().contains("hold the shortcut"), "{error}");
    }

    #[test]
    fn everything_can_be_switched_off() {
        let config = VoiceConfig::from_yaml(
            r#"
            transcriber:
              kind: command
              argv: ["printf", "um, hello   there"]
            cleanup: false
            "#,
        )
        .unwrap();
        let dictation = Dictation::new(config).unwrap();
        let run = dictation.transcribe(&wav_bytes(0.5)).unwrap();
        assert_eq!(run.text, "um, hello there");
    }

    #[test]
    fn the_json_shape_is_stable() {
        let dictation = Dictation::new(shell_config("printf 'hello'")).unwrap();
        let run = dictation.transcribe(&wav_bytes(1.0)).unwrap();
        let json = run.to_json();

        assert_eq!(json["text"], "Hello.");
        assert_eq!(json["raw"], "hello");
        assert_eq!(json["model"], "whisper-large-v3-turbo");
        assert!(json["transcriber"].as_str().unwrap().starts_with("command"));
        assert_eq!(json["audio_seconds"], 1.0);
        assert!(json["elapsed_ms"].is_u64());
        assert_eq!(json["trace"], json!([]));
    }

    #[test]
    fn a_missing_model_is_reported_before_recording_starts() {
        let config = VoiceConfig::from_yaml(
            r#"
            models:
              directory: /var/lib/aoe/blablablars-worktrees/gurjaras/does-not-exist
              default: small
            "#,
        )
        .unwrap();
        let error = Dictation::new(config).unwrap_err().to_string();
        assert!(error.contains("no models"), "{error}");
    }
}
