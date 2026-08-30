//! Executable guard for the projection-producer census.
//!
//! These tests deliberately inspect production source. They do not claim that
//! the grammar below can recognize every future filesystem API; they make the
//! currently audited grammar and architectural boundaries fail closed. A new
//! primitive, caller, native writer, process handoff, or user-selected writer
//! must update the census and this guard in the same change.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct ProductionFile {
    relative: String,
    code: String,
    compact: String,
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("tine-core remains under <repo>/crates/tine-core")
        .to_path_buf()
}

fn visit_rs(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("source directory is readable") {
        let path = entry.expect("source entry is readable").path();
        if path.is_dir() {
            visit_rs(&path, files);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            files.push(path);
        }
    }
}

fn module_directory(source_path: &Path) -> PathBuf {
    match source_path.file_name().and_then(|name| name.to_str()) {
        Some("lib.rs" | "mod.rs") => source_path.parent().unwrap().to_path_buf(),
        _ => source_path
            .parent()
            .unwrap()
            .join(source_path.file_stem().unwrap()),
    }
}

fn test_only_external_modules(source_path: &Path, source: &str) -> Vec<PathBuf> {
    let module_directory = module_directory(source_path);
    let mut modules = Vec::new();
    let mut suffixes = source.split("#[cfg(test)]").skip(1).collect::<Vec<_>>();
    let mut search = source;
    while let Some(offset) = search.find("#[cfg(all(test,") {
        let tail = &search[offset..];
        let Some(end) = tail.find(']') else {
            break;
        };
        suffixes.push(&tail[end + 1..]);
        search = &tail[end + 1..];
    }
    for suffix in suffixes {
        let declaration = suffix
            .trim_start()
            .lines()
            .next()
            .unwrap_or_default()
            .trim();
        let declaration = declaration
            .strip_prefix("pub(crate) ")
            .or_else(|| declaration.strip_prefix("pub "))
            .unwrap_or(declaration);
        let Some(name) = declaration
            .strip_prefix("mod ")
            .and_then(|name| name.strip_suffix(';'))
        else {
            continue;
        };
        for candidate in [
            module_directory.join(format!("{name}.rs")),
            module_directory.join(name).join("mod.rs"),
        ] {
            if candidate.exists() {
                modules.push(candidate);
            }
        }
    }
    modules
}

/// Replace comments and string/character literal bytes with spaces while
/// retaining byte offsets. This is a small lexer, not a Rust parser; offsets
/// matter because the test-item remover applies ranges to the original source.
fn code_mask(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"//") {
            let start = index;
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            out[start..index].fill(b' ');
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            let start = index;
            index += 2;
            let mut depth = 1_usize;
            while index < bytes.len() && depth > 0 {
                if bytes[index..].starts_with(b"/*") {
                    depth += 1;
                    index += 2;
                } else if bytes[index..].starts_with(b"*/") {
                    depth -= 1;
                    index += 2;
                } else {
                    index += 1;
                }
            }
            for byte in &mut out[start..index] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
            continue;
        }

        let raw_prefix = match bytes[index] {
            b'r' => Some(index + 1),
            b'b' if bytes.get(index + 1) == Some(&b'r') => Some(index + 2),
            _ => None,
        };
        if let Some(mut delimiter) = raw_prefix {
            let mut hashes = 0_usize;
            while bytes.get(delimiter) == Some(&b'#') {
                hashes += 1;
                delimiter += 1;
            }
            if bytes.get(delimiter) == Some(&b'"') {
                let start = index;
                index = delimiter + 1;
                while index < bytes.len() {
                    if bytes[index] == b'"'
                        && bytes
                            .get(index + 1..index + 1 + hashes)
                            .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
                    {
                        index += 1 + hashes;
                        break;
                    }
                    index += 1;
                }
                for byte in &mut out[start..index] {
                    if *byte != b'\n' {
                        *byte = b' ';
                    }
                }
                continue;
            }
        }

        let string_start = if bytes[index] == b'"' {
            Some(index)
        } else if bytes[index] == b'b' && bytes.get(index + 1) == Some(&b'"') {
            Some(index)
        } else {
            None
        };
        if let Some(start) = string_start {
            if bytes[index] == b'b' {
                index += 1;
            }
            index += 1;
            while index < bytes.len() {
                match bytes[index] {
                    b'\\' => index = (index + 2).min(bytes.len()),
                    b'"' => {
                        index += 1;
                        break;
                    }
                    _ => index += 1,
                }
            }
            for byte in &mut out[start..index] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
            continue;
        }

        let char_start = if bytes[index] == b'\'' {
            Some(index)
        } else if bytes[index] == b'b' && bytes.get(index + 1) == Some(&b'\'') {
            Some(index)
        } else {
            None
        };
        if let Some(start) = char_start {
            let mut cursor = index + usize::from(bytes[index] == b'b') + 1;
            if bytes.get(cursor) == Some(&b'\\') {
                cursor += 2;
            } else {
                cursor += 1;
            }
            if bytes.get(cursor) == Some(&b'\'') {
                index = cursor + 1;
                out[start..index].fill(b' ');
                continue;
            }
        }
        index += 1;
    }
    String::from_utf8(out).expect("mask preserves UTF-8 bytes")
}

