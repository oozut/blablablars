//! The plugin *spec*: one grammar for every transport.
//!
//! A spec is what a user writes, in a flag, an environment variable or the
//! config file:
//!
//! ```text
//! SPEC   ::= [ KIND ":" ] TARGET
//! KIND   ::= "rust" | "python" | "process"
//! ```
//!
//! `KIND` is matched case-insensitively and must be one of the three names
//! above; anything else before the first `:` is treated as part of the target,
//! so `C:\plugins\x.dll` keeps working. Without a prefix the kind is inferred
//! from the target's shape ([`PluginSpec::parse`] documents the table).
//!
//! New transports are added as new kinds here. That is the whole point: the CLI
//! flag, the environment variable and the config file do not grow per transport.

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

/// The transport a spec asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PluginKind {
    /// A compiled extension loaded with `dlopen` into the current process.
    Rust,
    /// A Python plugin executed by the embedded interpreter.
    Python,
    /// A plugin that runs as its own process and speaks JSON on stdio.
    Process,
}

impl PluginKind {
    /// Every kind, in the order they are documented and listed.
    pub const ALL: [Self; 3] = [Self::Rust, Self::Python, Self::Process];

    /// The name used in a spec (`rust:`, `python:`, `process:`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::Process => "process",
        }
    }

    /// Parse a spec prefix. Case-insensitive; unknown names are not prefixes.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| name.eq_ignore_ascii_case(kind.as_str()))
    }
}

impl fmt::Display for PluginKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a spec could not be parsed.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SpecError {
    #[error(
        "a plugin spec cannot be empty; write rust:PATH, python:PATH|MODULE or process:COMMAND"
    )]
    Empty,
    #[error("the plugin spec `{kind}:` has no target after the prefix")]
    EmptyTarget { kind: &'static str },
}

/// A parsed plugin spec: a kind and the thing to load.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginSpec {
    kind: PluginKind,
    target: String,
}

impl PluginSpec {
    /// Build a spec, rejecting an empty target.
    pub fn new(kind: PluginKind, target: impl Into<String>) -> Result<Self, SpecError> {
        let target = target.into();
        let target = target.trim();
        if target.is_empty() {
            return Err(SpecError::EmptyTarget {
                kind: kind.as_str(),
            });
        }
        Ok(Self {
            kind,
            target: target.to_string(),
        })
    }

    /// `rust:PATH` — a compiled extension.
    pub fn rust(path: impl Into<String>) -> Result<Self, SpecError> {
        Self::new(PluginKind::Rust, path)
    }

    /// `python:PATH|MODULE` — a Python plugin.
    pub fn python(path_or_module: impl Into<String>) -> Result<Self, SpecError> {
        Self::new(PluginKind::Python, path_or_module)
    }

    /// `process:COMMAND` — a child process plugin.
    pub fn process(command_line: impl Into<String>) -> Result<Self, SpecError> {
        Self::new(PluginKind::Process, command_line)
    }

    /// Parse a spec, inferring the kind when there is no prefix.
    ///
    /// | target looks like | kind |
    /// | --- | --- |
    /// | contains whitespace | `process` (a command line) |
    /// | ends in `.so`, `.dylib`, `.dll` | `rust` |
    /// | ends in `.py` | `python` |
    /// | ends in `.exe`, `.bat`, `.cmd`, `.sh`, `.ps1` | `process` |
    /// | a dotted module name (`my_company.audit`), no path separators | `python` |
    /// | anything else | `process` (a command line) |
    ///
    /// The inference exists for convenience on the command line; config files
    /// store the canonical (prefixed) form, which is what `blablablars plugin
    /// add` writes.
    pub fn parse(raw: &str) -> Result<Self, SpecError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(SpecError::Empty);
        }

        if let Some((prefix, rest)) = raw.split_once(':') {
            if let Some(kind) = PluginKind::parse(prefix) {
                return Self::new(kind, rest);
            }
        }

        Self::new(infer_kind(raw), raw)
    }

    /// The transport this spec asks for.
    pub fn kind(&self) -> PluginKind {
        self.kind
    }

    /// The path, module name or command line to hand to that transport.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The same spec with another target, e.g. after making a path absolute.
    pub fn with_target(&self, target: impl Into<String>) -> Result<Self, SpecError> {
        Self::new(self.kind, target)
    }
}

impl fmt::Display for PluginSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind.as_str(), self.target)
    }
}

