//! The one door through which tine-core sends SQL text to the projection.
//!
//! Every statement the app runs against the Direct Files projection passes
//! through [`run`] or [`visit`]; nothing else in this crate names
//! `run_projection_query`/`visit_projection_query` (I-11 — one seam, and
//! `tests/projection_sql_guards.rs` scans production source to keep it so).
//! The door adds no behaviour in production builds. Under `cfg(test)` it
//! records the normalized shape of every statement, which is what lets
//! `projection_sql_tests.rs` prove that each shape tine-core sends still
//! prepares and runs against the current schema — a renamed or dropped column
//! fails there, not in a user's Ctrl+K.
//!
//! This is deliberately not a runtime validator (campaign boundary B5): a
//! malformed statement fails a read of a disposable cache and can never
//! corrupt truth, so the projection has no SQL-text refusal.

use std::ops::ControlFlow;
use tine_storage::sqlite::{
    MaterializationError, PhysicalProjectionQuerySnapshot, PhysicalQueryValue,
};

/// Run one statement with bound parameters and collect its rows.
pub(crate) fn run(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    sql: &str,
    parameters: &[PhysicalQueryValue],
) -> Result<Vec<Vec<PhysicalQueryValue>>, MaterializationError> {
    #[cfg(test)]
    census::record(sql);
    snapshot.run_projection_query(sql, parameters)
}

/// Visit one row at a time without retaining the complete result set.
pub(crate) fn visit(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    sql: &str,
    parameters: &[PhysicalQueryValue],
    visitor: impl FnMut(&[PhysicalQueryValue]) -> Result<ControlFlow<()>, MaterializationError>,
) -> Result<(), MaterializationError> {
    #[cfg(test)]
    census::record(sql);
    snapshot.visit_projection_query(sql, parameters, visitor)
}

/// Test-only statement recorder. Query jobs run on worker threads, so the
/// store is process-wide rather than thread-local; the census test reads it
/// as a superset (other tests in the same process may add shapes).
#[cfg(test)]
pub(crate) mod census {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    static SHAPES: Mutex<BTreeMap<String, usize>> = Mutex::new(BTreeMap::new());

    pub(crate) fn record(sql: &str) {
        let shape = shape_of(sql);
        let mut shapes = SHAPES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *shapes.entry(shape).or_insert(0) += 1;
    }

    /// Every distinct shape recorded so far, with its count.
    pub(crate) fn recorded() -> BTreeMap<String, usize> {
        SHAPES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The shape of a statement: whitespace collapsed, and every run of
    /// positional placeholders (`?1, ?2, …` or `(?1), (?2), …`) folded to one
    /// placeholder so a batch of 3 ids and a batch of 128 ids are one shape.
    pub(crate) fn shape_of(sql: &str) -> String {
        let collapsed = sql.split_whitespace().collect::<Vec<_>>().join(" ");
        let numbered = regex::Regex::new(r"\?\d+").unwrap();
        let list = regex::Regex::new(r"\(\?(?:, \?)+\)").unwrap();
        let rows = regex::Regex::new(r"\(\?\)(?:, \(\?\))+").unwrap();
        let bare = numbered.replace_all(&collapsed, "?");
        let one_list = list.replace_all(&bare, "(?)");
        rows.replace_all(&one_list, "(?)").into_owned()
    }

    #[test]
    fn shapes_fold_placeholder_runs_and_whitespace() {
        assert_eq!(
            shape_of("SELECT a\n  FROM b WHERE id IN (?1, ?2, ?3) AND x = ?4"),
            "SELECT a FROM b WHERE id IN (?) AND x = ?"
        );
        assert_eq!(
            shape_of("WITH r(id) AS (VALUES (?1), (?2), (?3)) SELECT id FROM r"),
            "WITH r(id) AS (VALUES (?)) SELECT id FROM r"
        );
        assert_eq!(shape_of("SELECT ?1"), "SELECT ?");
        assert_eq!(shape_of("SELECT 1"), "SELECT 1");
    }
}

#[cfg(test)]
#[path = "projection_sql_tests.rs"]
mod tests;
