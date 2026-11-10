//! Turning a raw model transcript into text that belongs in the focused field.
//!
//! Whisper is trained on subtitles and on podcasts, so its output carries the
//! habits of both: `[BLANK_AUDIO]` tags, `(music)` annotations, filler words,
//! stutters, a space before every comma, and no final full stop. Typing that
//! verbatim is what makes dictation feel unfinished, so the raw transcript is
//! normalised before it is typed.
//!
//! Every rule is a switch, off is always available, and the whole step can be
//! skipped with `cleanup: false` in the config or `--no-cleanup` on the command
//! line. Extensions still see the raw transcript on `transcription.raw`, so a
//! rule that is wrong for a user can be replaced instead of worked around.

use std::sync::OnceLock;

use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};

/// Which normalisation rules run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanupOptions {
    /// Drop `[BLANK_AUDIO]`, `(music)`, `♪` and similar non-speech markers.
    pub artifacts: bool,
    /// Drop standalone `um`, `uh`, `erm`, `hmm` and friends.
    pub fillers: bool,
    /// Drop a word repeated immediately (`the the`), as a stutter or a
    /// duplicate prediction.
    pub repeated_words: bool,
    /// Fix spacing, capitalisation and the missing final full stop.
    pub punctuation: bool,
    /// Turn spoken commands into characters: `comma`, `full stop`, `new line`.
    /// Off by default: it is easy to say "I like the period piece".
    pub spoken_punctuation: bool,
    /// Appended when the text does not end in punctuation.
    pub terminal: String,
    /// Keep the character case the model produced.
    pub preserve_case: bool,
}

impl Default for CleanupOptions {
    fn default() -> Self {
        Self {
            artifacts: true,
            fillers: true,
            repeated_words: true,
            punctuation: true,
            spoken_punctuation: false,
            terminal: ".".to_string(),
            preserve_case: false,
        }
    }
}

impl CleanupOptions {
    /// Everything off: used by `--no-cleanup` and by `cleanup: false`.
    pub fn none() -> Self {
        Self {
            artifacts: false,
            fillers: false,
            repeated_words: false,
            punctuation: false,
            spoken_punctuation: false,
            terminal: String::new(),
            preserve_case: true,
        }
    }

    /// Trim the output without adding anything.
    pub fn without_terminal(mut self) -> Self {
        self.terminal.clear();
        self
    }
}

/// Words a model emits that stand for no speech at all.
const NON_SPEECH: &[&str] = &[
    "music",
    "applause",
    "laughter",
    "laughs",
    "silence",
    "noise",
    "inaudible",
    "indistinct",
    "blank_audio",
    "blank audio",
    "sighs",
    "sigh",
    "coughs",
    "cough",
    "breathing",
    "beep",
    "static",
    "foreign language",
];

/// Fillers that only ever add noise. Kept short on purpose: anything that is
/// also a real word (`like`, `so`, `right`) is left alone.
const FILLERS: &[&str] = &[
    "um", "umm", "ummm", "uh", "uhh", "uhm", "uhhm", "erm", "hmm", "hmmm", "mhm", "mm-hm", "er",
    "eh", "ah",
];

fn artifact_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        let non_speech = NON_SPEECH
            .iter()
            .map(|word| regex::escape(word))
            .collect::<Vec<_>>()
            .join("|");
        // Bracketed or parenthesised markers, musical notes and subtitle arrows.
        // A bracket is only a marker when it holds a shouty tag (`[BLANK_AUDIO]`)
        // or a known non-speech word: `[1]` and `[a side note]` are text. The
        // spoken-punctuation phrases are handled by the token pass below.
        Regex::new(&format!(
            r"(?x)
            \[\s*(?:[A-Z][A-Z0-9_ '\-]{{0,39}}|(?i:{non_speech})[^\]\n]{{0,40}})\s*\]
            | \(\s*(?i:{non_speech})[^)\n]{{0,40}}\)
            | [♪♫]+
            | >>
            "
        ))
        .expect("the artifact pattern is valid")
    })
}

fn space_before_punctuation() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"\s+([,.;:!?%])").expect("valid"))
}

fn space_around_newline() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"[ \t]*\n[ \t]*").expect("valid"))
}

fn runs_of_space() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"[ \t]{2,}").expect("valid"))
}

fn sentence_start() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        // A sentence starts at the beginning, after terminal punctuation, or
        // after an opening bracket or quote.
        Regex::new(r#"(?m)(^[(\[{"“']?\s*|[.!?]\s+|\n\s*)([a-z])"#).expect("valid")
    })
}

fn repeated_space_before_closing() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"\s+([)\]}»”’])").expect("valid"))
}

