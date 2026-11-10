//! Where things live, and which desktop this is, on Linux, macOS and Windows.
//!
//! Dictation has to work on all three, so every path and every "what program
//! does this" question goes through this module instead of assuming XDG
//! directories or a Unix session. The rules are the ones each platform
//! documents, `XDG_*` included, because that is what lets the tests, a sandbox
//! and a user who keeps everything in one folder all point the same variable at
//! a temporary directory.
//!
//! | | Linux | macOS | Windows |
//! |---|---|---|---|
//! | config | `$XDG_CONFIG_HOME` | `~/Library/Application Support` | `%APPDATA%` |
//! | data | `$XDG_DATA_HOME` | `~/Library/Application Support` | `%LOCALAPPDATA%` |
//! | logs | `$XDG_STATE_HOME` | `~/Library/Logs` | `%LOCALAPPDATA%` |
//! | runtime | `$XDG_RUNTIME_DIR` | `$TMPDIR` | `%TEMP%` |

use std::path::{Path, PathBuf};

/// The desktop conventions this build runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Desktop {
    Linux,
    MacOs,
    Windows,
    /// A Unix that is not Linux or macOS (the BSDs), or something we have not
    /// taught the commands to yet.
    Other,
}

/// The desktop this binary was compiled for.
pub const fn desktop() -> Desktop {
    if cfg!(target_os = "linux") {
        Desktop::Linux
    } else if cfg!(target_os = "macos") {
        Desktop::MacOs
    } else if cfg!(windows) {
        Desktop::Windows
    } else {
        Desktop::Other
    }
}

