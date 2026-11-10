//! End-to-end tests for the `blablablars` binary.
//!
//! They deliberately avoid third-party dev-dependencies: the binary path comes
//! from `CARGO_BIN_EXE_*` and plugins are plain `python3` scripts.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

fn blablablars() -> Command {
    Command::new(env!("CARGO_BIN_EXE_blablablars"))
}

fn run(args: &[&str]) -> Output {
    blablablars()
        .args(args)
        .output()
        .expect("the binary should run")
}

fn run_stdin(args: &[&str], stdin: &str) -> Output {
    let mut child = blablablars()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary should run");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(stdin.as_bytes())
        .expect("stdin should accept writes");
    child.wait_with_output().expect("the binary should finish")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output)).expect("stdout should be JSON")
}

/// Write a protocol-compliant plugin to a temporary directory.
fn temporary_plugin(name: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("blablablars-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir should be creatable");
    let path = dir.join(format!("{name}.py"));
    std::fs::write(&path, body).expect("plugin should be writable");
    path
}

const UPPERCASE_PLUGIN: &str = r#"
import json, sys

for line in sys.stdin:
    request = json.loads(line)
    method = request["method"]
    if method == "describe":
        result = {"protocol": 1, "name": "uppercase", "hooks": ["pipeline.input"]}
        sys.stdout.write(json.dumps({"id": request["id"], "result": result}) + "\n")
    elif method == "call":
        payload = request["params"]["payload"]
        payload["text"] = payload["text"].upper()
        sys.stdout.write(json.dumps({"id": request["id"], "result": payload}) + "\n")
    elif method == "shutdown":
        sys.stdout.write(json.dumps({"id": request["id"], "result": {}}) + "\n")
        sys.stdout.flush()
        break
    sys.stdout.flush()
"#;

#[test]
fn version_is_reported_plainly_and_as_json() {
    let plain = run(&["version"]);
    assert!(plain.status.success());
    assert!(stdout(&plain).starts_with("blablablars "));

    let payload = json(&run(&["version", "--json"]));
    assert_eq!(payload["name"], "blablablars");
    assert_eq!(payload["protocol"], 1);
}

#[test]
fn hooks_are_listed() {
    assert_eq!(
        stdout(&run(&["hooks"])),
        "pipeline.input\npipeline.output\ntranscription.raw\ntranscription.text\n"
    );

    let listed = json(&run(&["hooks", "--json"]));
    assert_eq!(
        listed,
        serde_json::json!([
            "pipeline.input",
            "pipeline.output",
            "transcription.raw",
            "transcription.text"
        ])
    );
}