fn matching_brace(mask: &str, open: usize) -> Option<usize> {
    let bytes = mask.as_bytes();
    let mut depth = 0_usize;
    for (offset, byte) in bytes[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset + 1);
                }
            }
            _ => {}
        }
    }
    None
}

fn without_test_items(source: &str) -> String {
    let mut production = source.to_owned();
    loop {
        let mask = code_mask(&production);
        let candidate = [
            "#[cfg(test)]",
            "#[cfg(all(test,",
            "#[test]",
            "#[tokio::test]",
        ]
        .into_iter()
        .filter_map(|attribute| mask.find(attribute))
        .min();
        let Some(attribute) = candidate else {
            break;
        };
        let attribute_len = mask[attribute..]
            .find(']')
            .map(|end| end + 1)
            .expect("test-only cfg attribute is complete");

        let mut item = attribute + attribute_len;
        loop {
            item += mask[item..]
                .find(|character: char| !character.is_whitespace())
                .unwrap_or(mask.len() - item);
            if !mask[item..].starts_with("#[") {
                break;
            }
            let Some(end) = mask[item..].find(']') else {
                break;
            };
            item += end + 1;
        }
        let tail = mask[item..].trim_start();
        item += mask[item..].len() - tail.len();
        let semicolon = mask[item..].find(';').map(|offset| item + offset + 1);
        let open = mask[item..].find('{').map(|offset| item + offset);
        let semicolon_item = ["use ", "type ", "const ", "static "]
            .iter()
            .any(|prefix| mask[item..].starts_with(prefix))
            || semicolon.is_some_and(|end| open.is_none_or(|brace| end < brace));
        let end = if semicolon_item {
            semicolon.expect("test-only declaration terminates with a semicolon")
        } else {
            matching_brace(&mask, open.expect("test-only item has a body"))
                .expect("test-only item has balanced braces")
        };
        production.replace_range(attribute..end, "");
    }
    production
}

fn production_rust() -> Vec<ProductionFile> {
    let repo = repository_root();
    let roots = [
        repo.join("crates/tine-core/src"),
        repo.join("src-tauri/src"),
    ];
    let mut paths = Vec::new();
    for root in &roots {
        visit_rs(root, &mut paths);
    }
    paths.sort();

    let test_only = paths
        .iter()
        .flat_map(|path| {
            let source = fs::read_to_string(path).expect("Rust source is readable");
            test_only_external_modules(path, &source)
        })
        .collect::<BTreeSet<_>>();

    paths
        .into_iter()
        .filter(|path| !test_only.contains(path))
        .filter(|path| {
            let relative = path.strip_prefix(&repo).unwrap().to_string_lossy();
            !relative.contains("/tests/")
                && !relative.contains("/benches/")
                && !relative.contains("/bin/")
                && !relative.ends_with("_tests.rs")
        })
        .map(|path| {
            let relative = path
                .strip_prefix(&repo)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let source = fs::read_to_string(&path).expect("Rust source is readable");
            let code = code_mask(&without_test_items(&source));
            let compact = code
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect();
            ProductionFile {
                relative,
                code,
                compact,
            }
        })
        .collect()
}

fn token_inventory(
    files: &[ProductionFile],
    tokens: &[(&'static str, &'static str)],
) -> Vec<(String, String, usize)> {
    let mut inventory = Vec::new();
    for file in files {
        for (name, token) in tokens {
            let count = file.compact.matches(token).count();
            if count != 0 {
                inventory.push((file.relative.clone(), (*name).to_owned(), count));
            }
        }
    }
    inventory.sort();
    inventory
}

fn call_count(files: &[ProductionFile], name: &str) -> usize {
    let call = format!("{name}(");
    let definition = format!("fn {name}(");
    files
        .iter()
        .map(|file| {
            identifier_occurrences(&file.code, &call) - file.code.matches(&definition).count()
        })
        .sum()
}

fn identifier_occurrences(source: &str, needle: &str) -> usize {
    let mut count = 0;
    let mut offset = 0;
    while let Some(relative) = source[offset..].find(needle) {
        let start = offset + relative;
        let boundary = start == 0
            || !source.as_bytes()[start - 1].is_ascii_alphanumeric()
                && source.as_bytes()[start - 1] != b'_';
        count += usize::from(boundary);
        offset = start + needle.len();
    }
    count
}

