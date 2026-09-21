use std::fs;
use std::path::{Path, PathBuf};

#[track_caller]
pub(crate) fn remove_dir_all(path: impl AsRef<Path>) {
    let path = path.as_ref();
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        #[cfg(windows)]
        Err(error)
            if error.raw_os_error()
                == Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32) => {}
        Err(error) => panic!(
            "failed to remove test directory {}: {error}",
            path.display()
        ),
    }
}

/// Every file of the `model` module as `(path relative to src/, text)`:
/// `model.rs` first, then each seam file under `model/` in path order.
///
/// K3 (2026-09-15) cut `impl Graph` into child modules. A source guard that
/// reads `model.rs` alone passes vacuously for code that moved out of it, so
/// every in-crate guard over the model module reads it through here (I-11).
/// `src/rustModelSource.test-helpers.ts` and the integration tests'
/// `production_source::model_module_files` answer the same question.
pub(crate) fn model_module_files() -> Vec<(String, String)> {
    fn seams(directory: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).expect("model/ is readable") {
            let path = entry.expect("model/ entry is readable").path();
            if path.is_dir() {
                seams(&path, files);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    seams(&src.join("model"), &mut files);
    files.sort();
    files.insert(0, src.join("model.rs"));
    files
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = fs::read_to_string(&path).unwrap();
            (relative, text)
        })
        .collect()
}

/// The whole `model` module's source: [`model_module_files`] concatenated.
pub(crate) fn model_module_source() -> String {
    model_module_files()
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every production file in a split Rust module, root first and then child
/// seams in path order. Test-only seam files are excluded; callers can strip
/// inline `#[cfg(test)]` modules with their existing source masker.
pub(crate) fn rust_module_production_files(module_root: &str) -> Vec<PathBuf> {
    fn seams(directory: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).expect("Rust module directory is readable") {
            let path = entry.expect("Rust module entry is readable").path();
            if path.is_dir() {
                seams(&path, files);
            } else if path.extension().is_some_and(|extension| extension == "rs")
                && !path.to_string_lossy().ends_with("_tests.rs")
            {
                files.push(path);
            }
        }
    }

    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let root = src.join(module_root);
    let directory = root.with_extension("");
    let mut files = Vec::new();
    if directory.is_dir() {
        seams(&directory, &mut files);
    }
    files.sort();
    files.insert(0, root);
    files
}

/// A split module's production files, read and joined (I-11).
pub(crate) fn rust_module_production_source(module_root: &str) -> String {
    rust_module_production_files(module_root)
        .into_iter()
        .map(|path| fs::read_to_string(path).expect("Rust module file is readable"))
        .collect::<Vec<_>>()
        .join("\n")
}
