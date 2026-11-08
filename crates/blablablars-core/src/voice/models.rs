//! Finding model files in the user's model folder.
//!
//! The folder is the interface: anything the user drops in as a `*.bin` ggml
//! model is listed by `blablablars models`, and the config names one of them.
//! Names are then resolved leniently, because filenames in the wild carry a
//! `ggml-` prefix and a quantisation suffix (`ggml-large-v3-turbo-q5_0.bin`)
//! while the config says `whisper-large-v3-turbo`.

use std::path::{Path, PathBuf};

use crate::error::CoreError;

/// One model file the user has installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// Normalised name, e.g. `large-v3-turbo` or `small.en`.
    pub name: String,
    /// File name as it is on disk, e.g. `ggml-small.en.bin`.
    pub file_name: String,
    pub path: PathBuf,
    pub bytes: u64,
}

impl Model {
    /// A name that could not be resolved to a file, passed through to a backend
    /// that does not need one.
    pub fn unresolved(name: &str) -> Self {
        Self {
            // Keep the name as the user wrote it: it is displayed, not matched.
            name: name.to_string(),
            file_name: name.to_string(),
            path: PathBuf::from(name),
            bytes: 0,
        }
    }

    /// Size in a form a human reads: `1.6 GB`.
    pub fn size_human(&self) -> String {
        let bytes = self.bytes as f64;
        let units = ["B", "KB", "MB", "GB", "TB"];
        let mut value = bytes;
        let mut unit = 0;
        while value >= 1024.0 && unit + 1 < units.len() {
            value /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            format!("{value:.0} {}", units[unit])
        } else {
            format!("{value:.1} {}", units[unit])
        }
    }
}

/// Lowercase, drop the `ggml-` prefix and a `.bin` suffix, collapse separators.
pub fn normalise(name: &str) -> String {
    let mut stem = name;
    if let Some(stripped) = stem.strip_suffix(".bin") {
        stem = stripped;
    }
    if let Some(stripped) = stem.strip_prefix("ggml-") {
        stem = stripped;
    }
    stem.trim()
        .trim_start_matches("whisper-")
        .to_lowercase()
        .replace(['_', ' '], "-")
}

/// Every `*.bin` file in `directory`, sorted by name.
pub fn discover(directory: &Path) -> Result<Vec<Model>, CoreError> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(CoreError::Voice(format!(
                "cannot read the model directory `{}`: {error}",
                directory.display()
            )))
        }
    };

    let mut models = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            CoreError::Voice(format!(
                "cannot read the model directory `{}`: {error}",
                directory.display()
            ))
        })?;
        let path = entry.path();
        let file_name = match path.file_name().and_then(|name| name.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };
        if !file_name.to_ascii_lowercase().ends_with(".bin") {
            continue;
        }
        let bytes = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        models.push(Model {
            name: normalise(&file_name),
            file_name,
            path,
            bytes,
        });
    }
    models.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(models)
}

