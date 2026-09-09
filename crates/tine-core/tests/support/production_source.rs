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
//!   * NOT a file that is reachable only through `#[cfg(test)]` module
//!     declarations, wherever in the repository they are written and however
//!     many hops away — a `*_tests.rs` file that itself declares further
//!     modules passes its test-only-ness on to them;
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

/// Where a `mod` declaration in `declarer` pulls its source from, and whether
/// the declaration itself is gated to `cfg(test)`.
struct ModuleDeclaration {
    target: PathBuf,
    cfg_test: bool,
}

/// Normalise away `.` and `..` without touching the filesystem: `#[path]` is
/// resolved against a directory that always exists, and a symlinked source tree
/// is not a shape this repository has.
fn normalize(path: PathBuf) -> PathBuf {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop();
            }
            other => parts.push(other.as_os_str().to_os_string()),
        }
    }
    parts.iter().collect()
}

/// Every `mod` declaration a file makes, resolved to the file it compiles.
///
/// Two resolution rules, and they are NOT the same directory — getting this
/// wrong is what let five test-only `eprintln!` sites into the print census:
///   * `#[path = "REL"]` is relative to the directory holding the DECLARING
///     file, so `src/query.rs` reaching `#[path = "query/oracle_gate1_tests.rs"]`
///     lands on `src/query/oracle_gate1_tests.rs`;
///   * a plain `mod name;` is relative to the declaring file's own module
///     directory — the parent for a `lib.rs`/`main.rs`/`mod.rs` root, otherwise
///     the sibling directory named after the file.
///
/// A visibility on the declaration (`pub(crate) mod gates_tests;`, so a sibling
/// test module can reuse the harness) says nothing about gating: the
/// `#[cfg(test)]` above it is what decides. Missing that once counted R3's 25
/// gate `eprintln!` sites as production print sites.
fn module_declarations(declarer: &Path, source: &str) -> Vec<ModuleDeclaration> {
    static DECLARATION: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let declaration = DECLARATION.get_or_init(|| {
        Regex::new(
            r#"(?m)^[ \t]*(?P<cfg>#\[cfg\((?:test\)|all\(\s*test\b)[^\n]*\]\s*)?(?:#\[path\s*=\s*"(?P<path>[^"]+)"\]\s*)?(?:pub(?:\([^)]*\))?\s+)?mod\s+(?P<name>\w+)\s*;"#,
        )
        .unwrap()
    });
    let Some(directory) = declarer.parent() else {
        return Vec::new();
    };
    let stem = declarer.file_stem().and_then(|stem| stem.to_str());
    let module_directory = match stem {
        Some("lib") | Some("main") | Some("mod") | None => directory.to_path_buf(),
        Some(stem) => directory.join(stem),
    };
    declaration
        .captures_iter(source)
        .map(|found| {
            let cfg_test = found.name("cfg").is_some();
            let target = match found.name("path") {
                Some(relative) => directory.join(relative.as_str()),
                None => {
                    let name = &found["name"];
                    let flat = module_directory.join(format!("{name}.rs"));
                    if flat.is_file() {
                        flat
                    } else {
                        module_directory.join(name).join("mod.rs")
                    }
                }
            };
            ModuleDeclaration {
                target: normalize(target),
                cfg_test,
            }
        })
        .collect()
}

/// Files that `candidates` can only reach through `#[cfg(test)]`.
///
/// Test-only-ness is TRANSITIVE, so this is a least fixed point rather than a
/// per-file question: seed with the directly gated includes, then keep
/// absorbing whatever only a test-only file declares, until nothing new
/// arrives. A production declarer keeps a file out of the set even when a test
/// module also pulls it in.
fn test_only_within(candidates: &[PathBuf]) -> std::collections::HashSet<PathBuf> {
    let mut declarers_of: std::collections::HashMap<PathBuf, Vec<(PathBuf, bool)>> =
        std::collections::HashMap::new();
    for file in candidates {
        let Ok(source) = fs::read_to_string(file) else {
            continue;
        };
        for declaration in module_declarations(file, &source) {
            declarers_of
                .entry(declaration.target)
                .or_default()
                .push((file.clone(), declaration.cfg_test));
        }
    }
    let mut test_only: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    loop {
        let mut grew = false;
        for (target, declarers) in &declarers_of {
            if test_only.contains(target) {
                continue;
            }
            if declarers
                .iter()
                .all(|(declarer, cfg_test)| *cfg_test || test_only.contains(declarer))
            {
                test_only.insert(target.clone());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    test_only
}

/// Every `.rs` file directly inside `directory`.
fn rs_files_beside(directory: Option<&Path>) -> Vec<PathBuf> {
    let Some(directory) = directory else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            path.extension()
                .is_some_and(|extension| extension == "rs")
                .then_some(path)
        })
        .collect()
}

/// Whether a shipped binary never compiles this file because every route to it
/// passes through `#[cfg(test)]`.
///
/// The search must be REPOSITORY-WIDE, because neither the declaration nor its
/// gating is visible from the included file or from its own directory.
/// `src/query.rs` declares its two oracle modules from the directory ABOVE
/// them, and `sync_runtime_tests.rs` declares `mod rebaselining_layout;` with
/// no `#[cfg(test)]` of its own — it does not need one, being itself reached
/// only under `cfg(test)`. A per-file sibling scan misses both shapes and
/// reports their `eprintln!` calls as production print sites, which is exactly
/// what it did.
pub fn test_only_include(path: &Path) -> bool {
    static REPOSITORY: std::sync::OnceLock<std::collections::HashSet<PathBuf>> =
        std::sync::OnceLock::new();
    let path = normalize(path.to_path_buf());
    if path.starts_with(repo_root()) {
        return REPOSITORY
            .get_or_init(|| test_only_within(&production_source_files()))
            .contains(&path);
    }
    // A file outside the scanned source roots: the scanner's own unit tests
    // fabricate one in a temp directory. Its only possible declarers are the
    // files beside it and one directory up, which is where both `mod` forms
    // can name it from.
    let mut candidates = rs_files_beside(path.parent());
    candidates.extend(rs_files_beside(path.parent().and_then(Path::parent)));
    test_only_within(&candidates).contains(&path)
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
