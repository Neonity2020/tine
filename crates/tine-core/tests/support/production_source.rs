//! The one scanner that answers "what source does a shipped Tine binary
//! actually compile?" — shared by every source-census guard.
//!
//! It exists as a module rather than as a copy per test because a second
//! scanner is a second answer to one question (I-12). That is not
//! hypothetical here: a first attempt at the print-census re-anchoring tool
//! re-implemented this walk and disagreed with it on `src/bin/*` and on
//! `#[path]`-included `*_tests.rs` modules, reporting ~17 print sites that
//! do not exist in a shipped binary.
//!
//! What "compiled production source" means, precisely:
//!   * `crates/*/src` and `src-tauri/src`;
//!   * NOT `src/bin/**` — standalone CLI binaries own their own terminal
//!     output and are not part of the application;
//!   * NOT a `*_tests.rs` file that a sibling pulls in under `#[cfg(test)]`;
//!   * NOT a trailing `#[cfg(test)] mod tests { .. }`, nor any other
//!     `#[cfg(test)]` or `#[cfg(all(test, ..))]` region — those are blanked in
//!     place, so line numbers stay true to the file on disk. `#[cfg(not(test))]`
//!     and `#[cfg(any(test, ..))]` ARE production: the latter ships on at least
//!     one target, so it stays in the census.
//!
//! Exemplar consumers: `content_out_of_logs.rs` (I-5 print sites) and
//! `process_termination_sites.rs` (I-10 abort/exit sites).

#![allow(dead_code)]

use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("tine-core lives at crates/tine-core")
        .to_path_buf()
}

pub fn collect_rs_files(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            if path
                .strip_prefix(root)
                .unwrap()
                .components()
                .any(|part| part.as_os_str() == "bin")
            {
                continue;
            }
            collect_rs_files(root, &path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

pub fn test_only_include(path: &Path) -> bool {
    let Some(file) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !file.ends_with("_tests.rs") {
        return false;
    }
    let stem = file.trim_end_matches(".rs");
    let escaped_file = regex::escape(file);
    let escaped_stem = regex::escape(stem);
    let include = Regex::new(&format!(
        r#"(?m)^#\[cfg\(test\)\]\s*\n(?:#\[path\s*=\s*"{escaped_file}"\]\s*\nmod\s+\w+;|mod\s+{escaped_stem};)"#
    ))
    .unwrap();
    fs::read_dir(path.parent().unwrap()).unwrap().any(|entry| {
        let sibling = entry.unwrap().path();
        sibling != path
            && sibling
                .extension()
                .is_some_and(|extension| extension == "rs")
            && include.is_match(&fs::read_to_string(sibling).unwrap())
    })
}

pub fn compiled_source(path: &Path) -> String {
    if test_only_include(path) {
        return String::new();
    }
    let source = fs::read_to_string(path).unwrap();
    let trailing_tests = Regex::new(r"(?m)^#\[cfg\(test\)\]\s*\nmod\s+tests\s*\{").unwrap();
    let source = trailing_tests
        .find(&source)
        .map_or(source.clone(), |found| source[..found.start()].to_string());
    erase_cfg_test_regions(source)
}

/// Byte offset just past the `]` that closes the attribute starting at `start`.
fn attribute_end(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut depth = 0_usize;
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return cursor + 1;
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    source.len()
}

pub fn erase_cfg_test_regions(mut source: String) -> String {
    // `#[cfg(test)]` and `#[cfg(all(test, ..))]` are both compiled ONLY under
    // `cfg(test)`; `#[cfg(any(test, ..))]` and `#[cfg(not(test))]` are not, and
    // must stay. Missing the `all(..)` shape was not hypothetical: it left
    // `src-tauri/src/lib.rs`'s `#[cfg(all(test, desktop))] mod multi_window_tests`
    // and `src-tauri/src/data_home.rs`'s test module inside "production source"
    // for every census built on this walker.
    let marker = Regex::new(r"#\[cfg\((?:test\)\]|all\(\s*test\s*[,)])").unwrap();
    let mut search_from = 0;
    while let Some(found) = marker.find(&source[search_from..]) {
        let start = search_from + found.start();
        let after = attribute_end(&source, start);
        let next_brace = source[after..].find('{').map(|offset| after + offset);
        let next_semicolon = source[after..].find(';').map(|offset| after + offset);
        let end = match (next_brace, next_semicolon) {
            (Some(brace), Some(semicolon)) if semicolon < brace => semicolon + 1,
            (None, Some(semicolon)) => semicolon + 1,
            (Some(brace), _) => {
                let bytes = source.as_bytes();
                let mut depth = 1_usize;
                let mut cursor = brace + 1;
                while cursor < bytes.len() && depth > 0 {
                    match bytes[cursor] {
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        _ => {}
                    }
                    cursor += 1;
                }
                cursor
            }
            (None, None) => source.len(),
        };
        let replacement = source.as_bytes()[start..end]
            .iter()
            .map(|byte| if *byte == b'\n' { b'\n' } else { b' ' })
            .collect::<Vec<_>>();
        source.replace_range(start..end, std::str::from_utf8(&replacement).unwrap());
        search_from = end;
    }
    source
}

/// Every `.rs` file that a shipped Tine binary compiles, in a stable order.
pub fn production_source_files() -> Vec<PathBuf> {
    let root = repo_root();
    let mut files = Vec::new();
    for entry in fs::read_dir(root.join("crates")).unwrap() {
        let source_root = entry.unwrap().path().join("src");
        if source_root.is_dir() {
            collect_rs_files(&root, &source_root, &mut files);
        }
    }
    collect_rs_files(&root, &root.join("src-tauri/src"), &mut files);
    files.sort();
    files
}

/// Repository-relative, forward-slashed path for a scanned file.
pub fn relative_path(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

/// 1-indexed line number of a byte offset within `source`.
pub fn line_of(source: &str, offset: usize) -> usize {
    source[..offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}
