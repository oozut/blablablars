//! Finding model files in the user's model folder.
//!
//! The folder is the interface: anything the user drops in as a `*.bin` ggml
//! model is listed by `blablablars models`, and the config names one of them.
//! Names are then resolved leniently, because filenames in the wild carry a
//! `ggml-` prefix and a quantisation suffix (`ggml-large-v3-turbo-q5_0.bin`)
//! while the config says `whisper-large-v3-turbo`.

use std::path::{Path, PathBuf};

use crate::error::CoreError;

/// What sort of model file (or folder) this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// A ggml `*.bin`: whisper.cpp's Whisper models and Parakeet TDT models.
    Ggml,
    /// A folder of ONNX files for a Whisper model
    /// (`*-encoder.onnx`, `*-decoder.onnx`, `*tokens.txt`).
    WhisperOnnx,
    /// A folder of ONNX files for a transducer (`tokens.txt` plus an encoder,
    /// a decoder and a joiner): NVIDIA Parakeet, Zipformer, Conformer.
    TransducerOnnx,
    /// Neither: a name a command backend may still understand.
    Unknown,
}

impl Layout {
    /// How the layout is described in `blablablars models` and `engines`.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Ggml => "ggml *.bin",
            Self::WhisperOnnx => "ONNX folder (Whisper)",
            Self::TransducerOnnx => "ONNX folder (transducer)",
            Self::Unknown => "unrecognised",
        }
    }
}

impl super::config::TranscriberKind {
    /// Whether this backend can run a model of that layout.
    pub fn reads(self, layout: Layout) -> bool {
        match self {
            Self::WhisperCli | Self::Parakeet => layout == Layout::Ggml,
            Self::SherpaOnnx => matches!(layout, Layout::WhisperOnnx | Layout::TransducerOnnx),
            // A command backend decides for itself what it can read.
            Self::Command => true,
        }
    }
}

/// The ONNX files one model folder holds, named the way sherpa-onnx wants them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnnxFiles {
    pub tokens: PathBuf,
    pub encoder: PathBuf,
    pub decoder: Option<PathBuf>,
    pub joiner: Option<PathBuf>,
}

/// One model file or model folder the user has installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// Normalised name, e.g. `large-v3-turbo` or `small.en`.
    pub name: String,
    /// File or folder name as it is on disk, e.g. `ggml-small.en.bin`.
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

    /// What sort of model this is, decided from the file or folder.
    pub fn layout(&self) -> Layout {
        layout_of(&self.path)
    }

    /// The files a sherpa-onnx model folder holds, when it is one.
    pub fn onnx_files(&self) -> Option<OnnxFiles> {
        onnx_files(&self.path)
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

/// Lowercase, drop the distribution prefixes and a `.bin` suffix, collapse
/// separators.
pub fn normalise(name: &str) -> String {
    let mut stem = name;
    if let Some(stripped) = stem.strip_suffix(".bin") {
        stem = stripped;
    }
    for prefix in ["ggml-", "sherpa-onnx-"] {
        if let Some(stripped) = stem.strip_prefix(prefix) {
            stem = stripped;
        }
    }
    stem.trim()
        .trim_start_matches("whisper-")
        .to_lowercase()
        .replace(['_', ' '], "-")
}

/// Every model in `directory`: ggml `*.bin` files and ONNX model folders,
/// sorted by name.
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
        let is_dir = path.is_dir();
        if !is_dir && !file_name.to_ascii_lowercase().ends_with(".bin") {
            continue;
        }
        // A folder only counts when it actually holds a model, so a stray
        // `models/notes/` does not show up in the list.
        if is_dir && onnx_files(&path).is_none() {
            continue;
        }
        let bytes = if is_dir {
            directory_bytes(&path)
        } else {
            entry.metadata().map(|meta| meta.len()).unwrap_or(0)
        };
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

/// Total size of the files in a model folder.
fn directory_bytes(directory: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum()
}

/// `Ggml` for a `*.bin` file, the ONNX layout for a folder, `Unknown` otherwise.
pub fn layout_of(path: &Path) -> Layout {
    if path.is_file() {
        return if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("bin"))
        {
            Layout::Ggml
        } else {
            Layout::Unknown
        };
    }
    match onnx_files(path) {
        Some(files) if files.joiner.is_some() => Layout::TransducerOnnx,
        Some(_) => Layout::WhisperOnnx,
        None => Layout::Unknown,
    }
}