#[test]
fn run_prints_a_human_report() {
    let output = run(&["run", "hello brave world"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("text:       hello brave world"));
    assert!(text.contains("words:      3"));
    assert!(text.contains("characters: 17"));
    assert!(text.contains("trace:      (no extensions)"));
}

#[test]
fn run_json_matches_the_shared_shape() {
    let payload = json(&run(&["run", "hello world", "--json"]));
    assert_eq!(payload["analysis"]["text"], "hello world");
    assert_eq!(payload["analysis"]["words"], 2);
    assert_eq!(payload["analysis"]["characters"], 11);
    assert_eq!(payload["trace"], serde_json::json!([]));
}

#[test]
fn run_reads_standard_input_when_no_text_is_given() {
    let output = run_stdin(&["run", "--json"], "piped text\n");
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(json(&output)["analysis"]["text"], "piped text");
}

#[test]
fn a_plugin_can_change_the_result() {
    let plugin = temporary_plugin("uppercase", UPPERCASE_PLUGIN);
    let command = format!("python3 {}", plugin.display());

    let output = run(&["run", "shout", "--json", "--plugin", &command]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let payload = json(&output);
    assert_eq!(payload["analysis"]["text"], "SHOUT");
    assert_eq!(
        payload["trace"],
        serde_json::json!(["uppercase: pipeline.input"])
    );
}

#[test]
fn plugins_are_described_after_the_handshake() {
    let plugin = temporary_plugin("uppercase-described", UPPERCASE_PLUGIN);
    let command = format!("python3 {}", plugin.display());

    let output = run(&["plugins", "--plugin", &command]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "uppercase: pipeline.input\n");
}

#[test]
fn a_broken_plugin_fails_the_command_with_a_message() {
    let plugin = temporary_plugin(
        "broken",
        "import sys\nsys.stdout.write('not json\\n')\nsys.stdout.flush()\nimport time; time.sleep(5)\n",
    );
    let command = format!("python3 {}", plugin.display());

    let output = run(&["run", "hi", "--plugin", &command]);

    assert!(!output.status.success());
    assert!(stderr(&output).contains("plugin"));
}

#[test]
fn a_missing_program_fails_with_a_helpful_message() {
    let output = run(&["run", "hi", "--plugin", "definitely-not-a-real-program-xyz"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("could not start plugin"));
}

// ---------------------------------------------------------------------------
// In-process extensions: compiled Rust libraries and embedded Python plugins.
// ---------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/blablablars-cli is two levels below the root")
        .to_path_buf()
}

fn fixture_library_name() -> String {
    let stem = if cfg!(target_os = "windows") {
        "blablablars_example_rust_plugin"
    } else {
        "libblablablars_example_rust_plugin"
    };
    let extension = if cfg!(target_os = "windows") {
        "dll"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    format!("{stem}.{extension}")
}

/// The fixture `cdylib`, built once (incremental, separate target dir).
///
/// Once, because half the tests in this file want it: two cargo runs sharing a
/// target directory are a race — one can finish while the other is uplifting the
/// artefact, and the loser is left with a fingerprint and no file.
fn rust_extension() -> PathBuf {
    static ARTIFACT: OnceLock<PathBuf> = OnceLock::new();
    ARTIFACT
        .get_or_init(|| build_rust_extension().unwrap_or_else(|error| panic!("{error}")))
        .clone()
}

fn build_rust_extension() -> Result<PathBuf, String> {
    let root = workspace_root();
    let target_dir = root.join("target").join("plugin-fixture");
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };

    let mut last = String::new();
    for attempt in 0..2 {
        if attempt == 1 {
            // Cargo says "Finished" without touching anything when it thinks the
            // crate is up to date, and a cache that restores fingerprints but
            // not artefacts produces exactly that. Taking the fingerprints away
            // is cheap next to a mystery, and the dependencies stay built.
            let _ = std::fs::remove_dir_all(target_dir.join(profile).join(".fingerprint"));
        }
        let (artifact, stderr) = run_fixture_build(&root, &target_dir)?;
        match artifact {
            Some(artifact) => return Ok(artifact),
            None => last = stderr,
        }
    }
    Err(format!(
        "building examples/rust-plugin reported success but produced no {}:\n{last}",
        fixture_library_name()
    ))
}

fn run_fixture_build(root: &Path, target_dir: &Path) -> Result<(Option<PathBuf>, String), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .arg("build")
        .arg("--manifest-path")
        .arg(root.join("examples/rust-plugin/Cargo.toml"))
        .arg("--target-dir")
        .arg(target_dir)
        // Ask cargo where the file is instead of guessing: `build.target` in a
        // cargo config, or `CARGO_BUILD_TARGET` in the environment, moves it
        // under `target/plugin-fixture/<triple>/`.
        .arg("--message-format=json-render-diagnostics")
        .output()
        .map_err(|error| {
            format!("cargo must be runnable to build the example extension: {error}")
        })?;
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() {
        return Err(format!("building examples/rust-plugin failed:\n{stderr}"));
    }
    Ok((artifact_in_messages(&output.stdout), stderr))
}

/// The fixture library among cargo's JSON messages.
fn artifact_in_messages(stdout: &[u8]) -> Option<PathBuf> {
    let wanted = fixture_library_name();
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact")
        .filter_map(|message| message["filenames"].as_array().cloned())
        .flatten()
        .filter_map(|file| file.as_str().map(PathBuf::from))
        .find(|path| {
            path.file_name()
                .is_some_and(|file| file.to_string_lossy() == wanted)
        })
}

/// The Python package, if it has been built (`maturin develop`).
fn python_package() -> Option<PathBuf> {
    let directory = workspace_root().join("python");
    let package = directory.join("blablablars");
    let built = std::fs::read_dir(&package)
        .ok()?
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().starts_with("_native"));
    built.then_some(directory)
}

const IN_PROCESS_PLUGIN: &str = r#"
from blablablars import HOOK_INPUT, hook

@hook(HOOK_INPUT)
def shout(payload):
    payload["text"] = "[" + payload["text"].upper() + "]"
    return payload
"#;