/// A platform directory, without the `blablablars` component.
///
/// The order is: the `XDG_*` variable, then the variable the platform itself
/// documents, then a folder under the home directory. `XDG_*` wins on every
/// system — Linux keeps it as its native convention, and on macOS and Windows it
/// is what lets a sandbox, a test or a user who keeps everything in one folder
/// redirect the whole application.
fn platform_dir(
    get: &dyn Fn(&str) -> Option<PathBuf>,
    xdg_var: &str,
    native_var: Option<&str>,
    home: &[&str],
) -> Option<PathBuf> {
    let set = |name: &str| get(name).filter(|dir| !dir.as_os_str().is_empty());
    if let Some(dir) = set(xdg_var) {
        return Some(dir);
    }
    if let Some(dir) = native_var.and_then(set) {
        return Some(dir);
    }
    let base = home_dir_from(get)?;
    Some(home.iter().fold(base, |path, part| path.join(part)))
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// The user's home directory: `$HOME`, or `%USERPROFILE%` on Windows.
pub fn home_dir() -> Option<PathBuf> {
    home_dir_from(&env_dir)
}

fn home_dir_from(get: &dyn Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    get("HOME")
        .filter(|dir| !dir.as_os_str().is_empty())
        .or_else(|| get("USERPROFILE").filter(|dir| !dir.as_os_str().is_empty()))
}

/// `~/.config/blablablars` on Linux, the application-support folder elsewhere.
pub fn config_dir() -> Option<PathBuf> {
    config_dir_from(&env_dir)
}

fn config_dir_from(get: &dyn Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    let base = if cfg!(windows) {
        platform_dir(get, "XDG_CONFIG_HOME", Some("APPDATA"), &[])
    } else if cfg!(target_os = "macos") {
        platform_dir(
            get,
            "XDG_CONFIG_HOME",
            None,
            &["Library", "Application Support"],
        )
    } else {
        platform_dir(get, "XDG_CONFIG_HOME", None, &[".config"])
    };
    base.map(|dir| dir.join("blablablars"))
}

/// `~/.local/share/blablablars` on Linux, the application-support folder
/// elsewhere. Models live in `models/` inside it.
pub fn data_dir() -> Option<PathBuf> {
    data_dir_from(&env_dir)
}

fn data_dir_from(get: &dyn Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    let base = if cfg!(windows) {
        platform_dir(get, "XDG_DATA_HOME", Some("LOCALAPPDATA"), &[])
    } else if cfg!(target_os = "macos") {
        platform_dir(
            get,
            "XDG_DATA_HOME",
            None,
            &["Library", "Application Support"],
        )
    } else {
        platform_dir(get, "XDG_DATA_HOME", None, &[".local", "share"])
    };
    base.map(|dir| dir.join("blablablars"))
}

/// `~/.local/state/blablablars` on Linux, `~/Library/Logs/blablablars` on macOS,
/// `%LOCALAPPDATA%\blablablars` on Windows.
pub fn state_dir() -> Option<PathBuf> {
    state_dir_from(&env_dir)
}

fn state_dir_from(get: &dyn Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    let base = if cfg!(windows) {
        platform_dir(get, "XDG_STATE_HOME", Some("LOCALAPPDATA"), &[])
    } else if cfg!(target_os = "macos") {
        platform_dir(get, "XDG_STATE_HOME", None, &["Library", "Logs"])
    } else {
        platform_dir(get, "XDG_STATE_HOME", None, &[".local", "state"])
    };
    base.map(|dir| dir.join("blablablars"))
}

/// The shortest-lived directory: a recording session's handshake file goes
/// here, because it must not survive a reboot.
pub fn runtime_dir() -> Option<PathBuf> {
    runtime_dir_from(&env_dir)
}

fn runtime_dir_from(get: &dyn Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    if let Some(dir) = get("XDG_RUNTIME_DIR").filter(|dir| !dir.as_os_str().is_empty()) {
        return Some(dir.join("blablablars"));
    }
    let base = get("TMPDIR")
        .or_else(|| get("TEMP"))
        .or_else(|| get("TMP"))
        .unwrap_or_else(std::env::temp_dir);
    Some(base.join("blablablars"))
}

/// Find a program on `PATH`, honouring the platform's executable suffixes.
///
/// On Windows a bare `whisper-cli` means `whisper-cli.exe`, and the search order
/// is `PATHEXT` (`%PATHEXT%` when set). Everywhere else the name is the name.
pub fn find_program(program: &str) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    if program.contains('/') || program.contains('\\') {
        let path = PathBuf::from(program);
        return path.is_file().then_some(path);
    }
    let path = std::env::var_os("PATH")?;
    let suffixes: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|suffix| !suffix.trim().is_empty())
            .map(|suffix| suffix.trim().to_lowercase())
            .chain(std::iter::once(String::new()))
            .collect()
    } else {
        vec![String::new()]
    };
    for directory in std::env::split_paths(&path) {
        for suffix in &suffixes {
            let candidate = directory.join(format!("{program}{suffix}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// A per-user tag for a file name, so two accounts never share one session.
pub fn user_tag() -> String {
    for variable in ["USER", "USERNAME", "LOGNAME"] {
        if let Ok(value) = std::env::var(variable) {
            let cleaned: String = value
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !cleaned.is_empty() {
                return cleaned;
            }
        }
    }
    "user".to_string()
}

/// Expand a leading `~` to the user's home directory.
pub fn expand_home(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let Some(rest) = text.strip_prefix('~') else {
        return path.to_path_buf();
    };
    let Some(home) = home_dir() else {
        return path.to_path_buf();
    };
    let rest = rest.trim_start_matches(['/', '\\']);
    if rest.is_empty() {
        home
    } else {
        home.join(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<PathBuf> {
        let entries: Vec<(String, PathBuf)> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), PathBuf::from(value)))
            .collect();
        move |key| {
            entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        }
    }

    /// A home directory that is absolute *on the platform being compiled for*,
    /// which is the point of the paths below: `/home/ana` has no drive letter,
    /// so it is not absolute on Windows and would make the test lie.
    fn home_path() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Users\ana")
        } else {
            PathBuf::from("/home/ana")
        }
    }

    fn native_profile() -> impl Fn(&str) -> Option<PathBuf> {
        let home = home_path();
        let appdata = home.join("AppData").join("Roaming");
        let localappdata = home.join("AppData").join("Local");
        let entries: Vec<(String, PathBuf)> = vec![
            ("HOME".to_string(), home.clone()),
            ("USERPROFILE".to_string(), home),
            ("APPDATA".to_string(), appdata),
            ("LOCALAPPDATA".to_string(), localappdata),
        ];
        move |key| {
            entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn every_platform_has_a_config_directory() {
        let get = native_profile();
        let config = config_dir_from(&get).unwrap();
        assert!(config.ends_with("blablablars"), "{}", config.display());
        assert!(config.is_absolute(), "{}", config.display());

        let home = home_path();
        if cfg!(windows) {
            // Windows documents `%APPDATA%` for configuration and
            // `%LOCALAPPDATA%` for everything the machine owns.
            assert_eq!(config, home.join("AppData/Roaming/blablablars"));
            assert_eq!(
                data_dir_from(&get).unwrap(),
                home.join("AppData/Local/blablablars")
            );
            assert_eq!(
                state_dir_from(&get).unwrap(),
                home.join("AppData/Local/blablablars")
            );
        } else if cfg!(target_os = "macos") {
            assert_eq!(config, home.join("Library/Application Support/blablablars"));
            assert_eq!(
                data_dir_from(&get).unwrap(),
                home.join("Library/Application Support/blablablars")
            );
            assert_eq!(
                state_dir_from(&get).unwrap(),
                home.join("Library/Logs/blablablars")
            );
        } else {
            assert_eq!(config, home.join(".config/blablablars"));
            assert_eq!(
                data_dir_from(&get).unwrap(),
                home.join(".local/share/blablablars")
            );
            assert_eq!(
                state_dir_from(&get).unwrap(),
                home.join(".local/state/blablablars")
            );
        }
    }

    #[test]
    fn the_xdg_variables_are_the_way_out_on_every_platform() {
        // A sandbox sets these; that has to work on macOS and Windows too, or
        // every test would write into the developer's real profile.
        let get = lookup(&[
            ("XDG_CONFIG_HOME", "/scratch/conf"),
            ("XDG_DATA_HOME", "/scratch/data"),
            ("XDG_STATE_HOME", "/scratch/state"),
            ("XDG_RUNTIME_DIR", "/scratch/run"),
            ("APPDATA", "/users/ana/AppData/Roaming"),
            ("LOCALAPPDATA", "/users/ana/AppData/Local"),
            ("HOME", "/users/ana"),
        ]);
        assert_eq!(
            config_dir_from(&get).unwrap(),
            PathBuf::from("/scratch/conf/blablablars")
        );
        assert_eq!(
            data_dir_from(&get).unwrap(),
            PathBuf::from("/scratch/data/blablablars")
        );
        assert_eq!(
            state_dir_from(&get).unwrap(),
            PathBuf::from("/scratch/state/blablablars")
        );
        assert_eq!(
            runtime_dir_from(&get).unwrap(),
            PathBuf::from("/scratch/run/blablablars")
        );
    }

    #[test]
    fn xdg_variables_win_over_the_home_directory() {
        let dir = config_dir_from(&lookup(&[
            ("HOME", "/home/ana"),
            ("XDG_CONFIG_HOME", "/scratch/conf"),
        ]))
        .unwrap();
        assert_eq!(dir, PathBuf::from("/scratch/conf/blablablars"));

        let data = data_dir_from(&lookup(&[
            ("HOME", "/home/ana"),
            ("XDG_DATA_HOME", "/scratch/data"),
        ]))
        .unwrap();
        assert_eq!(data, PathBuf::from("/scratch/data/blablablars"));

        let state = state_dir_from(&lookup(&[
            ("HOME", "/home/ana"),
            ("XDG_STATE_HOME", "/scratch/state"),
        ]))
        .unwrap();
        assert_eq!(state, PathBuf::from("/scratch/state/blablablars"));
    }

    #[test]
    fn the_runtime_directory_prefers_the_session_directory() {
        let dir = runtime_dir_from(&lookup(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("TMPDIR", "/tmp"),
        ]))
        .unwrap();
        assert_eq!(dir, PathBuf::from("/run/user/1000/blablablars"));

        let fallback = runtime_dir_from(&lookup(&[("TMPDIR", "/tmp")])).unwrap();
        assert_eq!(fallback, PathBuf::from("/tmp/blablablars"));
    }

    #[test]
    fn an_empty_variable_is_the_same_as_an_unset_one() {
        let dir =
            config_dir_from(&lookup(&[("HOME", "/home/ana"), ("XDG_CONFIG_HOME", "")])).unwrap();
        assert!(dir.starts_with("/home/ana"));
    }

    #[test]
    fn windows_falls_back_to_userprofile_for_home() {
        let home = home_dir_from(&lookup(&[("USERPROFILE", "/users/ana")])).unwrap();
        assert_eq!(home, PathBuf::from("/users/ana"));
    }

    #[test]
    fn the_user_tag_is_a_file_name() {
        let tag = user_tag();
        assert!(!tag.is_empty());
        assert!(tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn a_tilde_is_the_home_directory() {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        if let Some(home) = home {
            assert_eq!(expand_home(Path::new("~/models")), home.join("models"));
        }
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
    fn a_program_is_found_on_the_path_like_the_shell_would() {
        // `cmd` on Windows, `sh` everywhere else: one program that is always
        // there, so the test says the same thing on all three systems.
        let name = if cfg!(windows) { "cmd" } else { "sh" };
        let found = find_program(name).expect("the shell is on every PATH");
        assert!(found.is_file(), "{}", found.display());

        // A path is used as it is, with no search — and a bare name is not a
        // path, so neither `..` nor a directory can smuggle one in.
        let absolute = found.to_str().unwrap();
        assert!(find_program(absolute).unwrap().is_file());
        assert!(find_program("blablablars-no-such-program").is_none());
        assert!(find_program("").is_none());
        assert!(find_program("/no/such/directory/sh").is_none());
    }
}
