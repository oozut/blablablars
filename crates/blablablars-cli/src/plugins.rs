//! Attaching plugins, the one way: a spec, a config file, a directory.
//!
//! Every place a plugin can be named uses [`PluginSpec`]'s grammar, so this
//! module has a single implementation of "which plugins does this invocation
//! use" and a single place to touch when a transport is added.
//!
//! Sources, in the order they are attached:
//!
//! 1. `--plugin SPEC` on the command line, in order.
//! 2. `$BLABLABLARS_PLUGINS`, one spec per line.
//! 3. The config file (`blablablars plugin add` writes it).
//! 4. `--plugin-path DIR` directories, files in name order.
//! 5. `$BLABLABLARS_PLUGIN_PATH` directories.
//!
//! Steps 2, 3 and 5 are the *implicit* sources and are skipped by
//! `--no-plugins`; steps 1 and 4 are explicit and always apply. Nothing here is
//! discovered by walking the filesystem or the current directory, so running the
//! binary in an untrusted repository cannot execute code.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use blablablars_core::spec::{PluginKind, PluginSpec};
use blablablars_core::{CoreError, DynamicExtension, Extension, Pipeline, ProcessExtension};

/// Plugin specs, one per line. The same format as the config file, so a line
/// can be moved between the two unchanged.
pub const ENV_SPECS: &str = "BLABLABLARS_PLUGINS";
/// `PATH`-separated directories scanned for `*.so`/`*.dylib`/`*.dll`/`*.py`.
pub const ENV_PLUGIN_PATH: &str = "BLABLABLARS_PLUGIN_PATH";
/// An alternative config file, used by every subcommand that loads plugins.
pub const ENV_CONFIG: &str = "BLABLABLARS_PLUGIN_CONFIG";
/// `PATH`-separated directories added to the embedded interpreter's `sys.path`.
#[cfg(feature = "python")]
pub const ENV_PYTHON_PATH: &str = "BLABLABLARS_PYTHON_PATH";

const CONFIG_HEADER: &str = "\
# blablablars plugins -- one spec per line:
#   rust:PATH | python:PATH|MODULE | process:COMMAND
# Manage with `blablablars plugin add|list|remove`.
";

/// What the command line asked for. Filled in from `clap` arguments, kept
/// separate so the config commands can be exercised without a terminal.
#[derive(Debug, Default)]
pub struct AttachOptions {
    /// `--plugin` specs, in the order given.
    pub specs: Vec<PluginSpec>,
    /// `--plugin-path` directories.
    pub directories: Vec<PathBuf>,
    /// `--plugin-config`, or `None` for the default location.
    pub config: Option<PathBuf>,
    /// `--python-path` search directories.
    pub python_paths: Vec<PathBuf>,
    /// Read `$BLABLABLARS_PLUGINS` and the config file (`--no-plugins` turns
    /// this off).
    pub implicit: bool,
}

impl AttachOptions {
    /// Read the environment and the config file as well as the flags.
    pub fn with_implicit_sources(mut self) -> Self {
        self.implicit = true;
        self
    }
}

/// Where a spec came from, because the two are treated differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Named by the user: `--plugin`, `$BLABLABLARS_PLUGINS`, or the config file.
    Named,
    /// Found by scanning a directory, so not chosen one file at a time.
    Scanned,
}

/// Build a pipeline with every plugin this invocation asks for.
pub fn build(options: &AttachOptions) -> Result<Pipeline, CoreError> {
    let collected = collect(options)?;
    let specs: Vec<PluginSpec> = collected.iter().map(|(spec, _)| spec.clone()).collect();

    start_interpreter(&specs, &options.python_paths)?;

    let mut pipeline = Pipeline::new();
    for (spec, origin) in &collected {
        match attach(&mut pipeline, spec) {
            Ok(()) => {}
            // A directory is scanned, not chosen: a library in it that does not
            // export the entry point is skipped and said so, because a `.so` that
            // is not a plugin (a C library sitting beside them) would otherwise
            // take the whole directory down. Anything named explicitly is still
            // an error: there, "not a plugin" is a mistake worth stopping for.
            Err(error @ CoreError::NotAnExtension { .. }) if origin == &Origin::Scanned => {
                eprintln!("warning: skipping `{spec}`: {error}");
            }
            Err(error) => {
                return Err(CoreError::Plugin(format!(
                    "cannot attach `{spec}`: {error}"
                )))
            }
        }
    }
    Ok(pipeline)
}

