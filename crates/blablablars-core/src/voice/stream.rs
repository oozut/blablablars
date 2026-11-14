//! Typing while the user is still speaking.
//!
//! Holding a shortcut and waiting for the whole paragraph to come back is the
//! wrong shape for dictation: a three-minute thought means three minutes of
//! silence and then a wall of text. Streaming fixes that by transcribing every
//! few seconds and typing the part that will not change any more, so the words
//! appear roughly as fast as they are spoken.
//!
//! Two things make that possible:
//!
//! * a [`Committer`], which decides which words of a hypothesis are stable
//!   enough to type. Models revise their own output — the last word of a
//!   hypothesis is usually half-heard, and "there" may still become "their" —
//!   so it holds the newest word back and only commits the longest common
//!   prefix of consecutive hypotheses.
//! * a [`LiveEngine`], which turns the growing recording into a hypothesis.
//!   [`WindowEngine`] re-runs an ordinary batch [`Transcriber`] over the last
//!   few seconds, which is the only way to do this with `whisper-cli` and
//!   `parakeet-cli`. [`CommandEngine`] drives one long-lived process that
//!   streams partial results itself, which is what a genuinely streaming
//!   runtime (sherpa-onnx online, Riva, whisper-server) wants to be.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use crate::error::CoreError;

use super::audio::Audio;
use super::models::Model;
use super::transcribe::{substitute, Transcriber};

/// Words of a hypothesis that are safe to type.
///
/// The rule is deliberately conservative: the longest prefix this hypothesis
/// shares with the previous one is stable except for its last word, because the
/// model may still be hearing that word. Nothing is ever retracted, so a model
/// that changes its mind about an earlier word leaves the earlier spelling in
/// the document; that is the price of typing as you speak, and it is why the
/// last word is held back until the next step.
#[derive(Debug, Default)]
pub struct Committer {
    committed: Vec<String>,
    previous: Vec<String>,
}

impl Committer {
    pub fn new() -> Self {
        Self::default()
    }

    /// What has already been typed, joined by single spaces.
    pub fn committed(&self) -> String {
        self.committed.join(" ")
    }

    /// How many words have been typed.
    pub fn count(&self) -> usize {
        self.committed.len()
    }

    /// Feed the newest full hypothesis; returns the words to type now.
    pub fn push(&mut self, hypothesis: &str) -> String {
        let words = words(hypothesis);
        let shared = common_prefix(&self.previous, &words).min(words.len());
        // Words the model has already moved past are firm. The last word it has
        // said so far is not: it may still be half-heard, so it waits for the
        // next step to confirm it.
        let firm = if shared < words.len() {
            shared
        } else {
            shared.saturating_sub(1)
        };
        let ready = firm.max(self.committed.len()).min(words.len());
        self.previous = words.clone();
        self.commit_to(&words, ready)
    }

    /// Commit everything in the final hypothesis that is still owed.
    pub fn finish(&mut self, hypothesis: &str) -> String {
        let words = words(hypothesis);
        let ready = words.len();
        let ready = ready.max(self.committed.len().min(words.len()));
        self.previous = words.clone();
        self.commit_to(&words, ready)
    }

    fn commit_to(&mut self, words: &[String], ready: usize) -> String {
        if ready <= self.committed.len() {
            return String::new();
        }
        let fresh: Vec<String> = words[self.committed.len()..ready].to_vec();
        self.committed.extend(fresh.iter().cloned());
        fresh.join(" ")
    }
}

/// A duration the way it was configured: `2s`, `1.5s`, never `3s` for 2.5s.
pub fn seconds(duration: Duration) -> String {
    let value = duration.as_secs_f64();
    if (value - value.round()).abs() < 1e-9 {
        format!("{value:.0}s")
    } else {
        format!("{value:.1}s")
    }
}

/// Split a transcript into words the way the committer compares them.
pub fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