#[test]
fn a_rust_extension_runs_inside_the_binary() {
    let library = rust_extension();
    let spec = format!("rust:{}", library.display());
    let output = run(&["run", "hello brave world", "--json", "--plugin", &spec]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let payload = json(&output);
    assert_eq!(payload["analysis"]["text"], "[shouted] HELLO BRAVE WORLD");
    assert_eq!(
        payload["trace"],
        serde_json::json!(["shout: pipeline.input", "shout: pipeline.output"])
    );
}

#[test]
fn in_process_rust_and_python_extensions_share_one_pipeline() {
    let library = rust_extension();
    let Some(package) = python_package() else {
        eprintln!("skipping: build the Python package with `maturin develop` first");
        return;
    };
    let plugin = temporary_plugin("in-process", IN_PROCESS_PLUGIN);

    let rust_spec = format!("rust:{}", library.display());
    let python_spec = format!("python:{}", plugin.display());
    let output = run(&[
        "run",
        "hi",
        "--json",
        "--plugin",
        &rust_spec,
        "--plugin",
        &python_spec,
        "--python-path",
        &package.display().to_string(),
    ]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let payload = json(&output);
    // The Rust extension uppercases, then the Python one brackets the result:
    // both ran in this process, in registration order.
    assert_eq!(payload["analysis"]["text"], "[[SHOUTED] HI]");
    let trace = payload["trace"].as_array().expect("a trace array");
    assert_eq!(trace.len(), 3, "trace: {trace:?}");
    assert!(trace[0].as_str().unwrap().starts_with("shout: "));
    assert!(trace[1].as_str().unwrap().starts_with("in-process: "));
}

#[test]
fn python_plugins_can_be_loaded_from_the_environment() {
    let Some(package) = python_package() else {
        eprintln!("skipping: build the Python package with `maturin develop` first");
        return;
    };
    let plugin = temporary_plugin("from-env", IN_PROCESS_PLUGIN);

    let output = blablablars()
        .args(["plugins"])
        .env("BLABLABLARS_PYTHON_PATH", &package)
        // One spec per line, the same format as the config file.
        .env("BLABLABLARS_PLUGINS", plugin.display().to_string())
        .output()
        .expect("the binary should run");

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "from-env: pipeline.input\n");
}

#[test]
fn a_missing_rust_extension_fails_with_a_message() {
    let output = run(&["run", "hi", "--plugin", "rust:/nonexistent/plugin.so"]);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(
        message.contains("could not load the shared library"),
        "{message}"
    );
}

#[test]
fn a_bare_spec_is_read_by_its_shape() {
    let library = rust_extension();
    let output = run(&["plugins", "--plugin", &library.display().to_string()]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "shout: pipeline.input, pipeline.output\n");
}

#[test]
fn a_directory_contributes_one_plugin_per_file() {
    let directory = scratch("plugin-dir");
    let library = rust_extension();
    std::fs::copy(&library, directory.join(fixture_library_name()))
        .expect("the fixture should be copyable");
    std::fs::write(directory.join("ignored.txt"), "not a plugin").expect("writable");

    let output = run(&["plugins", "--plugin-path", &directory.display().to_string()]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "shout: pipeline.input, pipeline.output\n");
}

#[test]
fn an_unreadable_spec_says_what_to_write() {
    let output = run(&["run", "hi", "--plugin", "python:"]);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("no target after the prefix"), "{message}");
}

// ---------------------------------------------------------------------------
// The plugin list: `blablablars plugin add|list|remove|path`.
// ---------------------------------------------------------------------------

