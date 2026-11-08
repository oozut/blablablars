//! Putting the text where the user is typing.
//!
//! There are three ways to do this on a Linux desktop and none of them is
//! universal: `xdotool` on X11, `wtype` on Wayland compositors that implement
//! the virtual-keyboard protocol, and the clipboard as a fallback that requires
//! the user to paste. `auto` picks one from the session, and the choice is a
//! setting so a user can override it, including pointing it at their own
//! program through a `{text}` template.
//!
//! Nothing here is silent: if no tool can type, the error says which package to
//! install and that `stdout` always works.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::error::CoreError;

use super::config::{OutputConfig, OutputKind};
use super::platform;

/// Anything that can deliver text to the user.
pub trait Injector: Send + Sync {
    /// What is delivering the text, for logs and `--json`.
    fn describe(&self) -> String;

    /// Deliver `text` exactly as given.
    fn inject(&self, text: &str) -> Result<(), CoreError>;

    /// Deliver one piece of a streamed transcript.
    ///
    /// The difference only shows for terminals and files: `inject` ends the
    /// line, because a finished dictation is a finished line, while a streamed
    /// chunk has more words coming and must not start a new one.
    fn inject_chunk(&self, text: &str) -> Result<(), CoreError> {
        self.inject(text)
    }
}

/// Build the injector the config selects.
pub fn from_config(config: &OutputConfig) -> Result<Box<dyn Injector>, CoreError> {
    let kind = match config.kind {
        OutputKind::Auto => choose_auto()?,
        other => other,
    };

    Ok(match kind {
        OutputKind::Auto => unreachable!("choose_auto never returns auto"),
        OutputKind::Stdout => Box::new(StdoutInjector),
        OutputKind::Command => {
            if config.argv.is_empty() {
                return Err(CoreError::Voice(
                    "`output.kind: command` needs `output.argv`".to_string(),
                ));
            }
            Box::new(CommandInjector {
                argv: config.argv.clone(),
            })
        }
        OutputKind::Xdotool => {
            require("xdotool", "xdotool")?;
            Box::new(KeyInjector::Xdotool)
        }
        OutputKind::Wtype => {
            require("wtype", "wtype")?;
            Box::new(KeyInjector::Wtype)
        }
        OutputKind::Paste => Box::new(PasteInjector::detect()?),
    })
}

/// Pick a backend from the environment: Wayland first, because `DISPLAY` is set
/// under Xwayland even when `xdotool` cannot reach the focused window.
///
/// macOS and Windows have no command that types text without a helper, so they
/// paste: the clipboard is always reachable (`pbcopy`, `clip.exe`) and a paste
/// keystroke is one line of AppleScript or PowerShell. That is also what makes
/// them first-class rather than "set `output.kind: command` yourself".
fn choose_auto() -> Result<OutputKind, CoreError> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
    let display = std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty());
    decide(
        platform::desktop(),
        wayland,
        display,
        find_in_path("xdotool"),
        find_in_path("wtype"),
    )
    .ok_or_else(|| {
        CoreError::Voice(format!(
            "cannot type into the focused window: {}. Install one of them, set \
             `output.kind: stdout`, or point `output.argv` at your own tool.",
            missing_typing_tools_hint()
        ))
    })
}

fn missing_typing_tools_hint() -> String {
    if cfg!(windows) || cfg!(target_os = "macos") {
        "this system's clipboard or paste helper is missing".to_string()
    } else {
        "neither `xdotool` (X11) nor `wtype` (Wayland) is installed".to_string()
    }
}

/// The pure part of [`choose_auto`], so the preference order is testable.
fn decide(
    desktop: platform::Desktop,
    wayland: bool,
    display: bool,
    xdotool: Option<PathBuf>,
    wtype: Option<PathBuf>,
) -> Option<OutputKind> {
    match desktop {
        platform::Desktop::MacOs | platform::Desktop::Windows => return Some(OutputKind::Paste),
        platform::Desktop::Linux | platform::Desktop::Other => {}
    }
    if wayland {
        if wtype.is_some() {
            return Some(OutputKind::Wtype);
        }
        if xdotool.is_some() && display {
            // Xwayland-only compositors still accept synthetic X11 keys.
            return Some(OutputKind::Xdotool);
        }
        return None;
    }
    if display && xdotool.is_some() {
        return Some(OutputKind::Xdotool);
    }
    if wtype.is_some() {
        return Some(OutputKind::Wtype);
    }
    if xdotool.is_some() {
        return Some(OutputKind::Xdotool);
    }
    None
}