/// How many leading words two hypotheses agree on.
pub fn common_prefix(previous: &[String], current: &[String]) -> usize {
    previous
        .iter()
        .zip(current.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

/// Something that can turn a growing recording into a hypothesis.
pub trait LiveEngine: Send {
    /// What is doing the work, for logs and `--json`.
    fn describe(&self) -> String;

    /// Take the audio captured since the last call and return the current
    /// hypothesis for **everything said so far** — not just this chunk.
    fn step(&mut self, chunk: &Audio) -> Result<String, CoreError>;

    /// The session is over: return the best final hypothesis, usually from a
    /// higher-quality pass over the whole recording.
    fn finish(&mut self, whole: &Audio) -> Result<String, CoreError>;

    /// The driver has typed this much. A windowed engine uses it to keep its
    /// hypothesis monotonic when it drops audio it has already reported.
    fn note_committed(&mut self, _committed: &str) {}
}

/// A batch backend re-run over the last few seconds of the recording.
///
/// This is how streaming works for the runtimes that have no streaming mode:
/// every step re-transcribes a trailing window, and the committer throws away
/// whatever the model has not yet settled on. The window is what keeps latency
/// flat — a two-minute dictation costs no more per step than a ten-second one.
///
/// The window may not slide past audio nobody has heard yet. If a step comes
/// back with nothing worth typing (the model is still mid-sentence, or the
/// machine is slower than the speaker), starting the next window at "the last
/// few seconds" would leave the words in between unheard forever, and the
/// transcript would jump. So the window keeps growing over the untranscribed
/// part until a step commits, and only then does it slide forward.
pub struct WindowEngine {
    inner: std::sync::Arc<dyn Transcriber>,
    window: Duration,
    /// The audio the next window may still need, oldest first.
    heard: Vec<i16>,
    /// Where `heard[0]` sits in the recording, in samples.
    base: usize,
    /// Everything heard as of the last step, in samples from the recording's
    /// start: the end of the last window the model was shown.
    seen: usize,
    /// Up to here the transcript is in the user's document, so the next window
    /// may start here rather than before it.
    committed_to: usize,
    rate: u32,
    /// What the committer has already typed, prefixed to each hypothesis so the
    /// text stays monotonic while the audio window slides forward.
    prefix: String,
}

impl WindowEngine {
    pub fn new(inner: std::sync::Arc<dyn Transcriber>, window: Duration) -> Self {
        Self {
            inner,
            window,
            heard: Vec::new(),
            base: 0,
            seen: 0,
            committed_to: 0,
            rate: 16_000,
            prefix: String::new(),
        }
    }

    /// Where the next window starts, in samples from the recording's start.
    fn window_start(&self, end: usize) -> usize {
        let want = (self.window.as_secs_f64() * f64::from(self.rate)) as usize;
        // The natural start keeps the window short; it may not go past audio
        // that has not been transcribed yet, or words would be skipped.
        end.saturating_sub(want)
            .min(self.committed_to)
            .max(self.base)
    }
}

impl LiveEngine for WindowEngine {
    fn describe(&self) -> String {
        format!(
            "{} over the last {}",
            self.inner.describe(),
            seconds(self.window)
        )
    }

    fn step(&mut self, chunk: &Audio) -> Result<String, CoreError> {
        if chunk.is_empty() {
            return Ok(self.prefix.clone());
        }
        self.rate = chunk.rate();
        self.heard.extend_from_slice(chunk.samples());
        let end = self.base + self.heard.len();
        let start = self.window_start(end);
        // Drop what no window will ever need again, so a long dictation does
        // not hold three minutes of audio to look at the last four seconds.
        let drop = start - self.base;
        if drop > 0 {
            self.heard.drain(..drop);
            self.base = start;
        }
        let window = Audio::new(self.rate, self.heard.clone());
        self.seen = end;
        let heard = self.inner.transcribe(&window)?;
        Ok(merge(&self.prefix, &heard))
    }

    fn finish(&mut self, whole: &Audio) -> Result<String, CoreError> {
        // One unhurried pass over everything, at full quality.
        let text = self.inner.transcribe(whole)?;
        // The committer only accepts a superset of what it has typed, so the
        // final hypothesis has to carry the typed prefix. When the full pass
        // wrote those words differently, the typed spelling stays and only the
        // part the committer has not seen is appended: retracting text that is
        // already in the user's document is not possible anyway.
        Ok(merge(&self.prefix, &text))
    }

    fn note_committed(&mut self, committed: &str) {
        // Only words the window may no longer contain need carrying; keeping the
        // whole prefix is simplest and costs one `format!` per step.
        self.prefix = committed.to_string();
        // The window the model just read has been accepted, so the next one may
        // start where it ended instead of re-hearing audio already typed.
        self.committed_to = self.seen;
    }
}

/// One long-lived process that streams partial results itself.
///
/// The contract is the dictionary one: write raw 16 kHz mono s16le PCM to
/// stdin, and print one JSON object per line on stdout, each one the current
/// transcript of everything heard so far. `{"text": "..."}` — the same shape
/// extensions already use. Diagnostics on stdout are a protocol error; put them
/// on stderr, where they end up in the error message.
pub struct CommandEngine {
    argv: Vec<String>,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
    timeout: Duration,
    latest: String,
}

impl CommandEngine {
    /// Start the streaming program named by `command`.
    pub fn start(
        argv: &[String],
        model: &Model,
        language: &str,
        timeout: Duration,
    ) -> Result<Self, CoreError> {
        let argv = substitute(argv, std::path::Path::new(""), model, language);
        let program = argv
            .first()
            .cloned()
            .ok_or_else(|| CoreError::Voice("`stream.command` is empty".to_string()))?;
        let program = super::platform::resolve_program(&program);

        let mut command = Command::new(&program);
        command
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("BLABLABLARS_STREAM_RATE", "16000");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            // Its own group, so a wedged engine dies with the session.
            command.process_group(0);
        }

        let mut child = command.spawn().map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => CoreError::Voice(format!(
                "`stream.command` names `{program}`, which is not on PATH"
            )),
            _ => CoreError::Voice(format!("cannot run `{program}`: {error}")),
        })?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout is piped");
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let diagnostics = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let collected = std::sync::Arc::clone(&diagnostics);
        std::thread::spawn(move || {
            use std::io::Read as _;
            let mut buffer = Vec::new();
            let _ = stderr.read_to_end(&mut buffer);
            let text = String::from_utf8_lossy(&buffer)
                .trim()
                .lines()
                .take(5)
                .collect::<Vec<_>>()
                .join("\n");
            *collected.lock().unwrap_or_else(|error| error.into_inner()) = text;
        });

        let (sender, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            argv,
            child,
            stdin,
            lines,
            stderr: diagnostics,
            timeout,
            latest: String::new(),
        })
    }

    /// Read one hypothesis, waiting no longer than the timeout.
    fn next_hypothesis(&mut self) -> Result<Option<String>, CoreError> {
        let line = match self.lines.recv_timeout(self.timeout) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        };
        let program = self.argv[0].clone();
        let mut text = hypothesis_from_line(&line)
            .map_err(|problem| CoreError::Voice(format!("{program}: {problem}")))?;
        // An engine that is ahead of us may have printed several lines while we
        // waited; the newest one is the best hypothesis, so keep draining.
        while let Ok(newer) = self.lines.try_recv() {
            text = hypothesis_from_line(&newer)
                .map_err(|problem| CoreError::Voice(format!("{program}: {problem}")))?;
        }
        self.latest = text.clone();
        Ok(Some(text))
    }

    fn diagnostics(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

impl LiveEngine for CommandEngine {
    fn describe(&self) -> String {
        format!("stream ({})", self.argv.join(" "))
    }

    fn step(&mut self, chunk: &Audio) -> Result<String, CoreError> {
        if chunk.is_empty() {
            return Ok(self.latest.clone());
        }
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(CoreError::Voice(
                "the streaming engine closed its input".to_string(),
            ));
        };
        // A program that exits early breaks the pipe; its last words on stdout
        // are still the better answer, so read before reporting the write.
        let written = stdin
            .write_all(&chunk.to_s16le())
            .and_then(|()| stdin.flush());

        match self.next_hypothesis()? {
            Some(text) => Ok(text),
            None => {
                if let Err(error) = written {
                    return Err(CoreError::Voice(format!(
                        "cannot feed the streaming engine: {error}"
                    )));
                }
                let detail = self.diagnostics();
                Err(CoreError::Voice(format!(
                    "the streaming engine printed nothing within {:.0}s{}",
                    self.timeout.as_secs_f64(),
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {detail}")
                    }
                )))
            }
        }
    }

    fn finish(&mut self, _whole: &Audio) -> Result<String, CoreError> {
        // Closing stdin is the "no more audio" signal; whatever the engine
        // prints before exiting is the final transcript.
        drop(self.stdin.take());
        let deadline = std::time::Instant::now() + self.timeout;
        let mut latest = self.latest.clone();
        // An engine that keeps printing is not a reason to hang the session, so
        // the wait is a budget, not a per-line timeout.
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    if let Ok(text) = hypothesis_from_line(&line) {
                        latest = text;
                    }
                }
                Err(_) => break,
            }
        }
        // The budget was for the lines, and it has run out. An engine still
        // running has nothing left to say — it is ignoring the end of its input
        // — so it is stopped rather than waited for, or the session would hang
        // for as long as the program likes.
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        Ok(latest)
    }
}