fn scratch(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("blablablars-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("temp dir should be creatable");
    directory
}

#[test]
fn a_plugin_added_to_the_list_is_loaded_by_every_run() {
    let config = scratch("add").join("plugins");
    let config = config.display().to_string();
    let library = rust_extension();

    let added = run(&[
        "plugin",
        "add",
        &library.display().to_string(),
        "--config",
        &config,
    ]);
    assert!(added.status.success(), "stderr: {}", stderr(&added));
    assert!(stdout(&added).contains("added rust:"), "{}", stdout(&added));

    // Added twice on purpose: the second time must not duplicate the line.
    let again = run(&[
        "plugin",
        "add",
        &library.display().to_string(),
        "--config",
        &config,
    ]);
    assert!(again.status.success(), "stderr: {}", stderr(&again));
    assert!(stdout(&again).contains("already in"), "{}", stdout(&again));

    let listed = run(&["plugin", "list", "--config", &config]);
    assert_eq!(stdout(&listed).trim().matches("rust:").count(), 1);

    // Every run reads that list -- no flags needed beyond the file.
    let output = run(&["run", "hi", "--json", "--plugin-config", &config]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(json(&output)["analysis"]["text"], "[shouted] HI");

    let removed = run(&[
        "plugin",
        "remove",
        &library.display().to_string(),
        "--config",
        &config,
    ]);
    assert!(removed.status.success(), "stderr: {}", stderr(&removed));

    let output = run(&["run", "hi", "--json", "--plugin-config", &config]);
    assert_eq!(json(&output)["analysis"]["text"], "hi");

    // Removing what is not there is a failure, so scripts notice.
    let missing = run(&[
        "plugin",
        "remove",
        &library.display().to_string(),
        "--config",
        &config,
    ]);
    assert!(!missing.status.success());
    assert!(stderr(&missing).contains("not in"));
}

#[test]
fn the_plugin_list_is_machine_readable() {
    let config = scratch("list-json").join("plugins");
    let config = config.display().to_string();
    let library = rust_extension();
    run(&[
        "plugin",
        "add",
        &library.display().to_string(),
        "--config",
        &config,
    ]);

    let payload = json(&run(&["plugin", "list", "--json", "--config", &config]));
    assert_eq!(payload["config"], config);
    assert_eq!(payload["plugins"][0]["kind"], "rust");
    assert!(payload["plugins"][0]["target"]
        .as_str()
        .expect("a target")
        .ends_with(&fixture_library_name()));
}

#[test]
fn the_plugin_list_path_can_be_asked_for() {
    let directory = scratch("path");
    let config = directory.join("plugins");

    let printed = run(&["plugin", "path", "--config", &config.display().to_string()]);
    assert!(printed.status.success(), "stderr: {}", stderr(&printed));
    assert_eq!(stdout(&printed).trim(), config.display().to_string());

    // Without --config the user's own file is the one that would be read, and
    // XDG_CONFIG_HOME decides where it lives.
    let printed = blablablars()
        .args(["plugin", "path"])
        .env("XDG_CONFIG_HOME", &directory)
        .output()
        .expect("the binary should run");
    assert_eq!(
        stdout(&printed).trim(),
        directory
            .join("blablablars")
            .join("plugins")
            .display()
            .to_string()
    );
}

#[test]
fn no_plugins_ignores_the_list_and_the_environment() {
    let directory = scratch("no-plugins");
    let config = directory.join("plugins");
    let library = rust_extension();
    std::fs::write(&config, format!("rust:{}\n", library.display())).expect("writable");

    let loaded = run(&[
        "run",
        "hi",
        "--json",
        "--plugin-config",
        &config.display().to_string(),
    ]);
    assert_eq!(json(&loaded)["analysis"]["text"], "[shouted] HI");

    let ignored = run(&[
        "run",
        "hi",
        "--json",
        "--no-plugins",
        "--plugin-config",
        &config.display().to_string(),
    ]);
    assert_eq!(json(&ignored)["analysis"]["text"], "hi");

    // Explicit flags are not "implicit": they still apply.
    let explicit = format!("rust:{}", library.display());
    let still = run(&["run", "hi", "--json", "--no-plugins", "--plugin", &explicit]);
    assert_eq!(json(&still)["analysis"]["text"], "[shouted] HI");
}

#[test]
fn the_environment_holds_specs_one_per_line() {
    let library = rust_extension();
    let specs = format!(
        "# a comment\nrust:{}\nprocess:python3 {}",
        library.display(),
        temporary_plugin("env-order", UPPERCASE_PLUGIN).display()
    );

    let output = blablablars()
        .args(["run", "hi", "--json"])
        .env("BLABLABLARS_PLUGINS", specs)
        // Keep the developer's own plugin list out of the test.
        .env(
            "BLABLABLARS_PLUGIN_CONFIG",
            scratch("env-empty").join("plugins"),
        )
        .env_remove("BLABLABLARS_PLUGIN_PATH")
        .output()
        .expect("the binary should run");

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let payload = json(&output);
    // Both plugins ran, in spec order: the Rust one subscribes twice.
    assert_eq!(
        payload["trace"],
        serde_json::json!([
            "shout: pipeline.input",
            "uppercase: pipeline.input",
            "shout: pipeline.output"
        ])
    );
}
