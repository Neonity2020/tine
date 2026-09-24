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
    /// Every variant, so a consumer that must enumerate the wire vocabulary —
    /// the diagnostic record's reason filter — derives it from here instead of
    /// keeping a copy that silently falls behind (GH #543, re-audit A2-N3).
    pub const ALL: [Self; 4] = [
        Self::Indexing,
        Self::Recovering,
        Self::PendingEdits,
        Self::Busy,
    ];

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
    /// The index could not be built or updated and has stopped trying for
    /// this session (GH #594, index liveness L1): the user retries it, or the
    /// next launch does. The class says why, as a fixed code.
    IndexFailed(IndexFailureClass),
}

impl QueryUnavailableReason {
    /// Every variant; see [`QueryReadinessReason::ALL`]. `IndexFailed` stands
    /// for all its classes: its wire code does not carry the class.
    pub const ALL: [Self; 6] = [
        Self::ProjectionUnavailable,
        Self::ReadFailed,
        Self::InvalidSnapshot,
        Self::UnsupportedRelation,
        Self::StatisticsResourceLimit,
        Self::IndexFailed(IndexFailureClass::Other),
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectionUnavailable => "projection_unavailable",
            Self::ReadFailed => "read_failed",
            Self::InvalidSnapshot => "invalid_snapshot",
            Self::UnsupportedRelation => "unsupported_relation",
            Self::StatisticsResourceLimit => "statistics_resource_limit",
            Self::IndexFailed(_) => "index_failed",
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
            Self::IndexFailed(_) => "The index couldn't be built.",
        }
    }

    /// The failure class an `IndexFailed` carries, for the wire detail.
    pub fn index_failure(self) -> Option<IndexFailureClass> {
        match self {
            Self::IndexFailed(class) => Some(class),
            _ => None,
        }
    }
}

/// Why the index could not be built or updated (GH #594, index liveness L5).
/// A fixed vocabulary: each code is chosen here from the error, never the
/// error's text, so a diagnostic report can carry it without a path or a
/// byte of graph content.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum IndexFailureClass {
    /// Another program holds a file the index needs (Windows sharing
    /// violation: an antivirus scan, a sync client, a second process).
    FileInUse,
    DiskFull,
    PermissionDenied,
    /// Any other operating-system I/O error.
    Io,
    OutOfMemory,
    /// SQLite stayed busy or locked.
    Busy,
    /// The index database is damaged.
    Corrupt,
    /// The rows Tine wrote contradict each other: a Tine defect.
    Constraint,
    /// The pages kept changing while the index read them.
    PagesKeptChanging,
    /// The graph folder could not be listed.
    GraphUnreadable,
    /// The graph-text writer could not be admitted.
    WriterRefused,
    /// A pass ended with the same work still owed.
    NoProgress,
    Other,
}