/// The ONNX files in a model folder, chosen the way sherpa-onnx expects them.
///
/// The naming conventions in the wild are loose (`encoder-epoch-99-avg-1.onnx`,
/// `encoder.int8.onnx`, `tiny.en-encoder.int8.onnx`), so this matches on the
/// role in the name and prefers a full-precision file over a quantised one when
/// both are present.
pub fn onnx_files(path: &Path) -> Option<OnnxFiles> {
    if !path.is_dir() {
        return None;
    }
    let mut onnx = Vec::new();
    let mut tokens = Vec::new();
    for entry in std::fs::read_dir(path).ok()?.flatten() {
        let file = entry.path();
        let Some(name) = file.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".onnx") {
            onnx.push(file);
        } else if lower.contains("tokens") && (lower.ends_with(".txt") || lower.ends_with(".json"))
        {
            tokens.push(file);
        }
    }
    onnx.sort();
    tokens.sort();

    let pick = |role: &str| -> Option<PathBuf> {
        let mut candidates: Vec<&PathBuf> = onnx
            .iter()
            .filter(|file| {
                file.file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.to_ascii_lowercase().contains(role))
            })
            .collect();
        if candidates.is_empty() {
            return None;
        }
        // A deterministic choice: full precision first, then the shortest name.
        candidates.sort_by_key(|file| {
            let name = file.to_string_lossy().to_ascii_lowercase();
            let quantised = name.contains("int8") || name.contains("q8") || name.contains("fp16");
            (quantised, name.len(), name)
        });
        Some(candidates[0].clone())
    };

    let encoder = pick("encoder")?;
    Some(OnnxFiles {
        tokens: tokens.first().cloned()?,
        encoder,
        decoder: pick("decoder"),
        joiner: pick("joiner"),
    })
}