fn function_process_handoffs(files: &[ProductionFile], name: &str) -> usize {
    let definition = format!("fn{name}(");
    let mut total = 0;
    for file in files {
        let mut offset = 0;
        while let Some(relative) = file.compact[offset..].find(&definition) {
            let start = offset + relative;
            let open = start
                + file.compact[start..]
                    .find('{')
                    .expect("function definition has a body");
            let end = matching_brace(&file.compact, open).expect("function body is balanced");
            let body = &file.compact[open..end];
            total += body.matches(".spawn(").count() + body.matches(".status(").count();
            offset = end;
        }
    }
    total
}

#[test]
fn g_a_mutation_primitive_counts_are_pinned_per_file() {
    let actual = token_inventory(
        &production_rust(),
        &[
            ("cap.rename", ".rename("),
            ("cap.remove_file", ".remove_file("),
            ("cap.create_dir", ".create_dir("),
            ("cap.create_dir_all", ".create_dir_all("),
            ("cap.hard_link", ".hard_link("),
            ("cap.remove_dir", ".remove_dir("),
            ("cap.remove_dir_all", ".remove_dir_all("),
            ("fs.rename", "fs::rename("),
            ("fs.remove_file", "fs::remove_file("),
            ("fs.create_dir", "fs::create_dir("),
            ("fs.create_dir_all", "fs::create_dir_all("),
            ("fs.hard_link", "fs::hard_link("),
            ("fs.remove_dir", "fs::remove_dir("),
            ("fs.remove_dir_all", "fs::remove_dir_all("),
            ("fs.write", "fs::write("),
            ("fs.copy", "fs::copy("),
            ("libc.renameat", "libc::renameat("),
            ("libc.unlinkat", "libc::unlinkat("),
            ("libc.mkdirat", "libc::mkdirat("),
            ("libc.linkat", "libc::linkat("),
            ("libc.openat.create", "libc::O_CREAT"),
            ("libc.renameat2", "libc::SYS_renameat2"),
            ("open.create", ".create(true)"),
            ("open.create_new", ".create_new(true)"),
            ("open.truncate", ".truncate(true)"),
            ("file.create", "File::create("),
            ("file.set_len", ".set_len("),
            ("windows.MoveFileW", "MoveFileW("),
            ("windows.NtSetInformationFile", "NtSetInformationFile("),
            (
                "windows.SetFileInformationByHandle",
                "SetFileInformationByHandle(",
            ),
        ],
    );
    let expected = [
        (
            "crates/tine-core/src/concord_ledger.rs",
            "fs.create_dir_all",
            1,
        ),
        (
            "crates/tine-core/src/concord_ledger.rs",
            "fs.remove_file",
            4,
        ),
        ("crates/tine-core/src/concord_ledger.rs", "fs.rename", 1),
        ("crates/tine-core/src/concord_ledger.rs", "fs.write", 1),
        (
            "crates/tine-core/src/direct_projection.rs",
            "fs.create_dir_all",
            1,
        ),
        (
            "crates/tine-core/src/direct_projection.rs",
            "fs.remove_file",
            1,
        ),
        (
            "crates/tine-core/src/direct_projection.rs",
            "open.create",
            1,
        ),
        (
            "crates/tine-core/src/fast_commit.rs",
            "fs.create_dir_all",
            1,
        ),
        (
            "crates/tine-core/src/graph_name_folding.rs",
            "fs.create_dir_all",
            1,
        ),
        (
            "crates/tine-core/src/graph_name_folding.rs",
            "fs.remove_dir_all",
            1,
        ),
        (
            "crates/tine-core/src/graph_name_folding.rs",
            "fs.remove_file",
            2,
        ),
        ("crates/tine-core/src/graph_name_folding.rs", "fs.write", 2),
        (
            "crates/tine-core/src/managed_storage_journey.rs",
            "file.create",
            2,
        ),
        (
            "crates/tine-core/src/managed_storage_journey.rs",
            "fs.create_dir_all",
            4,
        ),
        ("crates/tine-core/src/model.rs", "cap.create_dir", 1),
        ("crates/tine-core/src/model.rs", "cap.remove_file", 23),
        ("crates/tine-core/src/model.rs", "cap.rename", 2),
        ("crates/tine-core/src/model.rs", "fs.create_dir", 8),
        ("crates/tine-core/src/model.rs", "fs.create_dir_all", 15),
        ("crates/tine-core/src/model.rs", "fs.remove_dir_all", 2),
        ("crates/tine-core/src/model.rs", "fs.remove_file", 16),
        ("crates/tine-core/src/model.rs", "fs.rename", 3),
        ("crates/tine-core/src/model.rs", "libc.renameat2", 3),
        ("crates/tine-core/src/model.rs", "open.create_new", 16),
        ("crates/tine-core/src/model.rs", "windows.MoveFileW", 1),
        (
            "crates/tine-core/src/model.rs",
            "windows.NtSetInformationFile",
            1,
        ),
        ("crates/tine-core/src/onboarding.rs", "fs.create_dir_all", 4),
        (
            "crates/tine-core/src/oplog/enrollment.rs",
            "cap.create_dir",
            1,
        ),
        (
            "crates/tine-core/src/oplog/enrollment.rs",
            "cap.hard_link",
            1,
        ),
        (
            "crates/tine-core/src/oplog/enrollment.rs",
            "cap.remove_file",
            6,
        ),
        ("crates/tine-core/src/oplog/enrollment.rs", "cap.rename", 1),
        (
            "crates/tine-core/src/oplog/enrollment.rs",
            "fs.create_dir_all",
            1,
        ),
        (
            "crates/tine-core/src/oplog/enrollment.rs",
            "libc.openat.create",
            2,
        ),
        (
            "crates/tine-core/src/oplog/enrollment.rs",
            "libc.renameat2",
            1,
        ),
        ("crates/tine-core/src/oplog/enrollment.rs", "open.create", 1),
        (
            "crates/tine-core/src/oplog/enrollment.rs",
            "open.create_new",
            1,
        ),
        (
            "crates/tine-core/src/oplog/hot_engine.rs",
            "cap.remove_dir",
            1,
        ),
        (
            "crates/tine-core/src/oplog/hot_engine.rs",
            "cap.remove_dir_all",
            1,
        ),
        (
            "crates/tine-core/src/oplog/identity.rs",
            "libc.openat.create",
            1,
        ),
        (
            "crates/tine-core/src/oplog/identity.rs",
            "open.create_new",
            1,
        ),
        ("crates/tine-core/src/oplog/import.rs", "fs.create_dir", 1),
        (
            "crates/tine-core/src/oplog/import.rs",
            "fs.create_dir_all",
            1,
        ),
        ("crates/tine-core/src/oplog/import.rs", "fs.remove_file", 5),
        ("crates/tine-core/src/oplog/import.rs", "fs.rename", 2),
        ("crates/tine-core/src/oplog/import.rs", "open.create_new", 2),
        (
            "crates/tine-core/src/oplog/lazy_genesis.rs",
            "fs.create_dir",
            1,
        ),
        (
            "crates/tine-core/src/oplog/lazy_genesis.rs",
            "fs.create_dir_all",
            2,
        ),
        (
            "crates/tine-core/src/oplog/lazy_genesis.rs",
            "fs.remove_dir_all",
            2,
        ),
        (
            "crates/tine-core/src/oplog/lazy_genesis.rs",
            "fs.remove_file",
            4,
        ),
        ("crates/tine-core/src/oplog/lazy_genesis.rs", "fs.rename", 6),
        (
            "crates/tine-core/src/oplog/lazy_genesis.rs",
            "open.create_new",
            6,
        ),
        (
            "crates/tine-core/src/oplog/object_store.rs",
            "cap.create_dir",
            6,
        ),
        (
            "crates/tine-core/src/oplog/object_store.rs",
            "cap.hard_link",
            1,
        ),
        (
            "crates/tine-core/src/oplog/object_store.rs",
            "cap.remove_file",
            4,
        ),
        (
            "crates/tine-core/src/oplog/object_store.rs",
            "cap.rename",
            2,
        ),
        (
            "crates/tine-core/src/oplog/object_store.rs",
            "libc.openat.create",
            1,
        ),
        (
            "crates/tine-core/src/oplog/object_store.rs",
            "open.create",
            1,
        ),
        (
            "crates/tine-core/src/oplog/object_store.rs",
            "open.create_new",
            3,
        ),
        (
            "crates/tine-core/src/oplog/projection_store.rs",
            "cap.remove_file",
            4,
        ),
        (
            "crates/tine-core/src/oplog/projection_store.rs",
            "cap.rename",
            2,
        ),
        (
            "crates/tine-core/src/oplog/projection_store.rs",
            "file.set_len",
            1,
        ),
        (
            "crates/tine-core/src/oplog/projection_store.rs",
            "fs.create_dir",
            1,
        ),
        (
            "crates/tine-core/src/oplog/projection_store.rs",
            "libc.mkdirat",
            1,
        ),
        (
            "crates/tine-core/src/oplog/projection_store.rs",
            "libc.openat.create",
            2,
        ),
        (
            "crates/tine-core/src/oplog/projection_store.rs",
            "libc.renameat",
            1,
        ),
        (
            "crates/tine-core/src/oplog/projection_store.rs",
            "libc.renameat2",
            1,
        ),
        (
            "crates/tine-core/src/oplog/projection_store.rs",
            "libc.unlinkat",
            1,
        ),
        (
            "crates/tine-core/src/oplog/projection_store.rs",
            "open.create",
            1,
        ),
        (
            "crates/tine-core/src/oplog/projection_store.rs",
            "open.create_new",
            1,
        ),
        (
            "crates/tine-core/src/oplog/receiver_absence_summary.rs",
            "cap.remove_file",
            2,
        ),
        (
            "crates/tine-core/src/oplog/resume_point.rs",
            "cap.remove_file",
            1,
        ),
        ("crates/tine-core/src/oplog/sqlite.rs", "cap.create_dir", 1),
        ("crates/tine-core/src/oplog/sqlite.rs", "fs.create_dir", 1),
        (
            "crates/tine-core/src/oplog/sqlite.rs",
            "fs.create_dir_all",
            2,
        ),
        ("crates/tine-core/src/oplog/sqlite.rs", "fs.remove_file", 1),
        (
            "crates/tine-core/src/oplog/sqlite.rs",
            "libc.openat.create",
            1,
        ),
        ("crates/tine-core/src/oplog/sqlite.rs", "open.create", 1),
        ("crates/tine-core/src/oplog/sqlite.rs", "open.create_new", 1),
        ("crates/tine-core/src/oplog/wire.rs", "cap.create_dir", 1),
        ("crates/tine-core/src/oplog/wire.rs", "cap.remove_file", 8),
        ("crates/tine-core/src/oplog/wire.rs", "cap.rename", 8),
        ("crates/tine-core/src/oplog/wire.rs", "file.set_len", 1),
        ("crates/tine-core/src/oplog/wire.rs", "fs.create_dir_all", 2),
        ("crates/tine-core/src/oplog/wire.rs", "libc.renameat2", 2),
        ("crates/tine-core/src/oplog/wire.rs", "open.create_new", 3),
        (
            "crates/tine-core/src/oplog/wire.rs",
            "windows.SetFileInformationByHandle",
            1,
        ),
        ("crates/tine-core/src/publish.rs", "cap.create_dir", 2),
        ("crates/tine-core/src/publish.rs", "cap.create_dir_all", 1),
        ("crates/tine-core/src/publish.rs", "cap.rename", 2),
        ("crates/tine-core/src/publish.rs", "fs.create_dir", 1),
        ("crates/tine-core/src/publish.rs", "fs.remove_dir_all", 1),
        ("crates/tine-core/src/publish.rs", "open.create_new", 1),
        ("crates/tine-core/src/sync_runtime.rs", "cap.remove_file", 3),
        (
            "crates/tine-core/src/sync_runtime.rs",
            "fs.create_dir_all",
            4,
        ),
        (
            "crates/tine-core/src/sync_runtime.rs",
            "fs.remove_dir_all",
            4,
        ),
        ("crates/tine-core/src/sync_runtime.rs", "fs.rename", 1),
        ("crates/tine-core/src/sync_runtime.rs", "open.create_new", 1),
        ("src-tauri/src/backup.rs", "cap.create_dir", 1),
        ("src-tauri/src/backup.rs", "cap.hard_link", 1),
        ("src-tauri/src/backup.rs", "cap.remove_file", 2),
        ("src-tauri/src/backup.rs", "cap.rename", 1),
        ("src-tauri/src/backup.rs", "fs.copy", 3),
        ("src-tauri/src/backup.rs", "fs.create_dir", 1),
        ("src-tauri/src/backup.rs", "fs.create_dir_all", 5),
        ("src-tauri/src/backup.rs", "fs.remove_dir_all", 3),
        ("src-tauri/src/backup.rs", "fs.remove_file", 1),
        ("src-tauri/src/backup.rs", "fs.rename", 2),
        ("src-tauri/src/backup.rs", "libc.renameat2", 1),
        ("src-tauri/src/backup.rs", "open.create_new", 3),
        ("src-tauri/src/commands.rs", "cap.remove_file", 1),
        ("src-tauri/src/data_home.rs", "fs.create_dir_all", 1),
        ("src-tauri/src/data_home.rs", "fs.remove_file", 1),
        ("src-tauri/src/data_home.rs", "fs.write", 1),
        ("src-tauri/src/debug.rs", "fs.create_dir_all", 1),
        ("src-tauri/src/debug.rs", "fs.remove_file", 5),
        ("src-tauri/src/debug.rs", "fs.rename", 3),
        ("src-tauri/src/debug.rs", "open.create", 1),
        ("src-tauri/src/graph.rs", "fs.create_dir", 1),
        ("src-tauri/src/graph.rs", "fs.create_dir_all", 1),
        (
            "src-tauri/src/linux_window_identity.rs",
            "fs.create_dir_all",
            1,
        ),
        (
            "src-tauri/src/linux_window_identity.rs",
            "fs.remove_file",
            1,
        ),
        ("src-tauri/src/linux_window_identity.rs", "fs.rename", 1),
        (
            "src-tauri/src/linux_window_identity.rs",
            "open.create_new",
            1,
        ),
        ("src-tauri/src/migrate_identifier.rs", "fs.copy", 1),
        (
            "src-tauri/src/migrate_identifier.rs",
            "fs.create_dir_all",
            2,
        ),
        (
            "src-tauri/src/migrate_identifier.rs",
            "fs.remove_dir_all",
            4,
        ),
        ("src-tauri/src/migrate_identifier.rs", "fs.rename", 4),
        ("src-tauri/src/plugins.rs", "fs.create_dir", 1),
        ("src-tauri/src/plugins.rs", "fs.create_dir_all", 2),
        ("src-tauri/src/plugins.rs", "fs.remove_dir", 1),
        ("src-tauri/src/plugins.rs", "fs.remove_dir_all", 2),
        ("src-tauri/src/plugins.rs", "fs.rename", 1),
        ("src-tauri/src/plugins.rs", "fs.write", 2),
        ("src-tauri/src/settings.rs", "fs.create_dir_all", 3),
        ("src-tauri/src/settings.rs", "fs.remove_file", 1),
        ("src-tauri/src/settings.rs", "fs.rename", 3),
        ("src-tauri/src/settings.rs", "fs.write", 1),
        ("src-tauri/src/settings.rs", "open.create_new", 1),
        ("src-tauri/src/sync_runtime.rs", "fs.create_dir", 1),
        ("src-tauri/src/sync_runtime.rs", "fs.create_dir_all", 2),
        ("src-tauri/src/sync_runtime.rs", "fs.remove_file", 1),
        ("src-tauri/src/sync_runtime.rs", "fs.rename", 3),
        ("src-tauri/src/sync_runtime.rs", "open.create_new", 1),
    ]
    .into_iter()
    .map(|(path, primitive, count)| (path.to_owned(), primitive.to_owned(), count))
    .collect::<Vec<_>>();
    assert_eq!(
        actual, expected,
        "update the census before accepting a primitive delta"
    );
}

