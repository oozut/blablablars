//! End-to-end tests for the `blablablars` binary.
//!
//! They deliberately avoid third-party dev-dependencies: the binary path comes
//! from `CARGO_BIN_EXE_*` and plugins are plain `python3` scripts.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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
        "pipeline.input\npipeline.output\n"
    );

    let listed = json(&run(&["hooks", "--json"]));
    assert_eq!(
        listed,
        serde_json::json!(["pipeline.input", "pipeline.output"])
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

/// Build `examples/rust-plugin` on demand (incremental, separate target dir).
fn rust_extension() -> PathBuf {
    let root = workspace_root();
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let target_dir = root.join("target").join("plugin-fixture");
    let artifact = target_dir.join(profile).join(fixture_library_name());

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .arg("build")
        .arg("--manifest-path")
        .arg(root.join("examples/rust-plugin/Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        .output()
        .expect("cargo must be runnable to build the example extension");
    assert!(
        output.status.success(),
        "building examples/rust-plugin failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    artifact
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
    let output = run(&[
        "run",
        "hello brave world",
        "--json",
        "--rust-plugin",
        &library.display().to_string(),
    ]);

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

    let output = run(&[
        "run",
        "hi",
        "--json",
        "--rust-plugin",
        &library.display().to_string(),
        "--python-path",
        &package.display().to_string(),
        "--python-plugin",
        &plugin.display().to_string(),
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
        .env("BLABLABLARS_PLUGINS", &plugin)
        .output()
        .expect("the binary should run");

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "from-env: pipeline.input\n");
}

#[test]
fn a_missing_rust_extension_fails_with_a_message() {
    let output = run(&["run", "hi", "--rust-plugin", "/nonexistent/plugin.so"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("could not load the shared library"));
}