impl Drop for CommandEngine {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Parse one line of the streaming protocol: `{"text": "..."}`.
fn hypothesis_from_line(line: &str) -> Result<String, String> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(String::new());
    }
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|error| format!("not a JSON line ({error}): {line}"))?;
    match value.get("text").and_then(serde_json::Value::as_str) {
        Some(text) => Ok(text.to_string()),
        None => Err(format!("a line without a string `text` field: {line}")),
    }
}

/// Put a carried prefix back in front of a window's transcript.
///
/// A window that still contains the audio of the typed words already reports
/// them, and prefixing would say them twice; a window that has slid past them
/// does not, and the prefix is what keeps the hypothesis monotonic.
fn merge(prefix: &str, text: &str) -> String {
    let carried = words(prefix);
    let fresh = words(text);
    let shared = common_prefix(&carried, &fresh);
    if shared == fresh.len() {
        // Nothing new: the hypothesis is all words that are already typed.
        return prefix.trim_end().to_string();
    }
    join(&carried.join(" "), &fresh[shared..].join(" "))
}

/// Join a carried prefix and fresh text without doubling a space.
fn join(prefix: &str, text: &str) -> String {
    match (prefix.trim_end().is_empty(), text.trim().is_empty()) {
        (true, _) => text.trim().to_string(),
        (false, true) => prefix.trim_end().to_string(),
        (false, false) => format!("{} {}", prefix.trim_end(), text.trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn speaker(rate: u32, seconds: f64) -> Audio {
        let samples = (rate as f64 * seconds) as usize;
        Audio::new(rate, vec![0i16; samples])
    }

    /// A second handle to a scripted backend, since a `Box<dyn Transcriber>`
    /// hides the concrete type the test wants to inspect.
    struct Sharing(std::sync::Arc<Scripted>);

    impl Transcriber for Sharing {
        fn describe(&self) -> String {
            self.0.describe()
        }

        fn transcribe(&self, audio: &Audio) -> Result<String, CoreError> {
            self.0.transcribe(audio)
        }
    }

    /// A batch backend that answers from a script instead of a model.
    struct Scripted {
        answers: std::sync::Mutex<Vec<String>>,
        seen: std::sync::Mutex<Vec<f64>>,
    }

    impl Scripted {
        fn new(answers: &[&str]) -> Self {
            Self {
                answers: std::sync::Mutex::new(
                    answers.iter().rev().map(|a| a.to_string()).collect(),
                ),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl Transcriber for Scripted {
        fn describe(&self) -> String {
            "scripted".to_string()
        }

        fn transcribe(&self, audio: &Audio) -> Result<String, CoreError> {
            self.seen
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(audio.seconds());
            Ok(self
                .answers
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pop()
                .unwrap_or_default())
        }
    }

    #[test]
    fn a_first_hypothesis_keeps_its_last_word_back() {
        let mut committer = Committer::new();
        // Nothing to compare against yet, so only the first word can be typed.
        assert_eq!(committer.push("hello"), "");
        assert_eq!(committer.push("hello there"), "hello");
        assert_eq!(committer.push("hello there world"), "there");
        // The newest word stays held back until the session ends.
        assert_eq!(committer.push("hello there world"), "");
        assert_eq!(committer.finish("hello there world"), "world");
    }

    #[test]
    fn a_word_that_changes_is_not_typed_twice() {
        let mut committer = Committer::new();
        assert_eq!(committer.push("I went to the"), "");
        assert_eq!(committer.push("I went to their house"), "I went to");
        // The model changes its mind about a word it had not typed yet.
        assert_eq!(committer.push("I went to the house"), "");
        assert_eq!(committer.finish("I went to the house"), "the house");
        assert_eq!(committer.committed(), "I went to the house");
    }

    #[test]
    fn finishing_types_whatever_is_still_owed() {
        let mut committer = Committer::new();
        assert_eq!(committer.push("one two three"), "");
        let rest = committer.finish("one two three four");
        assert_eq!(rest, "one two three four");
        assert_eq!(committer.committed(), "one two three four");
        // Nothing is owed after that.
        assert!(committer.finish("one two three four").is_empty());
    }

    #[test]
    fn a_shrinking_hypothesis_does_not_retract_anything() {
        let mut committer = Committer::new();
        assert_eq!(committer.push("alpha beta gamma"), "");
        assert_eq!(committer.push("alpha beta gamma delta"), "alpha beta gamma");
        // The model loses the tail entirely; nothing is typed and nothing is
        // taken back.
        assert_eq!(committer.push("alpha"), "");
        assert_eq!(committer.push("alpha"), "");
        assert_eq!(committer.committed(), "alpha beta gamma");
    }

    #[test]
    fn whitespace_is_not_a_change() {
        let mut committer = Committer::new();
        assert_eq!(committer.push("  one   two\tthree "), "");
        assert_eq!(committer.push("one two three four"), "one two three");
        assert_eq!(words("  a  b "), ["a", "b"]);
    }

    #[test]
    fn a_windowed_engine_only_feeds_the_tail_to_the_model() {
        let scripted = std::sync::Arc::new(Scripted::new(&["a", "b", "c", "d"]));
        let mut engine = WindowEngine::new(
            std::sync::Arc::new(Sharing(std::sync::Arc::clone(&scripted))),
            Duration::from_secs(3),
        );
        assert!(engine.describe().contains("last 3s"));

        for _ in 0..3 {
            engine.step(&speaker(16_000, 2.0)).unwrap();
            // Every word was typed, so the window may start where it ended.
            engine.note_committed("typed");
        }
        let seen = scripted.seen.lock().unwrap().clone();
        // 2s, 4s clipped to the 3s window, then still 3s: latency stays flat.
        assert_eq!(seen, vec![2.0, 3.0, 3.0]);
    }

    #[test]
    fn a_window_keeps_audio_nobody_has_typed_yet() {
        let scripted = std::sync::Arc::new(Scripted::new(&["a", "b", "c", "d"]));
        let mut engine = WindowEngine::new(
            std::sync::Arc::new(Sharing(std::sync::Arc::clone(&scripted))),
            Duration::from_secs(3),
        );

        for _ in 0..3 {
            // The model heard a word or two but nothing was stable enough to
            // type, so the window grows instead of sliding past the words.
            engine.step(&speaker(16_000, 2.0)).unwrap();
        }
        let seen = scripted.seen.lock().unwrap().clone();
        assert_eq!(seen, vec![2.0, 4.0, 6.0]);
    }

    #[test]
    fn a_windowed_engine_carries_what_was_typed_when_the_window_slides() {
        let mut engine = WindowEngine::new(
            std::sync::Arc::new(Scripted::new(&[
                "first second",
                "first second third",
                "fourth",
            ])),
            Duration::from_secs(1),
        );
        let mut committer = Committer::new();
        let one = engine.step(&speaker(16_000, 2.0)).unwrap();
        assert_eq!(one, "first second");
        assert_eq!(committer.push(&one), "");

        // The model has now said three words, so the first two are typed and
        // the window is about to slide past their audio.
        let two = engine.step(&speaker(16_000, 2.0)).unwrap();
        assert_eq!(two, "first second third");
        assert_eq!(committer.push(&two), "first second");
        engine.note_committed(&committer.committed());

        // Only the last second reaches the model this time, so the typed words
        // must be carried or the hypothesis would restart from nothing.
        let three = engine.step(&speaker(16_000, 2.0)).unwrap();
        assert_eq!(
            three, "first second fourth",
            "the window slid, so the typed words are carried"
        );
        assert_eq!(committer.push(&three), "");
    }

    #[test]
    fn a_final_pass_never_retracts_what_was_typed() {
        let mut engine = WindowEngine::new(
            std::sync::Arc::new(Scripted::new(&[
                "hello there",
                "hello there friend",
                "hello there friend today",
                // The final pass over the whole recording.
                "hello there friend today",
            ])),
            Duration::from_secs(30),
        );
        let mut committer = Committer::new();

        let one = engine.step(&speaker(16_000, 1.0)).unwrap();
        assert_eq!(one, "hello there");
        assert_eq!(committer.push(&one), "");
        engine.note_committed(&committer.committed());

        let two = engine.step(&speaker(16_000, 1.0)).unwrap();
        assert_eq!(two, "hello there friend");
        assert_eq!(committer.push(&two), "hello there");
        engine.note_committed(&committer.committed());

        let three = engine.step(&speaker(16_000, 1.0)).unwrap();
        assert_eq!(three, "hello there friend today");
        assert_eq!(committer.push(&three), "friend");

        // The full pass is a better transcript: the held-back word and the tail
        // arrive, and nothing already typed is repeated or taken back.
        let final_pass = engine.finish(&speaker(16_000, 3.0)).unwrap();
        assert_eq!(final_pass, "hello there friend today");
        assert_eq!(committer.finish(&final_pass), "today");
    }

    #[test]
    fn a_window_that_still_holds_the_typed_words_does_not_repeat_them() {
        assert_eq!(merge("", "hello there"), "hello there");
        assert_eq!(merge("hello", "hello there"), "hello there");
        assert_eq!(merge("hello there", "friend"), "hello there friend");
        assert_eq!(
            merge("hello there", "hello there friend"),
            "hello there friend"
        );
    }

    #[test]
    fn a_streaming_program_is_driven_over_stdin_and_json_lines() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            // One hypothesis per chunk of audio, then one more when the audio
            // stops. The loop has to end on EOF, or the test never returns.
            "while [ \"$(dd bs=32000 count=1 2>/dev/null | wc -c)\" -gt 0 ]; do \
             echo '{\"text\": \"hello\"}'; done; echo '{\"text\": \"hello there\"}'"
                .to_string(),
        ];
        let model = Model::unresolved("none");
        let mut engine = CommandEngine::start(&argv, &model, "en", Duration::from_secs(5)).unwrap();
        assert!(engine.describe().starts_with("stream ("));
        let first = engine.step(&speaker(16_000, 1.0)).unwrap();
        assert_eq!(first, "hello");
        let second = engine.step(&speaker(16_000, 1.0)).unwrap();
        assert_eq!(second, "hello", "the newest line is the hypothesis");
        assert_eq!(engine.finish(&speaker(16_000, 2.0)).unwrap(), "hello there");
    }

    #[test]
    fn a_streaming_program_that_never_answers_says_so() {
        let argv = vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()];
        let model = Model::unresolved("none");
        let mut engine =
            CommandEngine::start(&argv, &model, "en", Duration::from_millis(300)).unwrap();
        let error = engine.step(&speaker(16_000, 1.0)).unwrap_err().to_string();
        assert!(error.contains("printed nothing"), "{error}");
    }

    #[test]
    fn finishing_a_wedged_engine_does_not_wait_for_it() {
        // `sleep` never reads its input, so closing stdin says nothing to it.
        // The budget is the wait, not a suggestion: the session must not hang
        // for as long as the program likes.
        let argv = vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()];
        let model = Model::unresolved("none");
        let mut engine =
            CommandEngine::start(&argv, &model, "en", Duration::from_millis(300)).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(engine.finish(&speaker(16_000, 1.0)).unwrap(), "");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "finish waited {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_streaming_program_that_writes_noise_is_a_protocol_error() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo 'loading model'".to_string(),
        ];
        let model = Model::unresolved("none");
        let mut engine = CommandEngine::start(&argv, &model, "en", Duration::from_secs(5)).unwrap();
        let error = engine.step(&speaker(16_000, 1.0)).unwrap_err().to_string();
        assert!(error.contains("not a JSON line"), "{error}");
    }

    #[test]
    fn the_protocol_line_must_carry_a_text_field() {
        assert_eq!(
            hypothesis_from_line("{\"text\": \"hi\"}").unwrap(),
            "hi".to_string()
        );
        assert_eq!(hypothesis_from_line("   ").unwrap(), "");
        assert!(hypothesis_from_line("{\"words\": 1}")
            .unwrap_err()
            .contains("text"));
        assert!(hypothesis_from_line("hi")
            .unwrap_err()
            .contains("not a JSON"));
    }

    #[test]
    fn a_prefix_and_fresh_text_are_joined_once() {
        assert_eq!(join("", "hello"), "hello");
        assert_eq!(join("hello", ""), "hello");
        assert_eq!(join("hello ", "world"), "hello world");
        assert_eq!(join("hello", " world"), "hello world");
        assert_eq!(join("", ""), "");
    }
}
