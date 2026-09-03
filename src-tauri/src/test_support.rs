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