/// Puts the text on the clipboard, then sends the paste keystroke.
///
/// Two steps rather than typing character by character: it is what macOS and
/// Windows make easy, it keeps non-ASCII text intact without an input-method
/// dance, and it is what every other dictation tool does when it needs to be
/// quick.
pub struct PasteInjector {
    /// Receives the text on standard input.
    pub copy: Vec<String>,
    /// Run after the text is on the clipboard.
    pub paste: Vec<String>,
}

impl PasteInjector {
    /// Detect the clipboard and paste commands for this desktop.
    pub fn detect() -> Result<Self, CoreError> {
        let desktop = platform::desktop();
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
        let Some((copy, paste)) = paste_commands(desktop, wayland) else {
            return Err(CoreError::Voice(format!(
                "cannot paste: {}",
                paste_hint(desktop)
            )));
        };
        for program in [&copy[0], &paste[0]] {
            if find_in_path(program).is_none() {
                return Err(CoreError::Voice(format!(
                    "cannot paste: `{program}` is not installed. {}",
                    paste_hint(desktop)
                )));
            }
        }
        Ok(Self { copy, paste })
    }
}

fn paste_hint(desktop: platform::Desktop) -> String {
    match desktop {
        platform::Desktop::MacOs => "`pbcopy` and `osascript` ship with macOS".to_string(),
        platform::Desktop::Windows => "`clip.exe` and `powershell` ship with Windows".to_string(),
        _ => "install `wl-clipboard` and `wtype`, or `xclip` and `xdotool`".to_string(),
    }
}

/// The clipboard and paste commands for one desktop, pinned by tests.
pub fn paste_commands(
    desktop: platform::Desktop,
    wayland: bool,
) -> Option<(Vec<String>, Vec<String>)> {
    let owned = |parts: &[&str]| parts.iter().map(|part| part.to_string()).collect();
    Some(match desktop {
        platform::Desktop::MacOs => (
            owned(&["pbcopy"]),
            owned(&[
                "osascript",
                "-e",
                "tell application \"System Events\" to keystroke \"v\" using command down",
            ]),
        ),
        platform::Desktop::Windows => (
            owned(&["clip"]),
            owned(&[
                "powershell",
                "-NoProfile",
                "-Command",
                "$w = New-Object -ComObject WScript.Shell; $w.SendKeys('^v')",
            ]),
        ),
        _ if wayland => (
            owned(&["wl-copy"]),
            owned(&["wtype", "-M", "ctrl", "-k", "v", "-m", "ctrl"]),
        ),
        _ => (
            owned(&["xclip", "-selection", "clipboard"]),
            owned(&["xdotool", "key", "--clearmodifiers", "ctrl+v"]),
        ),
    })
}

impl Injector for PasteInjector {
    fn describe(&self) -> String {
        format!(
            "paste ({})",
            self.paste.first().cloned().unwrap_or_default()
        )
    }