/// Find the model the config asks for.
///
/// Matching is exact first, then prefix, then substring, so
/// `whisper-large-v3-turbo` finds `ggml-large-v3-turbo-q5_0.bin` while
/// `small.en` never picks `small.en-tdrz` over an exact `small.en`.
pub fn resolve(directory: &Path, wanted: &str) -> Result<Model, CoreError> {
    // An explicit path to a file always wins over the folder.
    let as_path = Path::new(wanted);
    if as_path.is_file() {
        let file_name = as_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(wanted)
            .to_string();
        return Ok(Model {
            name: normalise(&file_name),
            file_name,
            path: as_path.to_path_buf(),
            bytes: std::fs::metadata(as_path)
                .map(|meta| meta.len())
                .unwrap_or(0),
        });
    }

    let models = discover(directory)?;
    if models.is_empty() {
        return Err(CoreError::Voice(format!(
            "no models in `{}`.\n{}",
            directory.display(),
            download_hint(&normalise(wanted), directory)
        )));
    }

    let target = normalise(wanted);
    // `whisper-small` and `small` must name the same model.
    let target = target
        .strip_prefix("whisper-")
        .map(str::to_string)
        .unwrap_or(target);

    let exact: Vec<&Model> = models.iter().filter(|m| m.name == target).collect();
    let matches = if exact.len() == 1 {
        exact
    } else {
        models
            .iter()
            .filter(|m| m.name.starts_with(&format!("{target}-")) || m.name.starts_with(&target))
            .collect()
    };
    let matches = if matches.is_empty() {
        models.iter().filter(|m| m.name.contains(&target)).collect()
    } else {
        matches
    };

    match matches.as_slice() {
        [only] => Ok((*only).clone()),
        [] => Err(CoreError::Voice(format!(
            "no model matches `{wanted}` in `{}`. Available: {}.\n{}",
            directory.display(),
            models
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            download_hint(&target, directory)
        ))),
        several => Err(CoreError::Voice(format!(
            "`{wanted}` is ambiguous in `{}`: {}. Use one of those names.",
            directory.display(),
            several
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// The command that fetches a ggml model, so the error message is actionable.
pub fn download_hint(name: &str, directory: &Path) -> String {
    // The published files never carry the `whisper-` prefix or the extension, so
    // `whisper-large-v3-turbo` has to become `ggml-large-v3-turbo.bin`.
    let name = normalise(name);
    format!(
        "Download one with:\n  curl -L -o {}/ggml-{name}.bin \\\n    https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{name}.bin",
        directory.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("blb-models-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn touch(directory: &Path, name: &str, bytes: usize) {
        std::fs::write(directory.join(name), vec![0u8; bytes]).unwrap();
    }

    #[test]
    fn names_are_normalised_for_comparison() {
        assert_eq!(normalise("ggml-small.en.bin"), "small.en");
        assert_eq!(
            normalise("ggml-large-v3-turbo-q5_0.bin"),
            "large-v3-turbo-q5-0"
        );
        assert_eq!(normalise("whisper-large-v3-turbo"), "large-v3-turbo");
        assert_eq!(normalise("tiny"), "tiny");
    }

    #[test]
    fn discovery_lists_only_bin_files_sorted() {
        let directory = scratch("discover");
        touch(&directory, "ggml-small.bin", 10);
        touch(&directory, "ggml-base.bin", 20);
        touch(&directory, "notes.txt", 5);

        let models = discover(&directory).unwrap();
        let names: Vec<_> = models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["base", "small"]);
        assert_eq!(models[1].bytes, 10);
        assert_eq!(models[1].file_name, "ggml-small.bin");
    }

    #[test]
    fn a_missing_directory_is_an_empty_list_not_an_error() {
        let directory = std::env::temp_dir().join("blb-models-never-created");
        assert!(discover(&directory).unwrap().is_empty());
    }

    #[test]
    fn the_quantised_file_is_found_by_the_plain_name() {
        let directory = scratch("resolve-quant");
        touch(&directory, "ggml-large-v3-turbo-q5_0.bin", 10);
        touch(&directory, "ggml-base.bin", 10);

        let model = resolve(&directory, "whisper-large-v3-turbo").unwrap();
        assert_eq!(model.name, "large-v3-turbo-q5-0");
    }

    #[test]
    fn an_exact_match_wins_over_a_prefix() {
        let directory = scratch("resolve-exact");
        touch(&directory, "ggml-small.en.bin", 10);
        touch(&directory, "ggml-small.en-tdrz.bin", 10);

        let model = resolve(&directory, "small.en").unwrap();
        assert_eq!(model.file_name, "ggml-small.en.bin");
    }

    #[test]
    fn a_full_path_is_used_as_given() {
        let directory = scratch("resolve-path");
        touch(&directory, "ggml-tiny.bin", 10);
        let path = directory.join("ggml-tiny.bin");

        let model = resolve(Path::new("/nonexistent"), path.to_str().unwrap()).unwrap();
        assert_eq!(model.path, path);
        assert_eq!(model.name, "tiny");
    }

    #[test]
    fn a_missing_model_explains_how_to_get_it() {
        let directory = scratch("resolve-missing");
        touch(&directory, "ggml-base.bin", 10);

        let error = resolve(&directory, "large-v3-turbo").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("Available: base"), "{message}");
        assert!(message.contains("huggingface.co"), "{message}");
        assert!(message.contains("ggml-large-v3-turbo.bin"), "{message}");
    }

    #[test]
    fn an_empty_directory_says_where_it_looked() {
        let directory = scratch("resolve-empty");
        let error = resolve(&directory, "small").unwrap_err();
        assert!(error.to_string().contains(&directory.display().to_string()));
    }

    #[test]
    fn an_ambiguous_name_lists_the_candidates() {
        let directory = scratch("resolve-ambiguous");
        touch(&directory, "ggml-large-v3.bin", 10);
        touch(&directory, "ggml-large-v3-turbo.bin", 10);

        let error = resolve(&directory, "large").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("ambiguous"), "{message}");
        assert!(message.contains("large-v3-turbo"), "{message}");
    }

    #[test]
    fn sizes_are_human_readable() {
        let model = Model {
            name: "m".into(),
            file_name: "m.bin".into(),
            path: PathBuf::from("/m.bin"),
            bytes: 1_610_612_736,
        };
        assert_eq!(model.size_human(), "1.5 GB");

        let small = Model {
            bytes: 512,
            ..model
        };
        assert_eq!(small.size_human(), "512 B");
    }
}