/// Normalise `text` according to `options`.
pub fn clean(text: &str, options: &CleanupOptions) -> String {
    let mut current = text.to_string();

    if options.artifacts {
        current = artifact_regex().replace_all(&current, " ").into_owned();
    }

    // Token level rules: fillers, stutters and spoken punctuation. Tokens that
    // are nothing but punctuation are kept and glued back without a space.
    let mut pieces: Vec<Piece> = Vec::new();
    for token in current.split_whitespace() {
        let bare = token.trim_matches(|c: char| !c.is_alphanumeric());
        let lower = bare.to_lowercase();

        if options.fillers && FILLERS.contains(&lower.as_str()) {
            continue;
        }

        if options.spoken_punctuation {
            if let Some(replacement) = spoken_replacement(&lower) {
                // Keep trailing punctuation the model already wrote.
                let trailing: String = token
                    .chars()
                    .rev()
                    .take_while(|c| matches!(c, ',' | '.' | '?' | '!' | ';' | ':'))
                    .collect();
                pieces.push(Piece::Glued(format!("{replacement}{trailing}")));
                continue;
            }
        }

        if options.repeated_words {
            if let Some(Piece::Word(previous)) = pieces.last() {
                if previous.to_lowercase() == lower && !lower.is_empty() {
                    continue;
                }
            }
        }

        pieces.push(Piece::Word(token.to_string()));
    }

    current = join(pieces);

    if options.punctuation {
        current = space_before_punctuation()
            .replace_all(&current, "$1")
            .into_owned();
        current = repeated_space_before_closing()
            .replace_all(&current, "$1")
            .into_owned();
        current = space_around_newline()
            .replace_all(&current, "\n")
            .into_owned();
        current = runs_of_space().replace_all(&current, " ").into_owned();
        current = strip_leading_punctuation(&current);
        if !options.preserve_case {
            current = sentence_start()
                .replace_all(&current, |caps: &Captures| {
                    format!("{}{}", &caps[1], caps[2].to_uppercase())
                })
                .into_owned();
        }
    }

    current = current.trim().to_string();

    if !options.terminal.is_empty() && ends_a_sentence(&current) {
        let last = current.chars().last();
        let already = matches!(last, Some('.' | '!' | '?' | ':' | ';' | '\n'));
        if !already && !current.is_empty() {
            current.push_str(&options.terminal);
        }
    }

    // A lone `.` or leftover punctuation is not text.
    if current
        .chars()
        .all(|c| c.is_ascii_punctuation() || c.is_whitespace())
    {
        return String::new();
    }

    current
}

/// A token in the rebuilt text: words are separated by a space, punctuation is
/// glued to what precedes it.
#[derive(Debug, Clone)]
enum Piece {
    Word(String),
    Glued(String),
}

fn join(pieces: Vec<Piece>) -> String {
    let mut out = String::new();
    for piece in pieces {
        match piece {
            Piece::Word(word) => {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&word);
            }
            Piece::Glued(text) => {
                if text.starts_with('\n') {
                    out.truncate(out.trim_end().len());
                } else if !out.is_empty()
                    && !out.ends_with([' ', '\n'])
                    && !text.starts_with(['.', ',', ';', ':', '!', '?'])
                {
                    out.push(' ');
                }
                out.push_str(&text);
            }
        }
    }
    out
}

fn spoken_replacement(lower: &str) -> Option<&'static str> {
    Some(match lower {
        "comma" => ",",
        "period" | "full stop" | "fullstop" => ".",
        "question mark" | "questionmark" => "?",
        "exclamation mark" | "exclamationmark" | "exclamation point" | "exclamationpoint" => "!",
        "semicolon" => ";",
        "colon" => ":",
        "new line" | "newline" => "\n",
        "new paragraph" | "newparagraph" => "\n\n",
        "open quote" | "openquote" => "“",
        "close quote" | "closequote" => "”",
        _ => return None,
    })
}

fn strip_leading_punctuation(text: &str) -> String {
    text.trim_start_matches([',', '.', ';', ':', '!', '?', ' ', '\n'])
        .to_string()
}

/// True when appending `terminal` would make sense: the text ends in something
/// pronounceable, not in an opening bracket or a dash.
fn ends_a_sentence(text: &str) -> bool {
    match text.chars().last() {
        Some(c) => c.is_alphanumeric() || matches!(c, '"' | '\'' | '”' | '’' | ')' | ']'),
        None => false,
    }
}