#[test]
fn g_b_choke_helper_caller_counts_are_pinned() {
    let files = production_rust();
    let roots = [
        "managed_atomic_create_with_proof",
        "managed_atomic_write_validated",
        "managed_atomic_replace_bound",
        "atomic_publish",
        "atomic_write",
        "atomic_write_new",
        "atomic_replace_expected_with_hooks",
        "atomic_copy",
        "atomic_copy_new",
        "atomic_copy_file_new",
        "move_file_noreplace",
        "move_to_trash",
        "write_page_projection_with_attempts",
        "preserve_and_restore_projection_recovery",
        "reserve_and_rename",
        "create_projection_chain_component",
        "reserve_publish_stage",
        "reserve_publish_recovery",
        "commit_publish_stage",
        "write_publish_stage_file",
        "pending_projection_cleanup_bounded",
        "validate_pending_cleanup_round_root",
        "remove_mutation_authority_if_exact",
        "replace_mutation_authority_if_exact_inner",
        "move_pending_cleanup_marker_noreplace",
        "acquire_mutation_lease",
        "stage_object_bytes",
        "stage_manifest_bytes",
        "install_staged_artifact",
        "ensure_shared_provider_directory",
        "provider_retire_original_into_placeholder",
        "write_config",
        "atomic_update",
        "create_graph",
        "create_demo_graph",
        "reserve_restore_recovery",
        "publish_temp_noreplace",
        "atomic_copy_new_into_live",
        "move_live_to_recovery",
        "graph_name_folding",
        "probe_graph_name_folding",
    ];
    let actual = roots
        .into_iter()
        .map(|name| (name, call_count(&files, name)))
        .collect::<Vec<_>>();
    let expected = vec![
        ("managed_atomic_create_with_proof", 2),
        ("managed_atomic_write_validated", 2),
        ("managed_atomic_replace_bound", 1),
        ("atomic_publish", 2),
        ("atomic_write", 6),
        ("atomic_write_new", 11),
        ("atomic_replace_expected_with_hooks", 1),
        ("atomic_copy", 0),
        ("atomic_copy_new", 1),
        ("atomic_copy_file_new", 1),
        ("move_file_noreplace", 22),
        ("move_to_trash", 3),
        ("write_page_projection_with_attempts", 2),
        ("preserve_and_restore_projection_recovery", 2),
        ("reserve_and_rename", 2),
        ("create_projection_chain_component", 2),
        ("reserve_publish_stage", 1),
        ("reserve_publish_recovery", 2),
        ("commit_publish_stage", 1),
        ("write_publish_stage_file", 8),
        ("pending_projection_cleanup_bounded", 2),
        ("validate_pending_cleanup_round_root", 2),
        ("remove_mutation_authority_if_exact", 3),
        ("replace_mutation_authority_if_exact_inner", 1),
        ("move_pending_cleanup_marker_noreplace", 1),
        ("acquire_mutation_lease", 4),
        ("stage_object_bytes", 1),
        ("stage_manifest_bytes", 1),
        ("install_staged_artifact", 1),
        ("ensure_shared_provider_directory", 4),
        ("provider_retire_original_into_placeholder", 1),
        ("write_config", 9),
        ("atomic_update", 5),
        ("create_graph", 0),
        ("create_demo_graph", 1),
        ("reserve_restore_recovery", 2),
        ("publish_temp_noreplace", 1),
        ("atomic_copy_new_into_live", 4),
        ("move_live_to_recovery", 7),
        ("graph_name_folding", 2),
        ("probe_graph_name_folding", 2),
    ];
    assert_eq!(
        actual, expected,
        "update the producer-family census with every caller delta"
    );
}