    fn inject(&self, text: &str) -> Result<(), CoreError> {
        use std::io::Write as _;

        let copy = &self.copy[0];
        let mut child = Command::new(copy)
            .args(&self.copy[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| CoreError::Voice(format!("cannot run `{copy}`: {error}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| CoreError::Voice(format!("`{copy}` failed: {error}")))?;
        if !output.status.success() {
            return Err(CoreError::Voice(format!(
                "`{copy}` could not put the text on the clipboard: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        let paste = &self.paste[0];
        let status = Command::new(paste)
            .args(&self.paste[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .map_err(|error| CoreError::Voice(format!("cannot run `{paste}`: {error}")))?;
        if !status.success() {
            return Err(CoreError::Voice(format!(
                "`{paste}` could not send the paste keystroke: {status}"
            )));
        }
        Ok(())
    }
}

/// Print the text. Always available, and what CI and scripts use.
pub struct StdoutInjector;

impl Injector for StdoutInjector {
    fn describe(&self) -> String {
        "stdout".to_string()
    }

    fn inject(&self, text: &str) -> Result<(), CoreError> {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        write!(stdout, "{text}")?;
        if !text.ends_with('\n') {
            writeln!(stdout)?;
        }
        stdout.flush()?;
        Ok(())
    }

    fn inject_chunk(&self, text: &str) -> Result<(), CoreError> {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        write!(stdout, "{text}")?;
        stdout.flush()?;
        Ok(())
    }
}

/// Run a program with the text substituted into `{text}`.
pub struct CommandInjector {
    pub argv: Vec<String>,
}

impl Injector for CommandInjector {
    fn describe(&self) -> String {
        format!(
            "command ({})",
            self.argv.first().cloned().unwrap_or_else(|| "?".into())
        )
    }

    fn inject(&self, text: &str) -> Result<(), CoreError> {
        let mut argv: Vec<String> = self
            .argv
            .iter()
            .map(|argument| argument.replace("{text}", text))
            .collect();
        if !self.argv.iter().any(|argument| argument.contains("{text}")) {
            argv.push(text.to_string());
        }

        let program = argv
            .first()
            .cloned()
            .ok_or_else(|| CoreError::Voice("`output.argv` is empty".to_string()))?;
        let status = Command::new(&program)
            .args(&argv[1..])
            .stdin(Stdio::null())
            .status()
            .map_err(|error| CoreError::Voice(format!("cannot run `{program}`: {error}")))?;
        if !status.success() {
            return Err(CoreError::Voice(format!(
                "`{program}` failed to deliver the text: {status}"
            )));
        }
        Ok(())
    }
}

/// Synthetic keystrokes through an external tool.
pub enum KeyInjector {
    Xdotool,
    Wtype,
}

impl Injector for KeyInjector {
    fn describe(&self) -> String {
        match self {
            Self::Xdotool => "xdotool".to_string(),
            Self::Wtype => "wtype".to_string(),
        }
    }

    fn inject(&self, text: &str) -> Result<(), CoreError> {
        let (program, arguments) = self.command(text);
        let output = Command::new(program)
            .args(&arguments)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| CoreError::Voice(format!("cannot run `{program}`: {error}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CoreError::Voice(format!(
                "`{program}` could not type into the focused window: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }
}

impl KeyInjector {
    /// The exact command line, so the argument handling can be pinned by a test.
    pub fn command(&self, text: &str) -> (&'static str, Vec<String>) {
        match self {
            // `--` so text starting with a dash is not read as an option, and
            // `--clearmodifiers` so the shortcut's held modifiers do not turn
            // the first characters into accelerators.
            Self::Xdotool => (
                "xdotool",
                vec![
                    "type".to_string(),
                    "--clearmodifiers".to_string(),
                    "--delay".to_string(),
                    "0".to_string(),
                    "--".to_string(),
                    text.to_string(),
                ],
            ),
            Self::Wtype => ("wtype", vec!["--".to_string(), text.to_string()]),
        }
    }
}

/// Resolve a program on `PATH` without spawning anything.
pub use super::platform::find_program as find_in_path;

fn require(program: &str, package: &str) -> Result<(), CoreError> {
    if find_in_path(program).is_some() {
        return Ok(());
    }
    Err(CoreError::Voice(format!(
        "`{program}` is not installed. Install the `{package}` package, set \
         `output.kind: stdout`, or point `output.command` at your own tool."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_prefers_wtype_even_when_xwayland_sets_display() {
        assert_eq!(
            decide(
                platform::Desktop::Linux,
                true,
                true,
                Some("/bin/xdotool".into()),
                Some("/bin/wtype".into())
            ),
            Some(OutputKind::Wtype)
        );
    }

    #[test]
    fn x11_prefers_xdotool() {
        assert_eq!(
            decide(
                platform::Desktop::Linux,
                false,
                true,
                Some("/bin/xdotool".into()),
                Some("/bin/wtype".into())
            ),
            Some(OutputKind::Xdotool)
        );
    }

    #[test]
    fn wayland_without_a_tool_does_not_guess_xdotool_when_display_is_absent() {
        assert_eq!(
            decide(
                platform::Desktop::Linux,
                true,
                false,
                Some("/bin/xdotool".into()),
                None
            ),
            None
        );
    }

    #[test]
    fn a_headless_session_has_no_backend() {
        assert_eq!(
            decide(platform::Desktop::Linux, false, false, None, None),
            None
        );
    }

    #[test]
    fn one_tool_is_enough_even_without_a_session_variable() {
        assert_eq!(
            decide(
                platform::Desktop::Linux,
                false,
                false,
                Some("/bin/xdotool".into()),
                None
            ),
            Some(OutputKind::Xdotool)
        );
        assert_eq!(
            decide(
                platform::Desktop::Linux,
                false,
                false,
                None,
                Some("/bin/wtype".into())
            ),
            Some(OutputKind::Wtype)
        );
    }

    #[test]
    fn xdotool_gets_clearmodifiers_and_a_double_dash() {
        let (program, argv) = KeyInjector::Xdotool.command("-hello world");
        assert_eq!(program, "xdotool");
        assert_eq!(argv[0], "type");
        assert!(argv.contains(&"--clearmodifiers".to_string()));
        assert_eq!(argv.last().unwrap(), "-hello world");
        assert_eq!(argv[argv.len() - 2], "--");
    }

    #[test]
    fn wtype_gets_a_double_dash_too() {
        let (program, argv) = KeyInjector::Wtype.command("--not-an-option");
        assert_eq!(program, "wtype");
        assert_eq!(argv, ["--", "--not-an-option"]);
    }

    #[test]
    fn a_command_injector_substitutes_the_text_and_appends_when_absent() {
        let path = std::env::temp_dir().join(format!("blb-inject-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        CommandInjector {
            argv: vec![
                "sh".to_string(),
                "-c".to_string(),
                format!("printf '%s' \"$1\" >> {}", path.display()),
                "sh".to_string(),
                "{text}".to_string(),
            ],
        }
        .inject("hello, world")
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello, world");

        // No placeholder: the text becomes the last argument.
        CommandInjector {
            argv: vec![
                "sh".to_string(),
                "-c".to_string(),
                format!("printf '%s' \"$1\" >> {}", path.display()),
                "sh".to_string(),
            ],
        }
        .inject("!")
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello, world!");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_failing_command_is_an_error() {
        let error = CommandInjector {
            argv: vec!["sh".to_string(), "-c".to_string(), "exit 7".to_string()],
        }
        .inject("x")
        .unwrap_err();
        assert!(error.to_string().contains("failed to deliver"), "{error}");
    }

    #[test]
    fn a_missing_typing_tool_says_what_to_install() {
        let error = require("blablablars-no-such-tool", "xdotool").unwrap_err();
        assert!(error.to_string().contains("xdotool"), "{error}");
        assert!(error.to_string().contains("stdout"), "{error}");
    }

    #[test]
    fn macos_and_windows_always_paste() {
        // Neither system ships a command that types text into the focused
        // window; the clipboard is the only route, and it is always there.
        for desktop in [platform::Desktop::MacOs, platform::Desktop::Windows] {
            assert_eq!(
                decide(desktop, false, false, None, None),
                Some(OutputKind::Paste)
            );
        }
    }

    #[test]
    fn every_desktop_has_a_clipboard_and_a_paste_keystroke() {
        let mac = paste_commands(platform::Desktop::MacOs, false).unwrap();
        assert_eq!(mac.0, vec!["pbcopy"]);
        assert_eq!(mac.1[0], "osascript");
        assert!(mac.1[2].contains("command down"));

        let windows = paste_commands(platform::Desktop::Windows, false).unwrap();
        assert_eq!(windows.0, vec!["clip"]);
        assert_eq!(windows.1[0], "powershell");
        assert!(windows.1[3].contains("SendKeys('^v')"));

        let wayland = paste_commands(platform::Desktop::Linux, true).unwrap();
        assert_eq!(wayland.0, vec!["wl-copy"]);
        assert_eq!(
            wayland.1,
            vec!["wtype", "-M", "ctrl", "-k", "v", "-m", "ctrl"]
        );

        let x11 = paste_commands(platform::Desktop::Linux, false).unwrap();
        assert_eq!(x11.0, vec!["xclip", "-selection", "clipboard"]);
        assert_eq!(x11.1, vec!["xdotool", "key", "--clearmodifiers", "ctrl+v"]);
    }

    #[test]
    fn pasting_puts_the_text_on_the_clipboard_then_sends_the_keystroke() {
        let directory = std::env::temp_dir().join(format!(
            "blb-paste-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let clipboard = directory.join("clipboard");
        let pasted = directory.join("pasted");

        // The two commands are swapped for something observable: `copy` reads
        // its standard input, `paste` copies that file somewhere else.
        let injector = PasteInjector {
            copy: vec![
                "sh".into(),
                "-c".into(),
                format!("cat > {}", clipboard.display()),
            ],
            paste: vec![
                "sh".into(),
                "-c".into(),
                format!("cp {} {}", clipboard.display(), pasted.display()),
            ],
        };
        injector.inject("hallo wörld").unwrap();

        assert_eq!(std::fs::read_to_string(&pasted).unwrap(), "hallo wörld");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_copy_that_fails_reports_it_and_does_not_paste() {
        let injector = PasteInjector {
            copy: vec!["sh".into(), "-c".into(), "exit 3".into()],
            paste: vec!["sh".into(), "-c".into(), "echo should-not-run".into()],
        };
        let error = injector.inject("text").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("could not put the text on the clipboard"),
            "{error}"
        );
    }

    #[test]
    fn a_program_path_is_found_directly() {
        assert!(find_in_path("/bin/sh").is_some());
        assert!(find_in_path("sh").is_some());
        assert!(find_in_path("blablablars-no-such-tool").is_none());
    }
}
