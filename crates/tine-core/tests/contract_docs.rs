//! Doc-code consistency for the contracts that describe the Direct Files
//! projection (living contracts update in the same commit; AGENTS.md §2).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tine_storage::sqlite::{
    PhysicalGraphProjectionDatabase, PhysicalProjectionQuerySnapshot, PhysicalQueryValue,
};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

fn contract(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The table names of a projection freshly initialized by the pinned
/// tine-storage, read from `sqlite_master`.
fn fresh_projection_tables() -> BTreeSet<String> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("projection.sqlite");
    let database = PhysicalGraphProjectionDatabase::open_writable(&path).unwrap();
    database.initialize_schema().unwrap();
    drop(database);
    let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(())).unwrap();
    let rows = snapshot
        .run_projection_query(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            &[],
        )
        .unwrap();
    snapshot.finish();
    rows.into_iter()
        .map(|row| match row.into_iter().next() {
            Some(PhysicalQueryValue::Text(name)) => name,
            other => panic!("unexpected sqlite_master row {other:?}"),
        })
        .collect()
}

/// The table list §1.3 of the storage contract declares: every line of the
/// form "- `name` — purpose" inside the "Tables of the projection" block.
fn contract_tables(contract: &str) -> BTreeSet<String> {
    let start = contract
        .find("**Tables of the projection")
        .expect("storage contract §1.3 lists the projection's tables");
    let block = &contract[start..];
    let end = block.find("\n\n**").unwrap_or(block.len());
    block[..end]
        .lines()
        .filter(|line| line.trim_start().starts_with("- `"))
        .flat_map(|line| {
            let names = line.split(" — ").next().unwrap_or(line);
            names
                .split('`')
                .skip(1)
                .step_by(2)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn storage_contract_1_3_lists_exactly_the_tables_a_fresh_projection_creates() {
    let actual = fresh_projection_tables();
    eprintln!("fresh projection tables: {actual:?}");
    let listed = contract_tables(&contract("docs/storage-sync-contract.md"));
    assert_eq!(
        listed,
        actual,
        "docs/storage-sync-contract.md §1.3 is the projection's schema of record: \
         its table list must equal sqlite_master of a freshly initialized projection \
         (listed-but-absent: {:?}; present-but-unlisted: {:?})",
        listed.difference(&actual).collect::<Vec<_>>(),
        actual.difference(&listed).collect::<Vec<_>>()
    );
}

#[test]
fn direct_query_identities_contract_names_gates_that_exist() {
    let text = contract("docs/contracts/direct-query-identities.md");
    let gates_line = text
        .lines()
        .skip_while(|line| !line.starts_with("Gates:"))
        .take_while(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let gates = gates_line
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(!gates.is_empty(), "the identities contract names its gates");
    let root = repo_root().join("crates/tine-core");
    let mut sources = String::new();
    for dir in ["src", "tests"] {
        let mut stack = vec![root.join(dir)];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    sources.push_str(&std::fs::read_to_string(&path).unwrap());
                }
            }
        }
    }
    for gate in gates {
        assert!(
            sources.contains(&format!("fn {gate}(")),
            "docs/contracts/direct-query-identities.md names gate `{gate}` but no test \
             function of that name exists in crates/tine-core"
        );
    }
    for phrase in [
        "SQLite is a disposable projection, not durable identity authority",
        "No partial restoration is permitted",
        "A new Graph starts with no session mappings and derives structural runtime IDs",
    ] {
        assert!(
            text.contains(phrase),
            "identities contract lost the phrase: {phrase}"
        );
    }
}