/// Attach one plugin to a pipeline.
pub fn attach(pipeline: &mut Pipeline, spec: &PluginSpec) -> Result<(), CoreError> {
    pipeline.register(load(spec)?)
}

/// Load one plugin, without registering it.
pub fn load(spec: &PluginSpec) -> Result<Arc<dyn Extension>, CoreError> {
    let extension: Arc<dyn Extension> = match spec.kind() {
        PluginKind::Rust => Arc::new(DynamicExtension::open(spec.target())?),
        PluginKind::Process => Arc::new(ProcessExtension::spawn_line(spec.target())?),
        #[cfg(feature = "python")]
        PluginKind::Python => blablablars_pythonhost::load_path_or_module(spec.target())?,
        #[cfg(not(feature = "python"))]
        PluginKind::Python => {
            return Err(CoreError::Plugin(format!(
                "this build was compiled without embedded Python, so `{spec}` cannot run in-process; \
                 rebuild with the default features or use a `process:COMMAND` spec"
            )));
        }
    };
    Ok(extension)
}

/// The specs this invocation uses, in attachment order, each with its origin.
fn collect(options: &AttachOptions) -> Result<Vec<(PluginSpec, Origin)>, CoreError> {
    let named = |spec: PluginSpec| (spec, Origin::Named);
    let mut specs: Vec<(PluginSpec, Origin)> =
        options.specs.clone().into_iter().map(named).collect();

    if options.implicit {
        warn_about_legacy_environment();
        if let Some(text) = non_empty_env(ENV_SPECS) {
            specs.extend(parse_specs(&text.to_string_lossy())?.into_iter().map(named));
        }
        specs.extend(
            read_config(&config_file(options.config.as_deref())?)?
                .into_iter()
                .map(named),
        );
    }

    let mut directories = options.directories.clone();
    if options.implicit {
        directories.extend(environment_directories(ENV_PLUGIN_PATH));
    }
    for directory in &directories {
        specs.extend(
            specs_in_directory(directory)?
                .into_iter()
                .map(|spec| (spec, Origin::Scanned)),
        );
    }

    Ok(specs)
}

/// Parse the config-file format: one spec per line, `#` comments and blank
/// lines ignored, inline comments *not* supported (a command line may contain
/// `#`).
pub fn parse_specs(text: &str) -> Result<Vec<PluginSpec>, CoreError> {
    let mut specs = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let spec = PluginSpec::parse(line)
            .map_err(|error| CoreError::Plugin(format!("line {}: {error}", index + 1)))?;
        specs.push(spec);
    }
    Ok(specs)
}

/// The config file in use: the explicit path, then `$BLABLABLARS_PLUGIN_CONFIG`,
/// then the per-user location.
pub fn config_file(explicit: Option<&Path>) -> Result<PathBuf, CoreError> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(value) = non_empty_env(ENV_CONFIG) {
        return Ok(PathBuf::from(value));
    }
    let directory = non_empty_env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| non_empty_env("APPDATA").map(PathBuf::from))
        .or_else(|| non_empty_env("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| {
            CoreError::Plugin(format!(
                "no configuration directory: set {ENV_CONFIG} to choose a plugin list explicitly"
            ))
        })?;
    Ok(directory.join("blablablars").join("plugins"))
}

