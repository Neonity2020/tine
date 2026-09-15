//! tine-core: parsing, serialization, and the graph model for a
//! Logseq-compatible outliner. Pure Rust, no GUI dependencies — fully unit
//! testable without the Tauri shell.

pub mod backend_error;
pub mod concord_ledger;
pub mod concord_queue;
pub mod config;
/// Test-only: every `Config` field either reaches `GraphMeta` (Rust and TS) or
/// says why it does not.
#[cfg(test)]
mod config_projection_parity;
pub mod date;
pub mod direct_move_recovery;
#[cfg(test)]
#[path = "direct_move_recovery_corpus_tests.rs"]
mod direct_move_recovery_corpus_tests;
mod direct_projection;
pub mod directory_identity;
pub mod doc;
pub mod edn;
mod filesystem_durability;

pub mod durability_counters;
pub mod graph_text_path;
pub mod graph_text_scope;
pub mod html_sanitize;
pub mod journal_feed;
pub mod logbook;
pub mod model;
pub mod onboarding;
pub mod org;
mod outline;
pub mod pdf;
#[cfg(test)]
pub(crate) mod projection_producer_census;
mod property_line;
pub mod publish;
pub mod query;
pub(crate) mod query_cursor;
mod query_jobs;
pub mod query_plan;
mod reference_evidence;
pub mod refs;
pub mod render;
pub mod search_query;
pub mod sync_diff;
#[cfg(test)]
mod test_support;
pub mod text_merge;
pub mod vocab;

/// Re-export the lsdoc parser so the Tauri shell can name its AST types
/// (`tine_core::lsdoc::ast::Block`) without depending on lsdoc directly.
pub use lsdoc;

pub use config::{Config, Workflow};
pub use date::JournalDate;
pub use doc::{DocBlock, Document};
pub use graph_text_scope::{
    GraphTextScope, GraphTextScopeBinding, GraphTextScopeBindingError,
    GRAPH_TEXT_SCOPE_BINDING_SCHEMA_VERSION, GRAPH_TEXT_SCOPE_VERSION,
};
pub use model::{
    ActivationIntent, BlockDto, BlockPreview, ConflictOverride, ConflictPresentation,
    EditorActivation, EditorActivationHandle, Graph, GraphMeta, LiveSaveConflictCapture, PageDto,
    PageEntry, PageKind, RefGroup, ReferencedPageNames,
};
