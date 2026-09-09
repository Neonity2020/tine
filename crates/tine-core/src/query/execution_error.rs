//! Availability of an execution, separate from query syntax and support.

use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryReadinessReason {
    Indexing,
    Recovering,
    PendingEdits,
    Busy,
}

impl QueryReadinessReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Indexing => "indexing",
            Self::Recovering => "recovering",
            Self::PendingEdits => "pending_edits",
            Self::Busy => "busy",
        }
    }
}

/// Bounded public reasons. Database errors, paths and source text cannot be
/// carried here; directed internal diagnostics have their own existing owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryUnavailableReason {
    ProjectionUnavailable,
    ReadFailed,
    InvalidSnapshot,
    UnsupportedRelation,
    StatisticsResourceLimit,
}

impl QueryUnavailableReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectionUnavailable => "projection_unavailable",
            Self::ReadFailed => "read_failed",
            Self::InvalidSnapshot => "invalid_snapshot",
            Self::UnsupportedRelation => "unsupported_relation",
            Self::StatisticsResourceLimit => "statistics_resource_limit",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::StatisticsResourceLimit => "Exact query statistics exceed the available memory limit. Narrow the query or remove grouping or aggregates.",
            Self::ProjectionUnavailable => "The query index is unavailable.",
            Self::ReadFailed => "The query index could not be read.",
            Self::InvalidSnapshot => "The query index returned inconsistent results.",
            Self::UnsupportedRelation => {
                "This query relation cannot be evaluated by the query index."
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryExecutionError {
    NotReady(QueryReadinessReason),
    Unavailable(QueryUnavailableReason),
    Cancelled,
}

impl QueryExecutionError {
    /// The existing tagged command rejection format, decoded by backend.ts.
    /// Only NotReady is eligible for the query readiness retry loop.
    pub fn backend_wire_string(self) -> String {
        match self {
            Self::NotReady(reason) => {
                crate::sync_runtime::tagged_backend_error("query-not-ready", Some(reason.as_str()))
            }
            Self::Unavailable(reason) => {
                crate::sync_runtime::tagged_backend_error_with_reason_and_detail(
                    "query-unavailable",
                    reason.as_str(),
                    serde_json::json!({ "message": reason.message() }),
                )
            }
            Self::Cancelled => {
                crate::sync_runtime::tagged_backend_error("operation-cancelled", None)
            }
        }
    }
}

impl fmt::Display for QueryExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotReady(QueryReadinessReason::Recovering) => "Rebuilding the query index…",
            Self::NotReady(_) => "Updating query results…",
            Self::Unavailable(reason) => reason.message(),
            Self::Cancelled => "Operation cancelled.",
        })
    }
}

impl std::error::Error for QueryExecutionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_failures_preserve_the_frontend_retry_boundary() {
        for reason in [
            QueryReadinessReason::Indexing,
            QueryReadinessReason::Recovering,
            QueryReadinessReason::PendingEdits,
            QueryReadinessReason::Busy,
        ] {
            let wire: serde_json::Value =
                serde_json::from_str(&QueryExecutionError::NotReady(reason).backend_wire_string())
                    .unwrap();
            assert_eq!(wire["kind"], "query-not-ready");
            assert_eq!(wire["reason_code"], reason.as_str());
            assert!(wire.get("detail").is_none());
        }
        for reason in [
            QueryUnavailableReason::ProjectionUnavailable,
            QueryUnavailableReason::ReadFailed,
            QueryUnavailableReason::InvalidSnapshot,
            QueryUnavailableReason::UnsupportedRelation,
            QueryUnavailableReason::StatisticsResourceLimit,
        ] {
            let wire: serde_json::Value = serde_json::from_str(
                &QueryExecutionError::Unavailable(reason).backend_wire_string(),
            )
            .unwrap();
            assert_eq!(wire["kind"], "query-unavailable");
            assert_eq!(wire["reason_code"], reason.as_str());
            assert_eq!(wire["detail"]["message"], reason.message());
        }
        let wire: serde_json::Value =
            serde_json::from_str(&QueryExecutionError::Cancelled.backend_wire_string()).unwrap();
        assert_eq!(wire, serde_json::json!({"kind": "operation-cancelled"}));
    }
}