/// Every spec in the config file; an absent file is an empty list.
pub fn read_config(path: &Path) -> Result<Vec<PluginSpec>, CoreError> {
    match fs::read_to_string(path) {
        Ok(text) => parse_specs(&text)
            .map_err(|error| CoreError::Plugin(format!("{}: {error}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// Whether `add` changed the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddOutcome {
    Added(PluginSpec),
    AlreadyPresent(PluginSpec),
}

impl AddOutcome {
    /// The spec as it was stored (canonical, files absolute).
    pub fn spec(&self) -> &PluginSpec {
        match self {
            Self::Added(spec) | Self::AlreadyPresent(spec) => spec,
        }
    }

    /// Whether the file changed.
    pub fn added(&self) -> bool {
        matches!(self, Self::Added(_))
    }
}

/// Append a spec to the config file, storing it in canonical form.
///
/// Existing lines are left untouched — comments survive — and a spec that is
/// already listed is not added twice.
pub fn add(config: &Path, spec: &PluginSpec) -> Result<AddOutcome, CoreError> {
    let spec = normalise(spec)?;
    if read_config(config)?.contains(&spec) {
        return Ok(AddOutcome::AlreadyPresent(spec));
    }

    if let Some(parent) = config.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let mut text = fs::read_to_string(config).unwrap_or_default();
    if text.is_empty() {
        text.push_str(CONFIG_HEADER);
    } else if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&spec.to_string());
    text.push('\n');

    let mut file = fs::File::create(config)?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    Ok(AddOutcome::Added(spec))
}

/// Remove a spec from the config file; `false` when it was not listed.
pub fn remove(config: &Path, spec: &PluginSpec) -> Result<bool, CoreError> {
    let text = match fs::read_to_string(config) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };

    let mut kept = String::with_capacity(text.len());
    let mut removed = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            if let Ok(stored) = PluginSpec::parse(trimmed) {
                if matches(&stored, spec) {
                    removed = true;
                    continue;
                }
            }
        }
        kept.push_str(line);
        kept.push('\n');
    }

    if removed {
        fs::write(config, kept)?;
    }
    Ok(removed)
}

/// Rewrite a spec so it means the same thing wherever it is read from: file
/// targets become absolute paths, and anything that does not exist is rejected
/// now rather than at the next start.
pub fn normalise(spec: &PluginSpec) -> Result<PluginSpec, CoreError> {
    if !looks_like_a_path(spec) {
        return Ok(spec.clone());
    }
    let path = expand_home(spec.target());
    let absolute = fs::canonicalize(&path).map_err(|error| {
        CoreError::Plugin(format!("no plugin file `{}`: {error}", path.display()))
    })?;
    Ok(spec.with_target(absolute.to_string_lossy().into_owned())?)
}

/// `python:my_company.audit` is a module; `python:./audit.py` is a file.
fn looks_like_a_path(spec: &PluginSpec) -> bool {
    let target = spec.target();
    spec.kind() == PluginKind::Rust
        || target.ends_with(".py")
        || target.contains('/')
        || target.contains('\\')
}

