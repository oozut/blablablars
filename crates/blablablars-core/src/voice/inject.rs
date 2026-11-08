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

/// Anything that can deliver text to the user.
pub trait Injector: Send + Sync {
    /// What is delivering the text, for logs and `--json`.
    fn describe(&self) -> String;

    /// Deliver `text` exactly as given.
    fn inject(&self, text: &str) -> Result<(), CoreError>;
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
    })
}

/// Pick a backend from the environment: Wayland first, because `DISPLAY` is set
/// under Xwayland even when `xdotool` cannot reach the focused window.
fn choose_auto() -> Result<OutputKind, CoreError> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
    let display = std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty());
    decide(
        wayland,
        display,
        find_in_path("xdotool"),
        find_in_path("wtype"),
    )
    .ok_or_else(|| {
        CoreError::Voice(
            "cannot type into the focused window: neither `xdotool` (X11) nor `wtype` (Wayland) \
             is installed. Install one of them, set `output.kind: stdout`, or point \
             `output.argv` at your own tool."
                .to_string(),
        )
    })
}

/// The pure part of [`choose_auto`], so the preference order is testable.
fn decide(
    wayland: bool,
    display: bool,
    xdotool: Option<PathBuf>,
    wtype: Option<PathBuf>,
) -> Option<OutputKind> {
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
pub fn find_in_path(program: &str) -> Option<PathBuf> {
    if program.contains('/') || program.contains('\\') {
        let path = PathBuf::from(program);
        return path.is_file().then_some(path);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

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
        assert_eq!(decide(true, false, Some("/bin/xdotool".into()), None), None);
    }

    #[test]
    fn a_headless_session_has_no_backend() {
        assert_eq!(decide(false, false, None, None), None);
    }

    #[test]
    fn one_tool_is_enough_even_without_a_session_variable() {
        assert_eq!(
            decide(false, false, Some("/bin/xdotool".into()), None),
            Some(OutputKind::Xdotool)
        );
        assert_eq!(
            decide(false, false, None, Some("/bin/wtype".into())),
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
    fn a_program_path_is_found_directly() {
        assert!(find_in_path("/bin/sh").is_some());
        assert!(find_in_path("sh").is_some());
        assert!(find_in_path("blablablars-no-such-tool").is_none());
    }
}
