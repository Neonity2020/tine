//! Shared source guards used only by native tests.

pub(crate) struct AuditedWriteAllowance<'a> {
    pub(crate) source_line: &'a str,
    pub(crate) expected_count: usize,
}

pub(crate) fn assert_production_region_uses_named_audited_writes(
    source: &str,
    exemplar: &str,
    allowances: &[AuditedWriteAllowance<'_>],
) {
    let production = source
        .split_once("\n#[cfg(test)]")
        .map(|(production, _)| production)
        .expect("source has a top-level #[cfg(test)] boundary");
    assert!(
        production.contains(exemplar),
        "I-1/I-2 guard is vacuous: named audited exemplar `{exemplar}` is absent"
    );

    for allowance in allowances {
        let actual = production
            .lines()
            .filter(|line| line.trim() == allowance.source_line)
            .count();
        assert_eq!(
            actual, allowance.expected_count,
            "I-1/I-2 audited raw-write allowance drifted for `{}`",
            allowance.source_line
        );
    }

    let mut violations = Vec::new();
    for (index, line) in production.lines().enumerate() {
        let trimmed = line.trim();
        let allowed = allowances
            .iter()
            .any(|allowance| trimmed == allowance.source_line);
        if allowed {
            continue;
        }
        let imports_grouped_fs = trimmed
            .strip_prefix("use std::{")
            .and_then(|items| items.split_once("};").map(|(items, _)| items))
            .is_some_and(|items| {
                items.split(',').any(|item| {
                    let item = item.trim();
                    item == "fs" || item.starts_with("fs as ")
                })
            });
        let imports_fs = trimmed.starts_with("use std::fs")
            || trimmed.contains(" use std::fs")
            || imports_grouped_fs;
        let calls_raw_fs = [
            "fs::write(",
            "fs::rename(",
            "fs::remove_",
            "fs::copy(",
            "fs::create_dir",
            "File::create(",
            "OpenOptions",
        ]
        .iter()
        .any(|needle| trimmed.contains(needle));
        if imports_fs || calls_raw_fs {
            violations.push(format!("{}: {trimmed}", index + 1));
        }
    }

    assert!(
        violations.is_empty(),
        "I-1/I-2 require production durable-state writes to use the named audited `{exemplar}` path; raw or import-aliased filesystem writes found:\n{}",
        violations.join("\n")
    );
}

#[test]
fn audited_write_guard_rejects_import_alias_and_constructor_evasions() {
    for evasion in [
        "use std::fs; fn write() { fs::write(\"x\", b\"x\").unwrap(); }",
        "fn write() { std::fs::copy(\"x\", \"y\").unwrap(); }",
        "fn write() { std::fs::File::create(\"x\").unwrap(); }",
        "fn write() { std::fs::OpenOptions::new(); }",
        "fn write() { std::fs::create_dir_all(\"x\").unwrap(); }",
        "use std::{fs, io}; fn write() { let _ = io::empty(); fs::write(\"x\", b\"x\").unwrap(); }",
    ] {
        let source =
            format!("fn named_audited_path() {{}}\n{evasion}\n#[cfg(test)]\nmod tests {{}}");
        let rejected = std::panic::catch_unwind(|| {
            assert_production_region_uses_named_audited_writes(&source, "named_audited_path", &[]);
        });
        assert!(rejected.is_err(), "guard accepted evasion: {evasion}");
    }
}

#[test]
fn a_test_child_file_folds_in_as_test_code() {
    // `commands/capture_quick_switch_tests.rs` is declared
    // `#[cfg(test)] mod capture_quick_switch_tests;`: guards that strip test
    // items must not read its tests as production owners.
    let marker = "fn a_capture_query_on_a_retired_graph_is_answered_by_its_replacement";
    let whole = rust_module_source("commands.rs");
    assert!(
        whole.contains(marker),
        "the folded module lost its test child"
    );
    assert!(!without_cfg_test_items(&whole).contains(marker));
    assert!(!rust_module_production_source("commands.rs").contains(marker));
    assert!(without_cfg_test_items(&whole).contains("fn capture_quick_switch_for"));
}

/// Every `.rs` file of the module whose root file is `root`: the root first,
/// then everything below its sibling directory (`commands.rs` + `commands/**`),
/// in path order.
fn rust_module_paths(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn visit(directory: &std::path::Path, paths: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(directory).expect("module directory is readable") {
            let path = entry.expect("module entry is readable").path();
            if path.is_dir() {
                visit(&path, paths);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                paths.push(path);
            }
        }
    }

    let mut paths = Vec::new();
    let module_directory = root.with_extension("");
    if module_directory.is_dir() {
        visit(&module_directory, &mut paths);
    }
    paths.sort();
    paths.insert(0, root.to_path_buf());
    paths
}

/// A Rust module read as one logical source. Source guards read through this,
/// so code a seam cut moves into a child module stays visible to them (I-11;
/// the exemplar is tine-core's `projection_producer_census::production_rust`).
/// `src/rustModelSourceGuard.test.ts` fails on a guard that reads a split
/// module's root file alone.
pub(crate) fn rust_module_source_at(root: &std::path::Path) -> String {
    rust_module_paths(root)
        .into_iter()
        .map(|path| {
            let source = std::fs::read_to_string(&path).expect("module source is readable");
            if !is_cfg_test_child(&path) {
                return source;
            }
            // Fold a test-only child file in as the `#[cfg(test)]` block it
            // is, indented so its own closing braces cannot end the block
            // early: `without_cfg_test_items` then removes it like an inline
            // test module.
            let stem = path.file_stem().unwrap().to_string_lossy();
            let body = source
                .split('\n')
                .map(|line| {
                    if line.is_empty() {
                        String::new()
                    } else {
                        format!("    {line}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!("#[cfg(test)]\nmod {stem} {{\n{body}\n}}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether `path` is a child module file its parent declares as
/// `#[cfg(test)] mod name;` — test code a seam cut moved out of the parent.
fn is_cfg_test_child(path: &std::path::Path) -> bool {
    let (Some(directory), Some(stem)) = (path.parent(), path.file_stem()) else {
        return false;
    };
    let declaration = format!("mod {};", stem.to_string_lossy());
    [directory.with_extension("rs"), directory.join("mod.rs")]
        .iter()
        .filter_map(|parent| std::fs::read_to_string(parent).ok())
        .any(|parent| {
            let lines: Vec<&str> = parent.lines().map(str::trim).collect();
            lines
                .windows(2)
                .any(|pair| pair[0] == "#[cfg(test)]" && pair[1] == declaration)
        })
}

fn source_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// [`rust_module_source_at`] for a module file under `src-tauri/src`.
pub(crate) fn rust_module_source(file: &str) -> String {
    rust_module_source_at(&source_dir().join(file))
}

/// [`rust_module_source`] with each file's `#[cfg(test)]` items removed: the
/// module's production code, for guards whose own test data would otherwise
/// match.
pub(crate) fn rust_module_production_source(file: &str) -> String {
    rust_module_paths(&source_dir().join(file))
        .into_iter()
        .filter(|path| !is_cfg_test_child(path))
        .map(|path| {
            without_cfg_test_items(
                &std::fs::read_to_string(path).expect("module source is readable"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `source` without its `#[cfg(test)]` items. A one-line item (`use …;`,
/// `mod tests;`) ends at its line. A block item ends at the next line holding
/// only its indentation and `}`, which is where rustfmt closes it. The
/// TypeScript twin is `productionRust` in `src/queryMacroNames.test.ts`.
pub(crate) fn without_cfg_test_items(source: &str) -> String {
    let lines: Vec<&str> = source.split('\n').collect();
    let mut kept = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() != "#[cfg(test)]" {
            kept.push(lines[i]);
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < lines.len() && lines[j].trim_start().starts_with("#[") {
            j += 1;
        }
        if j < lines.len() && lines[j].trim_end().ends_with('{') {
            let indent = &lines[j][..lines[j].len() - lines[j].trim_start().len()];
            let close = format!("{indent}}}");
            while j < lines.len() && lines[j] != close {
                j += 1;
            }
        } else {
            while j < lines.len() && !lines[j].trim_end().ends_with(';') {
                j += 1;
            }
        }
        i = j + 1;
    }
    kept.join("\n")
}

/// Every top-level module under `src-tauri/src`, child files folded in.
pub(crate) fn rust_module_sources() -> Vec<(String, String)> {
    let mut roots = std::fs::read_dir(source_dir())
        .expect("src-tauri/src must be readable")
        .map(|entry| entry.expect("source entry is readable").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .collect::<Vec<_>>();
    roots.sort();
    roots
        .into_iter()
        .map(|path| {
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            (file, rust_module_source_at(&path))
        })
        .collect()
}