/// Find the model the config asks for.
///
/// Matching is exact first, then prefix, then substring, so
/// `whisper-large-v3-turbo` finds `ggml-large-v3-turbo-q5_0.bin` while
/// `small.en` never picks `small.en-tdrz` over an exact `small.en`.
pub fn resolve(directory: &Path, wanted: &str) -> Result<Model, CoreError> {
    // An explicit path to a file or a model folder always wins over the folder.
    let as_path = Path::new(wanted);
    if as_path.is_file() || (as_path.is_dir() && onnx_files(as_path).is_some()) {
        let file_name = as_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(wanted)
            .to_string();
        let bytes = if as_path.is_dir() {
            directory_bytes(as_path)
        } else {
            std::fs::metadata(as_path)
                .map(|meta| meta.len())
                .unwrap_or(0)
        };
        return Ok(Model {
            name: normalise(&file_name),
            file_name,
            path: as_path.to_path_buf(),
            bytes,
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

/// The command that fetches a model, so the error message is actionable.
///
/// Three families are in use and each has its own download command: ggml
/// Whisper models from `ggerganov/whisper.cpp`, ggml Parakeet models from
/// `ggml-org/parakeet-GGUF`, and ONNX models (NVIDIA Parakeet included) as
/// folders from the sherpa-onnx collections.
pub fn download_hint(name: &str, directory: &Path) -> String {
    let name = normalise(name);
    let lower = name.to_ascii_lowercase();

    if lower.starts_with("nemo-")
        || lower.starts_with("sherpa-")
        || lower.starts_with("zipformer")
        || lower.starts_with("paraformer")
        || lower.starts_with("sense-voice")
        || lower.contains("onnx")
        || lower.contains("int8")
        || lower.ends_with("-int8")
    {
        return format!(
            "Download one with:\n  hf download csukuangfj/sherpa-onnx-{name} --local-dir {}\n\
             (an ONNX model is a folder, not a file)",
            directory.display()
        );
    }

    if lower.starts_with("parakeet") || lower.contains("parakeet-tdt") {
        return format!(
            "Download one with:\n  curl -L -o {}/ggml-{name}-f16.bin \\\n    https://huggingface.co/ggml-org/parakeet-GGUF/resolve/main/ggml-{name}-f16.bin",
            directory.display()
        );
    }

    // The published files never carry the `whisper-` prefix or the extension, so
    // `whisper-large-v3-turbo` has to become `ggml-large-v3-turbo.bin`.
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
    /// One ONNX model folder, shaped the way sherpa-onnx publishes them.
    fn onnx_model(directory: &Path, joiner: bool) -> PathBuf {
        let model = directory.join("sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8");
        std::fs::create_dir_all(&model).unwrap();
        touch(&model, "tokens.txt", 4);
        touch(&model, "encoder.int8.onnx", 100);
        touch(&model, "encoder.onnx", 200);
        touch(&model, "decoder.int8.onnx", 50);
        touch(&model, "decoder.onnx", 90);
        if joiner {
            touch(&model, "joiner.int8.onnx", 10);
        }
        model
    }

    #[test]
    fn an_onnx_model_is_a_folder_and_is_discovered_like_a_file() {
        let directory = scratch("onnx-discover");
        touch(&directory, "ggml-small.bin", 10);
        onnx_model(&directory, true);
        touch(&directory, "notes.txt", 1);
        // A folder with no model inside is not a model.
        std::fs::create_dir_all(directory.join("empty")).unwrap();

        let models = discover(&directory).unwrap();
        let names: Vec<_> = models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            ["nemo-parakeet-tdt-0.6b-v2-int8", "small"],
            "the `sherpa-onnx-` prefix is noise, like `ggml-`"
        );
        assert_eq!(models[0].layout(), Layout::TransducerOnnx);
        assert_eq!(models[1].layout(), Layout::Ggml);
        // The folder's size is what a user wants to see, not zero.
        assert_eq!(models[0].bytes, 4 + 100 + 200 + 50 + 90 + 10);
    }

    #[test]
    fn a_whisper_onnx_folder_is_recognised() {
        let directory = scratch("onnx-whisper");
        let model = directory.join("sherpa-onnx-whisper-tiny.en");
        std::fs::create_dir_all(&model).unwrap();
        touch(&model, "tiny.en-tokens.txt", 4);
        touch(&model, "tiny.en-encoder.int8.onnx", 10);
        touch(&model, "tiny.en-decoder.int8.onnx", 10);

        let files = onnx_files(&model).unwrap();
        assert_eq!(files.tokens.file_name().unwrap(), "tiny.en-tokens.txt");
        assert_eq!(
            files.encoder.file_name().unwrap(),
            "tiny.en-encoder.int8.onnx"
        );
        assert!(files.joiner.is_none());
        assert_eq!(layout_of(&model), Layout::WhisperOnnx);
    }

    #[test]
    fn a_full_precision_weight_is_preferred_over_a_quantised_one() {
        let directory = scratch("onnx-precision");
        let model = onnx_model(&directory, true);
        let files = onnx_files(&model).unwrap();
        assert_eq!(files.encoder.file_name().unwrap(), "encoder.onnx");
        assert_eq!(files.decoder.unwrap().file_name().unwrap(), "decoder.onnx");
        // Only an int8 joiner exists, so that is the one used.
        assert_eq!(
            files.joiner.unwrap().file_name().unwrap(),
            "joiner.int8.onnx"
        );
    }

    #[test]
    fn a_model_named_on_the_command_line_can_be_a_folder() {
        let directory = scratch("onnx-resolve");
        let model = onnx_model(&directory, true);

        let resolved = resolve(Path::new("/nonexistent"), model.to_str().unwrap()).unwrap();
        assert_eq!(resolved.path, model);
        assert_eq!(resolved.name, "nemo-parakeet-tdt-0.6b-v2-int8");
        assert!(resolved.bytes > 400);

        let by_name = resolve(&directory, "nemo-parakeet-tdt-0.6b-v2-int8").unwrap();
        assert_eq!(by_name.path, model);
    }

    #[test]
    fn the_download_hint_names_the_right_collection() {
        let directory = Path::new("/models");
        let whisper = download_hint("large-v3-turbo", directory);
        assert!(whisper.contains("ggerganov/whisper.cpp"), "{whisper}");
        assert!(whisper.contains("ggml-large-v3-turbo.bin"), "{whisper}");

        let parakeet = download_hint("parakeet-tdt-0.6b-v3", directory);
        assert!(parakeet.contains("ggml-org/parakeet-GGUF"), "{parakeet}");
        assert!(
            parakeet.contains("ggml-parakeet-tdt-0.6b-v3-f16.bin"),
            "{parakeet}"
        );

        let onnx = download_hint("nemo-parakeet-tdt-0.6b-v2-int8", directory);
        assert!(onnx.contains("sherpa-onnx-nemo-parakeet"), "{onnx}");
        assert!(onnx.contains("folder"), "{onnx}");
    }
}