/// Multi-token spoken punctuation (`full stop`) is handled before [`clean`]
/// tokenises; this expands the phrases so [`spoken_replacement`] sees one token.
pub fn expand_spoken_phrases(text: &str) -> String {
    let mut out = text.to_string();
    for (phrase, replacement) in [
        ("full stop", "fullstop"),
        ("question mark", "questionmark"),
        ("exclamation mark", "exclamationmark"),
        ("exclamation point", "exclamationpoint"),
        ("new paragraph", "newparagraph"),
        ("new line", "newline"),
        ("open quote", "openquote"),
        ("close quote", "closequote"),
    ] {
        let pattern = Regex::new(&format!(r"(?i)\b{}\b", phrase.replace(' ', r"\s+"))).unwrap();
        out = pattern.replace_all(&out, replacement).into_owned();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults(raw: &str) -> String {
        clean(&expand_spoken_phrases(raw), &CleanupOptions::default())
    }

    fn with(options: CleanupOptions, raw: &str) -> String {
        clean(&expand_spoken_phrases(raw), &options)
    }

    #[test]
    fn non_speech_markers_disappear() {
        assert_eq!(defaults("[BLANK_AUDIO]"), "");
        assert_eq!(defaults("(music playing)"), "");
        assert_eq!(defaults("♪ ♪"), "");
        assert_eq!(defaults("hello [BLANK_AUDIO] world"), "Hello world.");
        assert_eq!(defaults("hello (applause) world"), "Hello world.");
        assert_eq!(defaults("(inaudible)"), "");
    }

    #[test]
    fn brackets_that_are_not_noise_are_left_alone() {
        assert_eq!(defaults("see [1] and [2]"), "See [1] and [2].");
        assert_eq!(defaults("see [a side note]"), "See [a side note].");
        assert_eq!(defaults("[Music]"), "");
        assert_eq!(defaults("[INAUDIBLE]"), "");
    }

    #[test]
    fn fillers_are_removed_but_real_words_are_not() {
        assert_eq!(defaults("um, hello there"), "Hello there.");
        assert_eq!(defaults("I was, uh, thinking"), "I was, thinking.");
        assert_eq!(defaults("I like it"), "I like it.");
        // A word that merely contains a filler is untouched.
        assert_eq!(defaults("the umbrella"), "The umbrella.");
        assert_eq!(defaults("a humming bird"), "A humming bird.");
    }

    #[test]
    fn a_stutter_or_a_duplicated_prediction_collapses() {
        assert_eq!(defaults("the the cat"), "The cat.");
        assert_eq!(defaults("I I I think"), "I think.");
        assert_eq!(defaults("that is is fine"), "That is fine.");
    }

    #[test]
    fn spacing_and_capitalisation_are_repaired() {
        assert_eq!(defaults("hello , world ."), "Hello, world.");
        assert_eq!(defaults("  lots   of    space  "), "Lots of space.");
        assert_eq!(defaults("one. two? three! four"), "One. Two? Three! Four.");
        assert_eq!(defaults("a quote ."), "A quote.");
    }

    #[test]
    fn a_single_word_gets_a_capital_and_a_full_stop() {
        assert_eq!(defaults("hello"), "Hello.");
    }

    #[test]
    fn terminal_punctuation_is_not_doubled() {
        assert_eq!(defaults("hello."), "Hello.");
        assert_eq!(defaults("hello?"), "Hello?");
        assert_eq!(defaults("\"a quote\""), "\"A quote\".");
    }

    #[test]
    fn terminal_can_be_disabled_for_a_space_separator() {
        let options = CleanupOptions {
            terminal: " ".to_string(),
            ..CleanupOptions::default()
        };
        assert_eq!(with(options, "hello there"), "Hello there ");
    }

    #[test]
    fn everything_can_be_turned_off() {
        let raw = "um, [BLANK_AUDIO]  hello , world";
        // Nothing is removed; only the token spacing is normalised.
        assert_eq!(
            clean(raw, &CleanupOptions::none()),
            "um, [BLANK_AUDIO] hello , world"
        );
    }

    #[test]
    fn spoken_punctuation_is_opt_in() {
        // Off (the default for `defaults`): the words stay words.
        assert_eq!(defaults("hello comma world"), "Hello comma world.");

        let options = CleanupOptions {
            spoken_punctuation: true,
            ..CleanupOptions::default()
        };
        assert_eq!(
            with(options.clone(), "hello comma world period"),
            "Hello, world."
        );
        assert_eq!(
            with(options.clone(), "wait question mark really"),
            "Wait? Really."
        );
        assert_eq!(
            with(options, "first line new line second line"),
            "First line\nSecond line."
        );
    }

    #[test]
    fn empty_and_punctuation_only_input_stays_empty() {
        assert_eq!(defaults(""), "");
        assert_eq!(defaults("   "), "");
        assert_eq!(defaults("..."), "");
    }
}
