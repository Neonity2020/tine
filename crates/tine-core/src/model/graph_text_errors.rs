//! Graph-text validation and error constructors: event-parent
//! checks, exact-feed path validation, bounded admission causes, the Direct
//! save failure code, and the limit and alias errors.

use super::*;

#[cfg(unix)]
pub(super) fn projection_file_link_count(file: &fs::File) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.nlink())
}

#[cfg(windows)]
pub(super) fn projection_file_link_count(file: &fs::File) -> io::Result<u64> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` retains the exact live handle and `information` is a
    // correctly sized writable result value.
    let result = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u64::from(information.nNumberOfLinks))
}

#[cfg(not(any(unix, windows)))]
pub(super) fn projection_file_link_count(_file: &fs::File) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file link-count proof is unavailable on this platform",
    ))
}

pub(super) fn validate_graph_text_event_parent(
    index: &CompleteGraphTextAdmissionIndex,
    target: &GraphTextExactPath,
    parent: &ProjectionParent,
) -> io::Result<()> {
    let mut retained_relative = String::new();
    for (depth, directory) in parent.chain.iter().enumerate() {
        if depth > 0 {
            if !retained_relative.is_empty() {
                retained_relative.push('/');
            }
            retained_relative.push_str(&target.parent_components[depth - 1]);
        }
        let expected = index
            .directories_by_exact_relative
            .get(&retained_relative)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!(
                        "exact feed parent was not retained in the snapshot: {retained_relative}"
                    ),
                )
            })?;
        if canonical_projection_directory_resource_id(directory)? != *expected {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("exact feed parent identity changed: {retained_relative}"),
            ));
        }
    }
    Ok(())
}

const MAX_GRAPH_TEXT_ADMISSION_DIAGNOSTIC_CAUSE_BYTES: usize = 4096;

/// Exact-path shape bounds for one graph-relative platform event path.
/// Carried over unchanged from the deleted exact-feed batch that used to
/// namespace them; `classify_graph_text_exact_feed_path` is the live consumer.
const MAX_GRAPH_TEXT_EXACT_RELATIVE_BYTES: usize = 4096;
const MAX_GRAPH_TEXT_EXACT_PATH_COMPONENTS: usize = MAX_GRAPH_TEXT_CAPTURE_DIRECTORY_DEPTH + 1;

pub(super) fn validate_graph_text_exact_feed_relative(relative: &str) -> io::Result<()> {
    if relative != relative.trim()
        || relative.is_empty()
        || relative.len() > MAX_GRAPH_TEXT_EXACT_RELATIVE_BYTES
        || relative.starts_with('/')
        || relative.contains('\\')
        || relative.contains('\0')
        || relative.split('/').count() > MAX_GRAPH_TEXT_EXACT_PATH_COMPONENTS
        || relative
            .split('/')
            .any(|component| !projection_component_is_portable(component))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid exact graph-relative feed path",
        ));
    }
    Ok(())
}

pub(super) fn graph_text_exact_feed_failure_cause(cause: &str) -> String {
    let label = "directory mutation";
    let available = MAX_GRAPH_TEXT_ADMISSION_DIAGNOSTIC_CAUSE_BYTES.saturating_sub(label.len() + 2);
    let mut boundary = cause.len().min(available);
    while !cause.is_char_boundary(boundary) {
        boundary -= 1;
    }
    bounded_graph_text_admission_cause(format!("{label}: {}", &cause[..boundary]))
}
pub(super) fn bounded_graph_text_admission_cause(mut cause: String) -> String {
    if cause.len() <= MAX_GRAPH_TEXT_ADMISSION_DIAGNOSTIC_CAUSE_BYTES {
        return cause;
    }
    let mut boundary = MAX_GRAPH_TEXT_ADMISSION_DIAGNOSTIC_CAUSE_BYTES;
    while !cause.is_char_boundary(boundary) {
        boundary -= 1;
    }
    cause.truncate(boundary);
    cause
}

pub(super) fn graph_text_admission_unavailable(cause: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("graph-text admission authority unavailable: {cause}"),
    )
}

/// The observation epoch a banner-class conflict was minted at, if this error
/// carries one. The UI stores it with the banner and presents it back on "Keep
/// mine" so the override answers the conflict the user saw, not whatever
/// authority happens to be current when the request runs.
pub fn direct_save_conflict_epoch(error: &io::Error) -> Option<u64> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<DirectSaveError>())
        .and_then(DirectSaveError::conflict_epoch)
}

/// Read the BOUNDED failure code a Direct-Markdown save producer stamped on
/// this error, or `unknown` if it did not stamp one.
///
/// Two reasons the code exists rather than logging the error itself. First, the
/// error messages carry graph-relative paths, and a user's page titles are their
/// private data -- a diagnostic that cannot be pasted into a bug report is not a
/// diagnostic. Second, "the save failed" is useless triage: the failure classes
/// behind it (a symlink somewhere in the walk, ambient filesystem churn between
/// the capture's two passes, a same-bytes external replace that moved the inode,
/// a rejected reparse point) have completely different fixes, and from outside
/// the process they are otherwise indistinguishable.
///
/// This function matches NOTHING. The code is a typed field on `DirectSaveError`
/// set where the failure is constructed, so a page whose own title contains one
/// of the display sentences can no longer be classified as a conflict -- the
/// misclassification that made the banner's "Use disk version" discard an
/// unsaved edit. `direct_save_failure_code_does_not_inherit_conflict_from_page_text`
/// pins that.
///
/// The risk this moved the failure mode TO is a producer stamping the wrong
/// variant, so the guards are on the producers, not here:
/// `direct_save_conflict_sites_produce_their_own_codes` drives every
/// `EditorConflictSite` through the real minting helpers,
/// `direct_save_precheck_helpers_produce_their_own_codes` drives the free
/// helpers, and `every_direct_save_failure_code_has_a_production_producer`
/// scans shipped source for a construction site per variant.
///
/// A data-preservation refusal (`ProjectionSemanticRefusal`: merge markers,
/// a preamble the DTO would drop, a header property moved into the outline, an
/// Org file that cannot round-trip) is a verdict on the DTO's CONTENT, so
/// resending the same draft can only be refused again. It used to fall through
/// to `Unknown`, which the frontend retries twice and then reports as a failure
/// "after 3 tries" while the user is still typing (GH #546, GH #535). It gets
/// its own no-retry code here, in one place, so every producer of that marker
/// type is covered without stamping each call site.
pub fn direct_save_failure_code(error: &io::Error) -> &'static str {
    if super::is_projection_semantic_refusal(error) {
        return DirectSaveFailureCode::RefusedDataPreservation.as_str();
    }
    let Some(typed) = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<DirectSaveError>())
    else {
        return DirectSaveFailureCode::Unknown.as_str();
    };
    // The command boundary tags every error before classifying it
    // (`DirectSaveError::ensure_io`), so a refusal arrives wrapped as
    // `Unknown`. Classified as `unknown` it was retried and shown as
    // `unknown`: the v0.6.985 GH #535/#546 fix never reached the app.
    if typed.code() == DirectSaveFailureCode::Unknown
        && super::is_projection_semantic_refusal(&typed.source)
    {
        return DirectSaveFailureCode::RefusedDataPreservation.as_str();
    }
    typed.code().as_str()
}

pub(super) fn graph_text_capture_limit_error(resource: &'static str) -> io::Error {
    DirectSaveError::into_io(
        DirectSaveFailureCode::PrecheckLimit,
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("graph-text capture {resource} bound exceeded"),
        ),
    )
}

pub(super) fn graph_text_inventory_alias_error(
    resource: &'static str,
    first: &str,
    second: &str,
) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("graph {resource} alias one resource: {first} and {second}"),
    )
}
