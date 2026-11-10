//! End-to-end test of the dynamic (in-process) Rust extension path.
//!
//! The fixture in `examples/rust-plugin` is a real `cdylib`. It is built on
//! demand into a separate target directory: a nested `cargo` run must not fight
//! the outer one for the lock on `target/debug`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

use blablablars_core::{DynamicExtension, Extension, Pipeline, HOOK_INPUT, HOOK_OUTPUT};
use serde_json::json;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/blablablars-core is two levels below the root")
        .to_path_buf()
}

fn library_file_name() -> String {
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

/// Build (once, incrementally) and return the fixture `cdylib`.
///
/// Always invoking cargo keeps the fixture in step with the ABI: a stale
/// artifact is rebuilt instead of being reported as an ABI mismatch.
fn fixture() -> PathBuf {
    static ARTIFACT: OnceLock<PathBuf> = OnceLock::new();
    ARTIFACT.get_or_init(build_fixture).clone()
}

fn build_fixture() -> PathBuf {
    let root = workspace_root();
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let target_dir = root.join("target").join("plugin-fixture");
    let artifact = target_dir.join(profile).join(library_file_name());

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
    assert!(
        artifact.exists(),
        "expected the fixture at {}",
        artifact.display()
    );
    artifact
}

#[test]
fn a_rust_extension_loads_in_process_and_serves_its_hooks() {
    let extension = DynamicExtension::open(fixture()).expect("the fixture must load");

    assert_eq!(extension.name(), "shout");
    assert_eq!(
        extension.hooks(),
        &[HOOK_INPUT.to_string(), HOOK_OUTPUT.to_string()]
    );

    let mut pipeline = Pipeline::new();
    pipeline
        .register(Arc::new(extension))
        .expect("registration must succeed");

    let run = pipeline
        .run("hello brave world")
        .expect("the run must succeed");

    assert_eq!(run.analysis.text, "[shouted] HELLO BRAVE WORLD");
    assert_eq!(
        run.trace,
        vec!["shout: pipeline.input", "shout: pipeline.output"]
    );
    // Both hooks contributed: the input one changed the text, the output one
    // tagged the analysis. The `[shouted]` marker counts as a fourth word.
    assert_eq!(run.analysis.words, 4);
}

#[test]
fn the_same_library_can_be_loaded_twice_without_sharing_state() {
    // Regression guard: instances used to be process-wide statics inside the
    // library, so unloading one left the other pointing at freed memory.
    let first = DynamicExtension::open(fixture()).expect("first load");
    let second = DynamicExtension::open(fixture()).expect("second load");

    assert_eq!(
        first.call(HOOK_INPUT, &json!({"text": "a"})).unwrap(),
        json!({"text": "[shouted] A"})
    );
    drop(first);
    assert_eq!(
        second.call(HOOK_INPUT, &json!({"text": "b"})).unwrap(),
        json!({"text": "[shouted] B"})
    );
}

#[test]
fn the_description_crosses_the_boundary_as_json() {
    let extension = DynamicExtension::open(fixture()).expect("the fixture must load");
    let payload = extension
        .call(HOOK_INPUT, &json!({ "text": "abc" }))
        .expect("the hook must answer");
    assert_eq!(payload, json!({ "text": "[shouted] ABC" }));
}

/// Some shared library that certainly is not a blablablars extension.
fn system_library() -> Option<&'static str> {
    if cfg!(target_os = "linux") {
        Some("libm.so.6")
    } else if cfg!(target_os = "macos") {
        Some("/usr/lib/libSystem.B.dylib")
    } else if cfg!(target_os = "windows") {
        Some("kernel32.dll")
    } else {
        None
    }
}

#[test]
fn a_library_without_the_entry_point_is_rejected() {
    let Some(candidate) = system_library() else {
        return;
    };
    match DynamicExtension::open(candidate) {
        Ok(_) => panic!("`{candidate}` must not load as a plugin"),
        Err(blablablars_core::CoreError::Plugin(message)) => {
            if message.contains("could not load the shared library") {
                // Not present on this system: nothing to assert.
                return;
            }
            assert!(
                message.contains("not a blablablars extension"),
                "unexpected message: {message}"
            );
        }
        Err(other) => panic!("unexpected error: {other}"),
    }
}