#[test]
fn g_c_producer_classes_keep_representative_entrypoints_and_negative_gates() {
    let repo = repository_root();
    let read = |relative: &str| fs::read_to_string(repo.join(relative)).unwrap();
    let representatives = [
        ("PC-1", "crates/tine-core/src/model.rs", "pub fn save_page("),
        (
            "PC-2",
            "crates/tine-core/src/sync_runtime.rs",
            "fn execute_provider(",
        ),
        (
            "PC-3",
            "crates/tine-core/src/oplog/operational_coordinator.rs",
            "fn execute_clean_local(",
        ),
        (
            "PC-4",
            "crates/tine-core/src/oplog/operational_coordinator.rs",
            "fn execute_clean_external(",
        ),
        (
            "PC-5",
            "src-tauri/src/sync_runtime.rs",
            "open_record_with_progress",
        ),
        (
            "PC-6",
            "src-tauri/src/sync_runtime.rs",
            "shutdown_for_direct_files_escape",
        ),
        (
            "PC-7",
            "src-tauri/src/watcher.rs",
            "observe_legacy_graph_text_event",
        ),
        (
            "PC-8",
            "crates/tine-core/src/model.rs",
            "pub fn publish_html(&self)",
        ),
        (
            "PC-9",
            "src-tauri/src/commands.rs",
            "apply_journal_filename_migrations",
        ),
        (
            "PC-10",
            "crates/tine-core/src/sync_runtime.rs",
            "prepare_shared_clean",
        ),
        (
            "PC-11",
            "src-tauri/src/commands.rs",
            "set_preferred_workflow",
        ),
        (
            "PC-12",
            "src-tauri/src/graph.rs",
            "pub(crate) fn create_graph(",
        ),
        (
            "PC-13",
            "src-tauri/src/backup.rs",
            "pub(crate) async fn restore_backup(",
        ),
        (
            "PC-14",
            "crates/tine-core/src/graph_name_folding.rs",
            "probe_graph_name_folding",
        ),
        (
            "PC-15",
            "src-tauri/src/sync_runtime.rs",
            "archive_graph_provider_namespace",
        ),
        (
            "PC-16",
            "src-tauri/src/android_managed_storage_smoke.rs",
            "runManagedActivationSmoke",
        ),
        (
            "PC-17",
            "src-tauri/ios-folder-picker-native/ios/Sources/GraphFolderPickerPlugin.swift",
            ".tine-container",
        ),
        ("PC-18", "src-tauri/src/commands.rs", "edit_asset_external"),
        ("PC-19", "src-tauri/src/debug.rs", "save_diagnostic_report"),
    ];
    for (class, path, needle) in representatives {
        assert!(
            read(path).contains(needle),
            "{class} lost representative {path}:{needle}"
        );
    }
    assert!(read("src-tauri/src/backup.rs").contains("legacy_graph_cloned"));
    assert!(read("src-tauri/src/lib.rs")
        .contains("#[cfg(all(target_os = \"android\", debug_assertions))]\nmod android_managed_storage_smoke;"));
    let folding_callers = production_rust()
        .into_iter()
        .filter_map(|file| {
            let count = identifier_occurrences(&file.code, "graph_name_folding(")
                - file.code.matches("fn graph_name_folding(").count();
            (count != 0).then_some((file.relative, count))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        folding_callers,
        [(
            "crates/tine-core/src/managed_storage_journey.rs".to_owned(),
            2
        )],
        "PC-14 must remain confined to the Android managed journey"
    );
}

#[test]
fn g_d_tine_storage_write_boundaries_are_pinned() {
    let actual = token_inventory(
        &production_rust(),
        &[
            (
                "immutable.single_writer",
                "publish_immutable_exact_single_writer(",
            ),
            ("immutable.batch", "ExactImmutablePublicationBatch::new("),
            (
                "durable_directory.open",
                "DurableDirectoryPublication::open(",
            ),
            ("journal.v1.open", "LocalJournalSegment::open("),
            ("journal.v2.prepare", "::prepare_single_writer("),
            ("journal.v2.open", "LocalJournalSegmentV2::open_selected("),
            ("journal.fast_append", "self.segment.append("),
            ("journal.managed_append", ".append(payload_kind,payload)"),
            ("journal.turn_append", "self.journal.append("),
        ],
    );
    let expected = [
        (
            "crates/tine-core/src/fast_commit.rs",
            "journal.fast_append",
            1,
        ),
        ("crates/tine-core/src/fast_commit.rs", "journal.v1.open", 1),
        (
            "crates/tine-core/src/oplog/hot_engine.rs",
            "journal.managed_append",
            2,
        ),
        (
            "crates/tine-core/src/oplog/object_store.rs",
            "durable_directory.open",
            1,
        ),
        (
            "crates/tine-core/src/oplog/object_store.rs",
            "immutable.batch",
            2,
        ),
        (
            "crates/tine-core/src/oplog/object_store.rs",
            "immutable.single_writer",
            1,
        ),
        (
            "crates/tine-core/src/oplog/projection_turn_journal.rs",
            "durable_directory.open",
            2,
        ),
        (
            "crates/tine-core/src/oplog/projection_turn_journal.rs",
            "journal.turn_append",
            1,
        ),
        (
            "crates/tine-core/src/oplog/projection_turn_journal.rs",
            "journal.v2.open",
            2,
        ),
        (
            "crates/tine-core/src/oplog/projection_turn_journal.rs",
            "journal.v2.prepare",
            1,
        ),
        (
            "crates/tine-core/src/sync_runtime.rs",
            "durable_directory.open",
            3,
        ),
        ("crates/tine-core/src/sync_runtime.rs", "journal.v2.open", 2),
        (
            "crates/tine-core/src/sync_runtime.rs",
            "journal.v2.prepare",
            1,
        ),
    ]
    .into_iter()
    .map(|(path, boundary, count)| (path.to_owned(), boundary.to_owned(), count))
    .collect::<Vec<_>>();
    assert_eq!(
        actual, expected,
        "a new tine-storage write crossing needs a census row"
    );
}

#[test]
fn g_e_shipped_native_targets_and_writers_are_pinned() {
    let repo = repository_root();
    let ios_root = repo.join("src-tauri/ios-folder-picker-native/ios/Sources");
    let mut ios = fs::read_dir(&ios_root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    ios.sort();
    assert_eq!(ios, ["GraphFolderPickerPlugin.swift"]);
    let swift = fs::read_to_string(ios_root.join(&ios[0])).unwrap();
    assert_eq!(
        swift
            .matches("Data().write(to: marker, options: .atomic)")
            .count(),
        2
    );

    let android_root = repo.join("src-tauri/gen/android/app/src/main/java/page/tine/app");
    let mut android = fs::read_dir(&android_root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    android.sort();
    assert_eq!(
        android,
        [
            "GraphFolderPickerPlugin.kt",
            "MainActivity.kt",
            "MediaCapturePlugin.kt",
            "SafeBackPlugin.kt",
            "SystemBarsPlugin.kt",
        ]
    );
    let picker = fs::read_to_string(android_root.join("GraphFolderPickerPlugin.kt")).unwrap();
    assert_eq!(
        picker.matches("Intent.ACTION_OPEN_DOCUMENT_TREE").count(),
        1
    );
    for mutation in [
        "FileOutputStream",
        "createTempFile",
        "writeBytes",
        "outputStream(",
    ] {
        assert!(
            !picker.contains(mutation),
            "Android picker became a graph-tree writer: {mutation}"
        );
    }
    let media = fs::read_to_string(android_root.join("MediaCapturePlugin.kt")).unwrap();
    assert_eq!(media.matches("File.createTempFile(").count(), 2);
    assert_eq!(media.matches("activity.cacheDir").count(), 4);
}

#[test]
fn g_f_graph_path_process_handoffs_are_pinned() {
    let files = production_rust();
    let expected = [
        ("edit_asset_external", 2),
        ("open_asset", 1),
        ("open_page_source", 1),
        ("reveal_page_source", 4),
    ];
    let actual = expected.map(|(name, _)| (name, function_process_handoffs(&files, name)));
    assert_eq!(
        actual, expected,
        "new graph-path process handoffs need a PC-18 census row"
    );
}

#[test]
fn g_g_user_selected_report_writes_stay_on_the_atomic_family() {
    let repo = repository_root();
    for relative in [
        "src-tauri/src/debug.rs",
        "src-tauri/src/graph_verification.rs",
    ] {
        let source = code_mask(&without_test_items(
            &fs::read_to_string(repo.join(relative)).unwrap(),
        ));
        assert_eq!(
            source.matches("tine_core::model::atomic_write(").count(),
            1,
            "{relative}"
        );
        assert_eq!(source.matches("std::fs::write(").count(), 0, "{relative}");
        assert_eq!(source.matches("fs::write(").count(), 0, "{relative}");
    }
}

#[test]
fn census_guard_itself_names_every_required_guard() {
    let source = include_str!("projection_producer_census.rs");
    let tests = source
        .lines()
        .filter_map(|line| line.trim().strip_prefix("fn g_"))
        .filter_map(|line| line.split_once('(').map(|(name, _)| name))
        .collect::<BTreeSet<_>>();
    assert_eq!(tests.len(), 7);
    let prefixes = tests
        .iter()
        .map(|name| name.split('_').next().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        prefixes,
        BTreeSet::from(["a", "b", "c", "d", "e", "f", "g"])
    );
}
