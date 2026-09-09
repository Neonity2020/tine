//! Statement-owned programs for SQLite result ordering.
//!
//! `tine-storage` supplies one fixed `tine_query_rank(program_id, text)` seam.
//! This table gives one statement as many independent ranking functions as it
//! needs without putting opaque closures in [`super::sql::SqlQuery`] or keeping
//! them beyond the snapshot operation. Future Friendly statements can bind
//! their lossless rank keys here beside the page sort programs.

use std::sync::Arc;

use tine_storage::sqlite::{MaterializationError, PhysicalProjectionQueryCancellation};

type RankProgram =
    dyn Fn(&str) -> Result<Option<Vec<u8>>, MaterializationError> + Send + Sync + 'static;

#[derive(Clone, Default)]
pub(crate) struct QueryRankPrograms {
    bindings: Vec<Arc<RankProgram>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JournalRankInput {
    StoredDay,
    DisplayName,
}

/// The two existing recency producers captured into owned statement inputs.
#[derive(Clone)]
pub(crate) struct PageRecencyPrograms {
    journal_input: JournalRankInput,
    journal: Arc<dyn Fn(&str) -> i64 + Send + Sync + 'static>,
    file: Arc<dyn Fn(&str) -> i64 + Send + Sync + 'static>,
}

impl PageRecencyPrograms {
    pub(crate) fn new(
        journal_input: JournalRankInput,
        journal: impl Fn(&str) -> i64 + Send + Sync + 'static,
        file: impl Fn(&str) -> i64 + Send + Sync + 'static,
    ) -> Self {
        Self {
            journal_input,
            journal: Arc::new(journal),
            file: Arc::new(file),
        }
    }

    pub(crate) fn bind(&self, programs: &mut QueryRankPrograms) -> BoundPageRecency {
        let journal = Arc::clone(&self.journal);
        let journal_id = programs.bind(move |text| {
            #[cfg(test)]
            crate::query::results::note_page_recency_lookup();
            Ok(Some(ordered_i64(journal(text))))
        });
        let file = Arc::clone(&self.file);
        let file_id = programs.bind(move |text| {
            #[cfg(test)]
            crate::query::results::note_page_recency_lookup();
            Ok(Some(ordered_i64(file(text))))
        });
        BoundPageRecency {
            journal_input: self.journal_input,
            journal_id,
            file_id,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BoundPageRecency {
    pub(crate) journal_input: JournalRankInput,
    pub(crate) journal_id: u64,
    pub(crate) file_id: u64,
}

impl QueryRankPrograms {
    /// Bind one operation-owned program and return its one-based SQL id.
    pub(crate) fn bind(
        &mut self,
        program: impl Fn(&str) -> Result<Option<Vec<u8>>, MaterializationError> + Send + Sync + 'static,
    ) -> u64 {
        self.bindings.push(Arc::new(program));
        self.bindings.len() as u64
    }

    pub(crate) fn bind_unicode_lowercase(&mut self) -> u64 {
        self.bind(|text| Ok(Some(text.to_lowercase().into_bytes())))
    }

    /// The single callback installed for this statement's whole program table.
    pub(crate) fn function(
        &self,
        cancellation: PhysicalProjectionQueryCancellation,
    ) -> impl Fn(u64, &str) -> Result<Option<Vec<u8>>, MaterializationError> + Send + 'static {
        let bindings = self.bindings.clone();
        move |id, text| {
            if cancellation.is_cancelled() {
                return Err(MaterializationError::Incomplete(
                    "query snapshot cancelled".into(),
                ));
            }
            let Some(program) = id.checked_sub(1).and_then(|at| bindings.get(at as usize)) else {
                return Err(MaterializationError::InvalidQuery(format!(
                    "query rank id {id} is not bound by this statement"
                )));
            };
            let ranked = program(text)?;
            if cancellation.is_cancelled() {
                return Err(MaterializationError::Incomplete(
                    "query snapshot cancelled".into(),
                ));
            }
            Ok(ranked)
        }
    }
}

impl std::fmt::Debug for QueryRankPrograms {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueryRankPrograms")
            .field("bindings", &self.bindings.len())
            .finish()
    }
}

/// Encode a signed numeric rank as a BLOB whose byte order is signed order.
pub(crate) fn ordered_i64(value: i64) -> Vec<u8> {
    ((value as u64) ^ (1 << 63)).to_be_bytes().to_vec()
}