impl FromStr for PluginSpec {
    type Err = SpecError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

/// Which transport a bare target asks for. See [`PluginSpec::parse`].
fn infer_kind(target: &str) -> PluginKind {
    // A target with spaces is a command line, whatever it ends in: `python3
    // plugin.py` is a child process, not a file to import. Paths that contain
    // spaces therefore need the `rust:`/`python:` prefix.
    if target.split_whitespace().count() > 1 {
        return PluginKind::Process;
    }

    let lower = target.to_ascii_lowercase();

    if lower.ends_with(".so") || lower.ends_with(".dylib") || lower.ends_with(".dll") {
        return PluginKind::Rust;
    }
    if lower.ends_with(".py") {
        return PluginKind::Python;
    }
    if [".exe", ".bat", ".cmd", ".sh", ".ps1"]
        .iter()
        .any(|extension| lower.ends_with(extension))
    {
        return PluginKind::Process;
    }
    if looks_like_a_module(target) {
        return PluginKind::Python;
    }
    PluginKind::Process
}

/// `my_company.audit` — dotted, no separators, no spaces, not a number.
fn looks_like_a_module(target: &str) -> bool {
    if !target.contains('.') || target.contains(['/', '\\', ' ', '\t']) {
        return false;
    }
    target.split('.').all(|part| {
        !part.is_empty()
            && !part.starts_with(|c: char| c.is_ascii_digit())
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_prefixes_win() {
        let spec = PluginSpec::parse("rust:/opt/libshout.so").unwrap();
        assert_eq!(spec.kind(), PluginKind::Rust);
        assert_eq!(spec.target(), "/opt/libshout.so");

        let spec = PluginSpec::parse("PYTHON:my_plugins.audit").unwrap();
        assert_eq!(spec.kind(), PluginKind::Python);
        assert_eq!(spec.target(), "my_plugins.audit");

        let spec = PluginSpec::parse("process:python3 -m blablablars.runtime p.py").unwrap();
        assert_eq!(spec.kind(), PluginKind::Process);
        assert_eq!(spec.target(), "python3 -m blablablars.runtime p.py");
    }

    #[test]
    fn prefixes_are_trimmed_but_targets_keep_their_content() {
        let spec = PluginSpec::parse("  rust: /opt/libshout.so  ").unwrap();
        assert_eq!(spec.to_string(), "rust:/opt/libshout.so");

        // A command line keeps its internal spacing.
        let spec = PluginSpec::parse("process:python3   -m   pkg").unwrap();
        assert_eq!(spec.target(), "python3   -m   pkg");
    }

    #[test]
    fn bare_targets_are_inferred_from_their_shape() {
        for (raw, kind) in [
            ("libshout.so", PluginKind::Rust),
            ("libshout.dylib", PluginKind::Rust),
            ("shout.dll", PluginKind::Rust),
            ("/opt/libshout.SO", PluginKind::Rust),
            ("plugin.py", PluginKind::Python),
            ("my_company.audit", PluginKind::Python),
            ("myapp.exe", PluginKind::Process),
            ("run.sh", PluginKind::Process),
            ("python3 -m blablablars.runtime p.py", PluginKind::Process),
            ("python3 plugin.py", PluginKind::Process),
            ("blablablars-worker", PluginKind::Process),
        ] {
            assert_eq!(PluginSpec::parse(raw).unwrap().kind(), kind, "for {raw:?}");
        }
    }

    #[test]
    fn something_that_only_looks_like_a_prefix_is_part_of_the_target() {
        // A Windows drive letter, a URL and a bare word are not kinds.
        let spec = PluginSpec::parse(r"C:\plugins\shout.dll").unwrap();
        assert_eq!(spec.kind(), PluginKind::Rust);
        assert_eq!(spec.target(), r"C:\plugins\shout.dll");

        let spec = PluginSpec::parse("https://example.test/plugin.py").unwrap();
        assert_eq!(spec.kind(), PluginKind::Python);
        assert_eq!(spec.target(), "https://example.test/plugin.py");

        // Only a prefix can name a path that contains spaces.
        let spec = PluginSpec::parse("/opt/my plugins/shout.so").unwrap();
        assert_eq!(spec.kind(), PluginKind::Process);
        let spec = PluginSpec::parse("rust:/opt/my plugins/shout.so").unwrap();
        assert_eq!(spec.kind(), PluginKind::Rust);

        let spec = PluginSpec::parse("make-plugin").unwrap();
        assert_eq!(spec.kind(), PluginKind::Process);
    }

    #[test]
    fn an_empty_spec_is_an_error_that_says_what_to_write() {
        let error = PluginSpec::parse("   ").unwrap_err();
        assert_eq!(error, SpecError::Empty);
        assert!(error.to_string().contains("rust:PATH"));

        let error = PluginSpec::parse("python:").unwrap_err();
        assert!(error.to_string().contains("python:"), "{error}");
    }

    #[test]
    fn a_spec_round_trips_through_its_canonical_form() {
        for raw in [
            "rust:/opt/libshout.so",
            "python:my_company.audit",
            "process:python3 -m blablablars.runtime p.py",
        ] {
            let spec = PluginSpec::parse(raw).unwrap();
            assert_eq!(spec.to_string(), raw);
            assert_eq!(spec.to_string().parse::<PluginSpec>().unwrap(), spec);
        }
    }

    #[test]
    fn kinds_are_listed_in_one_place() {
        assert_eq!(
            PluginKind::ALL.map(PluginKind::as_str),
            ["rust", "python", "process"]
        );
        assert_eq!(PluginKind::parse("Rust"), Some(PluginKind::Rust));
        assert_eq!(PluginKind::parse("rusty"), None);
    }
}
