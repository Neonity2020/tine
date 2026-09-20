//! I-11: tine-core sends SQL to the projection through ONE door,
//! `crates/tine-core/src/query/projection_sql.rs`. Every other production
//! file reaches the projection only through that door, and the number of
//! doorways per file is pinned so a new statement site must also join the
//! statement census (`projection_sql_tests.rs`).

#[path = "support/production_source.rs"]
mod production_source;

use production_source::{compiled_source, production_source_files, relative_path, repo_root};
use std::collections::BTreeMap;

const DOOR: &str = "crates/tine-core/src/query/projection_sql.rs";
const SEAM_CALLS: [&str; 3] = [
    ".run_projection_query(",
    ".visit_projection_query(",
    ".explain_query_plan(",
];
const DOOR_CALLS: [&str; 2] = ["projection_sql::run(", "projection_sql::visit("];

fn production_sources() -> BTreeMap<String, String> {
    let root = repo_root();
    production_source_files()
        .into_iter()
        .map(|path| (relative_path(&root, &path), compiled_source(&path)))
        .collect()
}

#[test]
fn only_the_door_names_the_storage_query_seam() {
    let mut offenders = Vec::new();
    for (file, source) in production_sources() {
        if file == DOOR {
            continue;
        }
        for call in SEAM_CALLS {
            if source.contains(call) {
                offenders.push(format!("{file}: {call}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "I-11: SQL reaches the projection only through {DOOR} \
         (crate::query::projection_sql::run / ::visit); route these through it:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn projection_statement_sites_are_pinned() {
    let mut sites: BTreeMap<String, usize> = BTreeMap::new();
    for (file, source) in production_sources() {
        let count = DOOR_CALLS
            .iter()
            .map(|call| source.matches(call).count())
            .sum::<usize>();
        if count > 0 {
            sites.insert(file, count);
        }
    }
    let expected: BTreeMap<String, usize> = [
        ("crates/tine-core/src/direct_projection.rs", 4),
        ("crates/tine-core/src/model/direct_query.rs", 3),
        ("crates/tine-core/src/query/export_results.rs", 5),
        ("crates/tine-core/src/query/friendly.rs", 4),
        ("crates/tine-core/src/query/registry_sql.rs", 1),
        ("crates/tine-core/src/query/results.rs", 5),
    ]
    .into_iter()
    .map(|(file, count)| (file.to_owned(), count))
    .collect();
    assert_eq!(
        sites, expected,
        "I-11: the set of projection statement sites changed. A new site must \
         be exercised by the statement census (projection_sql_tests.rs, \
         re-bless with TINE_BLESS_PROJECTION_STATEMENTS=1) and pinned here."
    );
}

#[test]
fn a_cfg_test_field_does_not_erase_the_item_after_its_struct() {
    // The eraser once ran from a `#[cfg(test)]` field attribute to the next
    // `{`, which was the `impl` block after the struct — so `DirectQueryJob`'s
    // one test-only field hid its whole `impl` (including the publication
    // fingerprint statement) from every source census.
    let source = "pub struct S {\n    #[cfg(test)]\n    pub x: u64,\n    pub y: u64,\n}\n\nimpl S {\n    fn f(&self) { projection_sql::visit(1); }\n}\n";
    let erased = production_source::erase_cfg_test_regions(source.to_owned());
    assert!(!erased.contains("pub x: u64"), "the gated field is erased");
    assert!(erased.contains("pub y: u64"), "the next field survives");
    assert!(
        erased.contains("projection_sql::visit(1)"),
        "the impl after the struct is production source: {erased}"
    );
    let generic =
        "struct T {\n    #[cfg(test)]\n    m: HashMap<String, u64>,\n    n: u64,\n}\nfn g() {}\n";
    let erased = production_source::erase_cfg_test_regions(generic.to_owned());
    assert!(!erased.contains("HashMap"));
    assert!(erased.contains("n: u64") && erased.contains("fn g()"));
}