fn expand_home(target: &str) -> PathBuf {
    if let Some(rest) = target.strip_prefix("~/") {
        if let Some(home) = non_empty_env("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(target)
}

/// Two specs name the same plugin when they parse the same, when the target is
/// verbatim equal, or when one is the absolute form of the other (a config file
/// written before the plugin moved is still removable).
fn matches(stored: &PluginSpec, wanted: &PluginSpec) -> bool {
    if stored == wanted || (stored.kind() == wanted.kind() && stored.target() == wanted.target()) {
        return true;
    }
    match normalise(wanted) {
        Ok(absolute) => stored == &absolute,
        Err(_) => false,
    }
}

/// The plugins found in a directory: shared libraries and Python files, in name
/// order. Anything else is ignored.
pub fn specs_in_directory(directory: &Path) -> Result<Vec<PluginSpec>, CoreError> {
    let entries = fs::read_dir(directory).map_err(|error| {
        CoreError::Plugin(format!(
            "cannot read plugin directory `{}`: {error}",
            directory.display()
        ))
    })?;

    let mut paths = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.is_file() {
            paths.push(path);
        }
    }
    paths.sort();

    let mut specs = Vec::new();
    for path in paths {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let target = path.to_string_lossy().into_owned();
        match extension.as_str() {
            "so" | "dylib" | "dll" => specs.push(PluginSpec::rust(target)?),
            "py" => specs.push(PluginSpec::python(target)?),
            _ => {}
        }
    }
    Ok(specs)
}

fn start_interpreter(specs: &[PluginSpec], python_paths: &[PathBuf]) -> Result<(), CoreError> {
    #[cfg(feature = "python")]
    {
        let mut search = python_paths.to_vec();
        search.extend(environment_directories(ENV_PYTHON_PATH));
        let wants_python = specs.iter().any(|spec| spec.kind() == PluginKind::Python);
        if wants_python || !search.is_empty() {
            blablablars_pythonhost::initialize(&search)?;
        }
    }
    #[cfg(not(feature = "python"))]
    {
        let _ = (specs, python_paths);
    }
    Ok(())
}

fn environment_directories(name: &str) -> Vec<PathBuf> {
    match non_empty_env(name) {
        Some(value) => env::split_paths(&value).collect(),
        None => Vec::new(),
    }
}

fn non_empty_env(name: &str) -> Option<OsString> {
    env::var_os(name).filter(|value| !value.is_empty())
}

/// Variables that only ever held one kind of plugin. Say where the behaviour
/// went instead of silently ignoring them.
fn warn_about_legacy_environment() {
    for (name, replacement) in [
        ("BLABLABLARS_RUST_PLUGINS", "BLABLABLARS_PLUGINS=rust:PATH"),
        ("BLABLABLARS_RUST_PLUGIN_DIRS", "BLABLABLARS_PLUGIN_PATH"),
        (
            "BLABLABLARS_PYTHON_PLUGINS",
            "BLABLABLARS_PLUGINS=python:PATH",
        ),
    ] {
        if non_empty_env(name).is_some() {
            eprintln!("warning: {name} is no longer read; use {replacement}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty directory per test.
    fn scratch(name: &str) -> PathBuf {
        let path = env::temp_dir().join(format!("blb-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn the_config_format_is_one_spec_per_line_with_comments() {
        let specs = parse_specs(
            "\
# a comment
rust:/opt/libshout.so

python:my_company.audit   # not an inline comment: whole-line only
process:python3 -m blablablars.runtime p.py
",
        )
        .unwrap();
        assert_eq!(
            specs.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec![
                "rust:/opt/libshout.so",
                // The trailing `# ...` belongs to the target, which is why
                // inline comments are not a thing.
                "python:my_company.audit   # not an inline comment: whole-line only",
                "process:python3 -m blablablars.runtime p.py",
            ]
        );
    }

    #[test]
    fn a_bad_line_names_its_number() {
        let error = parse_specs("rust:/ok.so\npython:\n").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("line 2"), "{message}");
        assert!(message.contains("python:"), "{message}");
    }

    #[test]
    fn add_stores_the_canonical_form_and_never_twice() {
        let directory = scratch("add");
        let library = directory.join("libshout.so");
        fs::write(&library, b"not really a library").unwrap();
        let config = directory.join("plugins");

        // A bare path becomes a prefixed, canonical spec.
        let bare = PluginSpec::parse(&library.display().to_string()).unwrap();
        let first = add(&config, &bare).unwrap();
        assert!(first.added());
        let stored = first.spec().clone();
        assert_eq!(stored.kind(), PluginKind::Rust);
        assert!(Path::new(stored.target()).is_absolute());

        let again = add(&config, &bare).unwrap();
        assert_eq!(again, AddOutcome::AlreadyPresent(stored.clone()));
        assert_eq!(read_config(&config).unwrap(), vec![stored.clone()]);

        let text = fs::read_to_string(&config).unwrap();
        assert_eq!(text, format!("{CONFIG_HEADER}{stored}\n"));
    }

    #[test]
    fn remove_keeps_comments_and_the_other_plugins() {
        let directory = scratch("remove");
        let library = directory.join("libshout.so");
        fs::write(&library, b"x").unwrap();
        let config = directory.join("plugins");

        let library_spec = PluginSpec::parse(&library.display().to_string()).unwrap();
        add(&config, &library_spec).unwrap();
        add(
            &config,
            &PluginSpec::parse("process:blablablars-worker").unwrap(),
        )
        .unwrap();

        assert!(remove(&config, &library_spec).unwrap());
        let remaining = read_config(&config).unwrap();
        assert_eq!(
            remaining
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["process:blablablars-worker"]
        );

        let text = fs::read_to_string(&config).unwrap();
        assert!(text.starts_with(CONFIG_HEADER), "{text}");
        assert!(!remove(&config, &library_spec).unwrap());
    }

    #[test]
    fn add_refuses_a_file_that_is_not_there() {
        let directory = scratch("missing");
        let error = add(
            &directory.join("plugins"),
            &PluginSpec::parse("rust:./nope.so").unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no plugin file"), "{error}");
    }

    #[test]
    fn a_module_spec_is_stored_as_written() {
        let directory = scratch("module");
        let outcome = add(
            &directory.join("plugins"),
            &PluginSpec::parse("my_company.audit").unwrap(),
        )
        .unwrap();
        assert_eq!(
            outcome.spec().to_string(),
            "python:my_company.audit",
            "a dotted name is a module, not a path"
        );
    }

    #[test]
    fn a_directory_yields_one_spec_per_plugin_in_name_order() {
        let directory = scratch("dir");
        fs::write(directory.join("b.py"), b"").unwrap();
        fs::write(directory.join("a.so"), b"").unwrap();
        fs::write(directory.join("notes.txt"), b"").unwrap();

        let specs = specs_in_directory(&directory).unwrap();
        assert_eq!(
            specs
                .iter()
                .map(|spec| (
                    spec.kind(),
                    Path::new(spec.target()).file_name().unwrap().to_owned()
                ))
                .collect::<Vec<_>>(),
            [
                (PluginKind::Rust, std::ffi::OsString::from("a.so")),
                (PluginKind::Python, std::ffi::OsString::from("b.py")),
            ]
        );
    }

    /// Some shared library on this machine that is certainly not an extension.
    #[cfg(unix)]
    fn system_library() -> Option<PathBuf> {
        [
            "/lib/x86_64-linux-gnu/libm.so.6",
            "/usr/lib/x86_64-linux-gnu/libm.so.6",
            "/lib/aarch64-linux-gnu/libm.so.6",
            "/usr/lib/aarch64-linux-gnu/libm.so.6",
            "/lib64/libm.so.6",
            "/usr/lib/libm.so.6",
            "/usr/lib/libSystem.B.dylib",
        ]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
    }

    /// A directory holding one library that is not a plugin.
    #[cfg(unix)]
    fn directory_with_a_plain_library(name: &str) -> Option<(PathBuf, PathBuf)> {
        let library = system_library()?;
        let directory = scratch(name);
        let extension = if cfg!(target_os = "macos") {
            "dylib"
        } else {
            "so"
        };
        let path = directory.join(format!("libnotaplugin.{extension}"));
        fs::copy(&library, &path).ok()?;
        Some((directory, path))
    }

    #[cfg(unix)]
    #[test]
    fn a_library_in_a_scanned_directory_that_is_not_an_extension_is_skipped() {
        let Some((directory, _)) = directory_with_a_plain_library("scan-skip") else {
            return;
        };
        let options = AttachOptions {
            directories: vec![directory.clone()],
            ..Default::default()
        };

        // The directory is scanned, so one stray `.so` beside the plugins must
        // not take the whole directory down.
        let pipeline = build(&options).expect("a plain library is skipped, not fatal");
        assert!(pipeline.registry().is_empty());

        let _ = fs::remove_dir_all(&directory);
    }

    #[cfg(unix)]
    #[test]
    fn a_named_library_that_is_not_an_extension_is_still_an_error() {
        let Some((directory, path)) = directory_with_a_plain_library("scan-named") else {
            return;
        };
        let options = AttachOptions {
            specs: vec![PluginSpec::rust(path.to_string_lossy().into_owned()).unwrap()],
            ..Default::default()
        };

        // Named by hand, "not a plugin" is a mistake worth stopping for.
        let error = build(&options).unwrap_err().to_string();
        assert!(error.contains("cannot attach"), "{error}");
        assert!(error.contains("not a blablablars extension"), "{error}");

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_missing_directory_is_an_error_not_an_empty_list() {
        let error = specs_in_directory(Path::new("/nonexistent/blb-plugins")).unwrap_err();
        assert!(
            error.to_string().contains("cannot read plugin directory"),
            "{error}"
        );
    }
}