impl IndexFailureClass {
    pub const ALL: [Self; 13] = [
        Self::FileInUse,
        Self::DiskFull,
        Self::PermissionDenied,
        Self::Io,
        Self::OutOfMemory,
        Self::Busy,
        Self::Corrupt,
        Self::Constraint,
        Self::PagesKeptChanging,
        Self::GraphUnreadable,
        Self::WriterRefused,
        Self::NoProgress,
        Self::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::FileInUse => "file_in_use",
            Self::DiskFull => "disk_full",
            Self::PermissionDenied => "permission_denied",
            Self::Io => "io",
            Self::OutOfMemory => "out_of_memory",
            Self::Busy => "busy",
            Self::Corrupt => "corrupt",
            Self::Constraint => "constraint",
            Self::PagesKeptChanging => "pages_kept_changing",
            Self::GraphUnreadable => "graph_unreadable",
            Self::WriterRefused => "writer_refused",
            Self::NoProgress => "no_progress",
            Self::Other => "other",
        }
    }

    /// The class of a failed index write, from its error text. tine-storage
    /// carries SQLite's and the OS's errors as text, so this reads the codes
    /// and phrases they print; `index_failure_classes_read_real_errors`
    /// pins them against the real messages.
    pub fn of_message(message: &str) -> Self {
        let lower = message.to_ascii_lowercase();
        // The OS error codes are the platform's own: the same number means
        // different things on Windows and on the POSIX targets (Linux, macOS,
        // iOS, Android), and an error is classified where it arose.
        #[cfg(windows)]
        let (in_use, full, denied, memory): (&[i32], &[i32], &[i32], &[i32]) =
            (&[32, 33], &[39, 112], &[5], &[8, 14]);
        #[cfg(not(windows))]
        let (in_use, full, denied, memory): (&[i32], &[i32], &[i32], &[i32]) =
            (&[], &[28], &[1, 13], &[12]);
        let os_error = |codes: &[i32]| {
            codes
                .iter()
                .any(|code| lower.contains(&format!("os error {code})")))
        };
        if lower.contains(" constraint failed") {
            Self::Constraint
        } else if os_error(in_use)
            || lower.contains("being used by another process")
            || lower.contains("sharing violation")
        {
            Self::FileInUse
        } else if os_error(full)
            || lower.contains("no space left")
            || lower.contains("database or disk is full")
            || lower.contains("quota exceeded")
        {
            Self::DiskFull
        } else if os_error(denied)
            || lower.contains("permission denied")
            || lower.contains("access is denied")
        {
            Self::PermissionDenied
        } else if os_error(memory)
            || lower.contains("out of memory")
            || lower.contains("cannot allocate memory")
        {
            Self::OutOfMemory
        } else if lower.contains("database is locked") || lower.contains("database is busy") {
            Self::Busy
        } else if lower.contains("malformed")
            || lower.contains("not a database")
            || lower.contains("corrupt")
        {
            Self::Corrupt
        } else if lower.contains("os error") || lower.contains("disk i/o error") {
            Self::Io
        } else {
            Self::Other
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
                crate::backend_error::tagged_backend_error("query-not-ready", Some(reason.as_str()))
            }
            Self::Unavailable(reason) => {
                crate::backend_error::tagged_backend_error_with_reason_and_detail(
                    "query-unavailable",
                    reason.as_str(),
                    match reason.index_failure() {
                        Some(class) => serde_json::json!({
                            "message": reason.message(),
                            "indexFailure": class.as_str(),
                        }),
                        None => serde_json::json!({ "message": reason.message() }),
                    },
                )
            }
            Self::Cancelled => {
                crate::backend_error::tagged_backend_error("operation-cancelled", None)
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
    /// GH #594 L5: the classes read the errors as the OS and SQLite print
    /// them, on the platform they arose on.
    #[test]
    fn index_failure_classes_read_real_errors() {
        use super::IndexFailureClass as Class;
        let os = |code: i32| std::io::Error::from_raw_os_error(code).to_string();
        let cases: Vec<(String, Class)> = vec![
            (
                "UNIQUE constraint failed: blocks.result_id".into(),
                Class::Constraint,
            ),
            ("database or disk is full".into(), Class::DiskFull),
            ("database disk image is malformed".into(), Class::Corrupt),
            ("file is not a database".into(), Class::Corrupt),
            ("database is locked".into(), Class::Busy),
            ("disk I/O error".into(), Class::Io),
            ("no such table: pages".into(), Class::Other),
            #[cfg(windows)]
            (os(32), Class::FileInUse),
            #[cfg(windows)]
            (os(33), Class::FileInUse),
            #[cfg(windows)]
            (os(112), Class::DiskFull),
            #[cfg(windows)]
            (os(5), Class::PermissionDenied),
            #[cfg(windows)]
            (os(8), Class::OutOfMemory),
            #[cfg(not(windows))]
            (os(28), Class::DiskFull),
            #[cfg(not(windows))]
            (os(13), Class::PermissionDenied),
            #[cfg(not(windows))]
            (os(1), Class::PermissionDenied),
            #[cfg(not(windows))]
            (os(12), Class::OutOfMemory),
            // EIO is not "access denied", whatever Windows' 5 means.
            #[cfg(not(windows))]
            (os(5), Class::Io),
            #[cfg(not(windows))]
            (os(20), Class::Io),
            (
                "The process cannot access the file because it is being used by another \
                 process. (os error 32)"
                    .into(),
                Class::FileInUse,
            ),
        ];
        for (message, class) in cases {
            assert_eq!(Class::of_message(&message), class, "{message:?}");
        }
    }

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
