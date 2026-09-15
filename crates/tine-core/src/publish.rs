//! Static HTML export: render the whole graph to a folder of linked HTML pages
//! plus an index. Inline formatting, nested block lists, and `[[page]]` links
//! become real anchors between the generated files.

use crate::doc::{self, DocBlock};
use crate::model::{BlockDto, Graph, PageKind, RefGroup};
use crate::query::ir::{Bounds, ExecutionContext, PageRow, QueryResult, QueryRows, ViewSettings};
use crate::query::macro_text::is_query_macro_name;
use crate::query::QueryExecutionError;
use crate::refs::block_id;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use lsdoc::ast::{Block, Inline, Url};
#[cfg(not(target_os = "windows"))]
use same_file::Handle as FileIdentity;
use serde_json::json;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// `same_file::Handle` keeps its Windows handle open without FILE_SHARE_DELETE,
// which makes MoveFileW reject the final stage rename. Keep a separately-opened
// identity handle that does share deletion instead. We first compare it against
// the bound capability while both are open, so an ambient path swap cannot make
// the identity refer to a different directory; the live handle then prevents
// file-ID reuse through the move and supports ReFS's full 128-bit identities.
#[cfg(target_os = "windows")]
#[derive(Debug)]
struct FileIdentity {
    _file: fs::File,
    volume: u64,
    id: [u8; 16],
}

#[cfg(target_os = "windows")]
impl PartialEq for FileIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.volume == other.volume && self.id == other.id
    }
}

#[cfg(target_os = "windows")]
impl Eq for FileIdentity {}

#[cfg(target_os = "windows")]
fn identity_from_file(file: fs::File) -> io::Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let mut information = FILE_ID_INFO::default();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut information as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileIdentity {
        _file: file,
        volume: information.VolumeSerialNumber,
        id: information.FileId.Identifier,
    })
}

#[cfg(not(target_os = "windows"))]
fn identity_from_file(file: fs::File) -> io::Result<FileIdentity> {
    FileIdentity::from_file(file)
}

#[cfg(target_os = "windows")]
fn identity_from_path(path: &Path) -> io::Result<FileIdentity> {
    use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    identity_from_file(unsafe { fs::File::from_raw_handle(handle) })
}

#[cfg(not(target_os = "windows"))]
fn identity_from_path(path: &Path) -> io::Result<FileIdentity> {
    FileIdentity::from_path(path)
}

/// URL/file-safe slug for a page name (links and filenames must match).
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Per-export map from a page's display name (lowercased for case-insensitive
/// lookup, matching Logseq's case-insensitive page identity) to its UNIQUE,
/// GUARANTEED-NONEMPTY output slug. Built once per export (`build_slug_map`) and
/// used as the single source of truth for every filename, cross-page link, and
/// search-index entry — so a link can never diverge from the file it points at.
type SlugMap = std::collections::HashMap<String, String>;

/// One operation-owned current-main reader supplied by the publication
/// boundary. The renderer parses every authored surface, while the backend
/// adapter owns the coherent snapshot, registry and cancellation lifetime.
pub(crate) trait PublicationQueryRead {
    fn run_subtrees(
        &self,
        query: &crate::query::ir::Query,
        view: &ViewSettings,
        bounds: Bounds,
        context: &ExecutionContext,
    ) -> Result<crate::query::export_execute::SubtreeQueryResult, QueryExecutionError>;

    fn run(
        &self,
        query: &crate::query::ir::Query,
        view: &ViewSettings,
        bounds: Bounds,
        context: &ExecutionContext,
    ) -> Result<QueryResult, QueryExecutionError>;

    fn ensure_current(&self) -> Result<(), QueryExecutionError>;
}

#[derive(Debug)]
pub enum PrintPreparationError {
    Io(io::Error),
    Query(QueryExecutionError),
    Budget(&'static str),
}

const PRINT_SELECTION_REFUSAL: &str = "Couldn't prepare this page for PDF: a query exceeds the Print limit. Narrow the query and try again.";
const PRINT_SOURCE_REFUSAL: &str = "Couldn't prepare this page for PDF: a query source exceeds the 64 KiB Print limit. Shorten the query and try again.";
const PRINT_NESTING_REFUSAL: &str = "Couldn't prepare this page for PDF: a query exceeds the Print nesting limit of 64. Simplify the query and try again.";

impl std::fmt::Display for PrintPreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::Query(e) => e.fmt(f),
            Self::Budget(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PrintPreparationError {}
impl From<io::Error> for PrintPreparationError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<QueryExecutionError> for PrintPreparationError {
    fn from(error: QueryExecutionError) -> Self {
        Self::Query(error)
    }
}

impl PrintPreparationError {
    pub fn backend_wire_string(&self) -> String {
        match self {
            Self::Query(error) => error.backend_wire_string(),
            Self::Budget(message) => {
                crate::backend_error::tagged_backend_error_with_reason_and_detail(
                    "query-unavailable",
                    "print_query_budget_exceeded",
                    json!({ "message": message }),
                )
            }
            Self::Io(error) => error.to_string(),
        }
    }
}

fn publication_query_io_error(error: QueryExecutionError) -> io::Error {
    let kind = match error {
        QueryExecutionError::NotReady(_) => io::ErrorKind::WouldBlock,
        QueryExecutionError::Unavailable(_) => io::ErrorKind::Other,
        QueryExecutionError::Cancelled => io::ErrorKind::Interrupted,
    };
    io::Error::new(kind, error)
}

#[cfg(test)]
mod publish_test_counts {
    use crate::model::Graph;
    use std::cell::Cell;
    use std::path::Path;

    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
        static PAGE_DOC_LOADS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) struct Guard;

    pub(super) fn count_for(_root: &Path) -> Guard {
        ACTIVE.set(true);
        PAGE_DOC_LOADS.set(0);
        Guard
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            ACTIVE.set(false);
        }
    }

    pub(super) fn bump_page_doc_load(_graph: &Graph) {
        if ACTIVE.get() {
            PAGE_DOC_LOADS.set(PAGE_DOC_LOADS.get() + 1);
        }
    }

    pub(super) fn page_doc_loads() -> usize {
        PAGE_DOC_LOADS.get()
    }
}

/// FNV-1a 64-bit hash → 8 lowercase hex chars. Deterministic across runs (unlike
/// std's `DefaultHasher`/`RandomState`, which are randomly seeded), so re-exports
/// of the same graph produce identical filenames and diffs stay small.
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", (h & 0xffff_ffff) as u32)
}

/// The base slug for a page name, made NONEMPTY: `slug(name)` when it has any
/// ASCII-alnum content, else a stable `page-<hash>` token (a title of only
/// punctuation / non-ASCII would otherwise slug to the empty string).
fn base_slug(name: &str) -> String {
    let s = slug(name);
    if s.is_empty() {
        format!("page-{}", short_hash(name))
    } else {
        s
    }
}

/// Build the per-export name→slug map guaranteeing every page a UNIQUE, NONEMPTY
/// filename. `names` must be in a deterministic order (the caller sorts by name)
/// so the same graph always yields the same assignment. On a collision the loser
/// gets a stable `-<hash>` suffix (hash of its own name, so it's stable across
/// runs — not a mutable counter); a `-<n>` counter is only a last resort if even
/// the hashed slug collides. Returns the map plus the list of `(name, base,
/// chosen)` renames so the caller can warn about them. O(n) over pages.
fn build_slug_map(names: &[&str]) -> (SlugMap, Vec<(String, String, String)>) {
    let mut map = SlugMap::with_capacity(names.len());
    let mut used: HashSet<String> = HashSet::with_capacity(names.len());
    let mut collisions = Vec::new();
    for name in names {
        let base = base_slug(name);
        let mut chosen = base.clone();
        if used.contains(&chosen) {
            chosen = format!("{base}-{}", short_hash(name));
            // Vanishingly unlikely, but stay correct: if the hashed slug also
            // collides, disambiguate with a deterministic counter.
            if used.contains(&chosen) {
                let stem = chosen.clone();
                let mut k = 2u32;
                while used.contains(&chosen) {
                    chosen = format!("{stem}-{k}");
                    k += 1;
                }
            }
            collisions.push((name.to_string(), base, chosen.clone()));
        }
        used.insert(chosen.clone());
        map.insert(name.to_lowercase(), chosen);
    }
    (map, collisions)
}

/// Resolve a page name to its export slug via the per-export map (the single
/// source of truth). Falls back to a raw `slug()` for a name not in the map — a
/// reference to a page that isn't being exported (its link is dead either way),
/// or the single-page print export (`ctx.slugs == None`, no cross-page files).
enum ExportAssetUrl {
    Keep(String),
    Omitted,
}

/// Route a safe local URL through the export's asset sink when there is one:
/// a copied asset gets its in-export URL, an omitted one is reported, and
/// anything that is not a local asset reference passes through unchanged.
fn export_asset_url(ctx: &Ctx, url: &str) -> ExportAssetUrl {
    let Some(sink) = ctx.asset_sink else {
        return ExportAssetUrl::Keep(url.to_string());
    };
    if AssetSink::asset_relative(url).is_none() {
        return ExportAssetUrl::Keep(url.to_string());
    }
    match sink.borrow_mut().copy(url) {
        Some(published) => ExportAssetUrl::Keep(published),
        None => ExportAssetUrl::Omitted,
    }
}

fn page_slug(ctx: &Ctx, name: &str) -> String {
    ctx.slugs
        .and_then(|m| m.get(&name.to_lowercase()))
        .cloned()
        .unwrap_or_else(|| slug(name))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A print document crosses Rust -> IPC -> WebKit and base64 expands its inputs.
/// Bound both one pathological file and the complete export before any bytes are
/// read. The cumulative input ceiling keeps the resulting HTML below roughly
/// 43 MiB even when many otherwise-valid images are present.
const PRINT_ASSET_MAX_BYTES: u64 = 12 * 1024 * 1024;
const PRINT_ASSETS_TOTAL_MAX_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug)]
struct PrintAssetBudget {
    per_asset: u64,
    remaining: u64,
}

impl PrintAssetBudget {
    fn standard() -> Self {
        Self {
            per_asset: PRINT_ASSET_MAX_BYTES,
            remaining: PRINT_ASSETS_TOTAL_MAX_BYTES,
        }
    }
}

/// Read a local `assets/<file>` image referenced by an export `data-asset` path
/// (e.g. `../assets/cat.png`) and return it as a self-contained `data:` URI, for
/// the single-page print/PDF export. Returns `None` for a non-local URL (http/…),
/// no graph, an unreadable/oversized file, an exhausted export budget, or a path
/// that escapes `assets/`. The caller emits an inert omission marker rather than
/// leaving a broken or network-capable image in the privileged export flow.
fn inline_asset_uri(ctx: &Ctx, src: &str) -> Option<String> {
    let graph = ctx.graph?;
    let budget_cell = ctx.print_asset_budget?;
    // Only local asset references; leave remote/data URLs untouched.
    if src.contains("://") || src.starts_with("data:") {
        return None;
    }
    // `read_asset` re-guards against traversal; pass just the file name so a
    // `../assets/x` (or `assets/x`) ref resolves to `<graph>/assets/x`.
    let name = src.rsplit('/').next().unwrap_or(src);
    let mut budget = budget_cell.borrow_mut();
    let admission_limit = budget.per_asset.min(budget.remaining);
    let bytes = graph.read_asset_limited(name, admission_limit).ok()?;
    budget.remaining = budget.remaining.saturating_sub(bytes.len() as u64);
    let mime = match name
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("bmp") => "image/bmp",
        Some("avif") => "image/avif",
        _ => "application/octet-stream",
    };
    Some(format!("data:{mime};base64,{}", base64_encode(&bytes)))
}

/// Minimal standard base64 (no line wrapping) — used only to inline print-export
/// image assets as `data:` URIs. Dependency-free on purpose (tine-core carries no
/// base64 crate); correctness is covered by `base64_matches_known_vectors`.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18 & 63) as usize] as char);
        out.push(TABLE[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Where a block lives — its target page slug + its first content line — keyed by
/// the block's `id::` uuid, built from the exported (public) pages so that
/// `((block refs))` can link to the actual block (`<slug>.html#<uuid>`).
struct RefTarget {
    slug: String,
    text: String,
}
type RefIndex = std::collections::HashMap<String, RefTarget>;

struct Referrer {
    slug: String,
    page: String,
    anchor: String,
    text: String,
}
type ReverseRefIndex = std::collections::HashMap<String, Vec<Referrer>>;

fn collect_block_refs(blocks: &[DocBlock], slug: &str, refs: &mut RefIndex) {
    for b in blocks {
        if let Some(id) = block_id(&b.raw) {
            refs.insert(
                id,
                RefTarget {
                    slug: slug.to_string(),
                    text: ref_target_text(&b.raw),
                },
            );
        }
        collect_block_refs(&b.children, slug, refs);
    }
}

fn collect_reverse_refs(
    blocks: &[DocBlock],
    slug: &str,
    page: &str,
    counter: &mut u32,
    public_targets: &RefIndex,
    reverse: &mut ReverseRefIndex,
) {
    for block in blocks {
        let anchor = block_id(&block.raw).unwrap_or_else(|| {
            let anchor = format!("b{}", *counter);
            *counter += 1;
            anchor
        });
        let mut seen = HashSet::new();
        for target in &block.projection().block_refs {
            if !public_targets.contains_key(target) || !seen.insert(target.as_str()) {
                continue;
            }
            reverse.entry(target.clone()).or_default().push(Referrer {
                slug: slug.to_string(),
                page: page.to_string(),
                anchor: anchor.clone(),
                text: ref_target_text(&block.raw),
            });
        }
        collect_reverse_refs(
            &block.children,
            slug,
            page,
            counter,
            public_targets,
            reverse,
        );
    }
}

/// Append `s` HTML-escaped for an ATTRIBUTE value (`& < > " '`) — matches lsdoc's
/// `esc_attr`, so a re-emitted attribute (asset src, alt) round-trips identically.
fn esc_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Encode an opaque block id for the URL-fragment context. The same raw value is
/// HTML-escaped separately when emitted as an `id` attribute; treating these as
/// distinct contexts prevents a user-controlled `id::` from breaking either.
fn fragment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Closed URL policy for navigable content in a static export. External links
/// are limited to ordinary web/contact schemes; graph assets and local export
/// links may use a relative path or fragment. Scheme-like and network-path
/// references do not fall through to the relative case.
fn safe_export_url(url: &str) -> Option<&str> {
    let url = url.trim();
    if url.is_empty()
        || url.bytes().any(|b| b == 0 || b.is_ascii_control())
        || url.starts_with("//")
        || url.starts_with('\\')
        || url.contains('\\')
    {
        return None;
    }
    let first_delimiter = url.find(['/', '?', '#']).unwrap_or(url.len());
    if let Some(colon) = url.find(':').filter(|colon| *colon < first_delimiter) {
        let scheme = &url[..colon];
        if !scheme.bytes().enumerate().all(|(i, b)| {
            if i == 0 {
                b.is_ascii_alphabetic()
            } else {
                b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')
            }
        }) {
            return None;
        }
        return matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "mailto" | "tel"
        )
        .then_some(url);
    }
    Some(url)
}

fn safe_media_url(url: &str) -> Option<&str> {
    let safe = safe_export_url(url)?;
    let lower = safe.to_ascii_lowercase();
    if lower.starts_with("mailto:") || lower.starts_with("tel:") {
        None
    } else {
        Some(safe)
    }
}

/// Reverse lsdoc's HTML escaping for a `data-*` payload we re-emit elsewhere (e.g.
/// `data-tex` → visible KaTeX text, `data-asset` → an `src`). `&amp;` decodes LAST so
/// an escaped `&amp;lt;` becomes `&lt;`, not `<`.
fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The value of attribute `name` in a start-tag's inner text (`a class="x" data-page="y"`).
/// lsdoc always double-quotes and attribute-escapes values, so the value runs to the next
/// `"` (an interior quote is `&quot;`).
fn tag_attr<'a>(inner: &'a str, name: &str) -> Option<&'a str> {
    let pat = format!("{name}=\"");
    let start = inner.find(&pat)? + pat.len();
    let end = inner[start..].find('"')? + start;
    Some(&inner[start..end])
}

/// True if the tag's `class` attribute (space-separated) contains `cls`.
fn has_class(inner: &str, cls: &str) -> bool {
    tag_attr(inner, "class").is_some_and(|c| c.split_whitespace().any(|x| x == cls))
}

/// Consume `html` from `*i` up to and INCLUDING the next `</tag>`, returning the inner
/// text. For elements whose body lsdoc emits as plain text (block-ref) or leaves empty
/// (math / macro / raw-html / media).
fn take_to_close(html: &str, i: &mut usize, tag: &str) -> String {
    let close = format!("</{tag}>");
    match html[*i..].find(&close) {
        Some(rel) => {
            let body = html[*i..*i + rel].to_string();
            *i += rel + close.len();
            body
        }
        None => {
            let body = html[*i..].to_string();
            *i = html.len();
            body
        }
    }
}

/// Decorate lsdoc's canonical skeleton (`render_html`) for the STATIC export: resolve
/// the `data-*` hooks lsdoc leaves to the consumer. lsdoc owns structure + classes +
/// escaping; the export owns link resolution:
/// - page ref  → `<a class="ref" href="slug.html">name</a>` (brackets dropped)
/// - tag       → `<a class="tag" href="slug.html">#name</a>`
/// - block ref → in-page anchor via `refs` (muted text if the target isn't public)
/// - `data-tex`   → KaTeX `\(..\)` / `\[..\]` delimiters (typeset client-side)
/// - `data-asset` → the asset's path as `src`
/// - `data-lang`  → a `language-X` class for highlight.js
/// - `data-raw`   → the raw HTML, sanitized to the shared allowlist (`html_sanitize`) and emitted live
/// - macros       → EXPANDED when a graph is in context (`ctx.graph`): `query` runs the
///   query engine, `embed` inlines the target block/page, `video` embeds an iframe,
///   `namespace` lists child pages, user macros expand from config; unknown macros
///   render as muted literal `{{name …}}`. With no graph (unit tests of the inline
///   decorator) they drop, as before.
/// Everything else (tags, classes, escaped text, nesting, `td`/`th` `data-align`) passes
/// through verbatim.
///
/// O(n) single pass over the html: lsdoc escapes every text + attribute value, so the only
/// raw `<` in its output opens a tag — a `<`-delimited scan is exact and can't be fooled by
/// content. (`depth` bounds macro-expansion recursion; see `expand_macro`.)
fn decorate(html: &str, ctx: &Ctx, depth: u8) -> String {
    decorate_source(html, None, ctx, depth)
}

/// [`decorate`] with the ORIGINAL block source `html` was rendered from, when
/// the caller has it (§4.3.1).
///
/// **Why a query macro cannot use lsdoc's `data-args`.** The macro parser splits
/// arguments on commas and stops before the first `}`, so for
/// `{{query (task TODO) {:title "T"}}}` the AST argument is `(task TODO)
/// {:title "T"` — the options map's closing brace is MISSING — and
/// `content = 'a,b'` comes back as two arguments that rejoin as `'a, b'`. Both
/// are silent corruptions of the author's bytes, and `args.join(", ")` is the
/// reconstruction §4.3.1 forbids. So a query macro's argument is taken from the
/// raw source instead: the k-th query macro ELEMENT in this body is the k-th
/// query macro EXTENT in its source, because lsdoc emits elements in source
/// order and [`query_macro_extents`] scans in source order.
///
/// `raw = None` (macro expansion, decorator unit tests) keeps the old
/// reconstructed argument: it is lossy, but there is no raw slice to prefer.
fn decorate_source(html: &str, raw: Option<&str>, ctx: &Ctx, depth: u8) -> String {
    let raw_query_arguments: Vec<String> = raw
        .map(|raw| {
            crate::query::macro_text::query_macro_extents(raw)
                .into_iter()
                .map(|extent| extent.argument)
                .collect()
        })
        .unwrap_or_default();
    let mut query_macros_seen = 0usize;
    let refs = ctx.refs;
    let b = html.as_bytes();
    let mut out = String::with_capacity(html.len() + 64);
    let mut i = 0;
    // After a page-ref open tag, the next text node is the link body: strip a surrounding
    // `[[ ]]` (unlabeled `[[name]]`). A labeled ref's body starts with a tag, so the flag
    // is cleared without stripping.
    let mut strip_brackets = false;
    let mut inert_link_closures = 0usize;
    while i < b.len() {
        if b[i] != b'<' {
            let start = i;
            while i < b.len() && b[i] != b'<' {
                i += 1;
            }
            let text = &html[start..i];
            if strip_brackets {
                strip_brackets = false;
                let t = text.trim();
                if let Some(inner) = t.strip_prefix("[[").and_then(|x| x.strip_suffix("]]")) {
                    out.push_str(inner);
                    continue;
                }
            }
            out.push_str(text);
            continue;
        }
        let close = match html[i..].find('>') {
            Some(rel) => i + rel,
            None => {
                out.push_str(&html[i..]); // malformed tail — emit verbatim
                break;
            }
        };
        let inner = &html[i + 1..close];
        i = close + 1;
        // A labeled page-ref body that opened with a tag → not the `[[name]]` form.
        if strip_brackets && !inner.starts_with('/') {
            strip_brackets = false;
        }
        let name = inner.split([' ', '\t', '/']).next().unwrap_or("");

        if inner == "/a" && inert_link_closures > 0 {
            inert_link_closures -= 1;
            out.push_str("</span>");
            continue;
        }

        if name == "a" && has_class(inner, "page-ref") {
            if let Some(page) = tag_attr(inner, "data-page") {
                let page = unescape(page);
                if ctx.inert_outside_links && !publish_page_allowed(ctx, &page) {
                    // Outside the exported set: keep the authored text, drop the
                    // destination (there is no file to point at).
                    out.push_str("<span class=\"ref ref-outside\">");
                    inert_link_closures += 1;
                } else {
                    out.push_str(&format!(
                        "<a class=\"ref\" href=\"{}.html\">",
                        page_slug(ctx, &page)
                    ));
                }
                strip_brackets = true;
                continue;
            }
        }
        if name == "a" && has_class(inner, "tag") {
            if let Some(page) = tag_attr(inner, "data-page") {
                let page = unescape(page);
                if ctx.inert_outside_links && !publish_page_allowed(ctx, &page) {
                    out.push_str("<span class=\"tag tag-outside\">");
                    inert_link_closures += 1;
                } else {
                    out.push_str(&format!(
                        "<a class=\"tag\" href=\"{}.html\">",
                        page_slug(ctx, &page)
                    ));
                }
                continue;
            }
        }
        if name == "span" && has_class(inner, "block-ref") {
            if let Some(id_esc) = tag_attr(inner, "data-block") {
                let id = unescape(id_esc);
                let body = take_to_close(html, &mut i, "span"); // body is plain text
                let auto = format!("(({}))", id.chars().take(8).collect::<String>());
                match refs.get(&id) {
                    Some(t) => {
                        let text = if body == auto { esc(&t.text) } else { body };
                        out.push_str(&format!(
                            "<a class=\"ref block-ref\" href=\"{}.html#{}\">{}</a>",
                            t.slug,
                            fragment(&id),
                            text
                        ));
                    }
                    None => out.push_str(&format!("<span class=\"block-ref\">{body}</span>")),
                }
                continue;
            }
        }
        if name == "span" && has_class(inner, "math") {
            if let Some(tex_esc) = tag_attr(inner, "data-tex") {
                let _ = take_to_close(html, &mut i, "span"); // empty body
                let tex = esc(&unescape(tex_esc));
                let (l, r, cls) = if has_class(inner, "math-display") {
                    ("\\[", "\\]", "math math-display")
                } else {
                    ("\\(", "\\)", "math")
                };
                out.push_str(&format!("<span class=\"{cls}\">{l}{tex}{r}</span>"));
                continue;
            }
        }
        if name == "span" && has_class(inner, "macro") {
            let _ = take_to_close(html, &mut i, "span"); // empty element; args are in attrs
            let mname = tag_attr(inner, "data-macro")
                .map(unescape)
                .unwrap_or_default();
            let args = macro_args(tag_attr(inner, "data-args"));
            if is_query_macro_name(&mname) {
                let raw_argument = raw_query_arguments.get(query_macros_seen).cloned();
                query_macros_seen += 1;
                if let Some(argument) = raw_argument {
                    out.push_str(&expand_query_macro(&mname, argument.trim(), ctx, depth));
                    continue;
                }
            }
            out.push_str(&expand_macro(&mname, &args, ctx, depth));
            continue;
        }
        if name == "span" && has_class(inner, "raw-html") {
            let _ = take_to_close(html, &mut i, "span");
            if let Some(raw_esc) = tag_attr(inner, "data-raw") {
                // Sanitize to the shared allowlist and emit LIVE (mirrors the app's
                // DOMPurify pass — see html_sanitize). Handlers/`style`/`<script>`/
                // `<iframe>` are stripped; the surviving markup is already safe, so it
                // is pushed verbatim (NOT re-escaped).
                out.push_str(&crate::html_sanitize::sanitize(&unescape(raw_esc)));
            }
            continue;
        }
        if name == "img" && has_class(inner, "inline-image") {
            if let Some(asset) = tag_attr(inner, "data-asset") {
                let alt = tag_attr(inner, "alt").map(unescape).unwrap_or_default();
                let src = unescape(asset);
                if ctx.inline_assets {
                    if let Some(inlined) = inline_asset_uri(ctx, &src) {
                        out.push_str(&format!(
                            "<img class=\"inline-image\" src=\"{}\" alt=\"{}\">",
                            esc_attr(&inlined),
                            esc_attr(&alt)
                        ));
                    } else {
                        out.push_str(
                            "<span class=\"print-asset-omitted\">[Image omitted from PDF: unavailable or exceeds the print size limit]</span>",
                        );
                    }
                } else if let Some(src) = safe_media_url(&src) {
                    match export_asset_url(ctx, src) {
                        ExportAssetUrl::Keep(src) => out.push_str(&format!(
                            "<img class=\"inline-image\" src=\"{}\" alt=\"{}\">",
                            esc_attr(&src),
                            esc_attr(&alt)
                        )),
                        ExportAssetUrl::Omitted => out.push_str(
                            "<span class=\"asset-omitted\">[Image omitted: unavailable or over the export size limit]</span>",
                        ),
                    }
                } else {
                    out.push_str("<span class=\"unsafe-link\">[Unsafe image URL omitted]</span>");
                }
                continue;
            }
        }
        if (name == "video" || name == "audio") && has_class(inner, "media-embed") {
            if let Some(asset) = tag_attr(inner, "data-asset") {
                let _ = take_to_close(html, &mut i, name); // empty element
                let src = unescape(asset);
                if let Some(src) = safe_media_url(&src) {
                    match export_asset_url(ctx, src) {
                        ExportAssetUrl::Keep(src) => out.push_str(&format!(
                            "<{name} class=\"media-embed\" controls src=\"{}\"></{name}>",
                            esc_attr(&src)
                        )),
                        ExportAssetUrl::Omitted => out.push_str(
                            "<span class=\"asset-omitted\">[Media omitted: unavailable or over the export size limit]</span>",
                        ),
                    }
                } else {
                    out.push_str("<span class=\"unsafe-link\">[Unsafe media URL omitted]</span>");
                }
                continue;
            }
        }
        if name == "a" {
            if let Some(href) = tag_attr(inner, "href").map(unescape) {
                if safe_export_url(&href).is_some() {
                    match export_asset_url(ctx, &href) {
                        ExportAssetUrl::Keep(rewritten) if rewritten != href => {
                            // Re-emit the tag with the copied asset's URL; every
                            // other attribute stays as lsdoc rendered it.
                            let rebuilt = inner.replacen(
                                &format!("href=\"{}\"", esc_attr(&href)),
                                &format!("href=\"{}\"", esc_attr(&rewritten)),
                                1,
                            );
                            out.push('<');
                            out.push_str(&rebuilt);
                            out.push('>');
                        }
                        ExportAssetUrl::Keep(_) => {
                            out.push('<');
                            out.push_str(inner);
                            out.push('>');
                        }
                        ExportAssetUrl::Omitted => {
                            out.push_str("<span class=\"asset-omitted\">");
                            inert_link_closures += 1;
                        }
                    }
                } else {
                    out.push_str("<span class=\"unsafe-link\">");
                    inert_link_closures += 1;
                }
                continue;
            }
        }
        if name == "code" && has_class(inner, "hljs") {
            // data-lang → highlight.js's `language-X` class; body (escaped code) + the
            // `</code>` close pass through as the default text/close-tag.
            let lang = tag_attr(inner, "data-lang")
                .map(unescape)
                .unwrap_or_default();
            if lang.is_empty() {
                out.push_str("<code class=\"hljs\">");
            } else {
                out.push_str(&format!(
                    "<code class=\"hljs language-{}\">",
                    esc_attr(&lang)
                ));
            }
            continue;
        }
        // default: verbatim (incl. `th`/`td` keeping `data-align` for the export CSS)
        out.push('<');
        out.push_str(inner);
        out.push('>');
    }
    out
}

/// The destination string of a link `url` (mirrors the frontend `urlDest`).
fn url_dest(url: &Url) -> String {
    match url {
        Url::PageRef { v }
        | Url::BlockRef { v }
        | Url::Search { v }
        | Url::File { v }
        | Url::EmbedData { v } => v.clone(),
        Url::Complex { protocol, link } => match (protocol, link) {
            (Some(p), Some(l)) => format!("{p}://{l}"),
            (_, l) => l.clone().unwrap_or_default(),
        },
    }
}

/// Flatten an inline run to plain SEARCH text (mirrors lsdoc's `flatten_text` / the
/// frontend `astText`): keep plain/code text, emphasis children, `#tag`, page-ref names,
/// link labels; drop block-ref uuids, timestamps, macros, breaks (noise in an index).
fn flatten_inlines(inlines: &[Inline], out: &mut String) {
    for s in inlines {
        match s {
            Inline::Plain { text, .. }
            | Inline::Code { text, .. }
            | Inline::Verbatim { text, .. } => out.push_str(text),
            Inline::Emphasis { children, .. }
            | Inline::Subscript { children, .. }
            | Inline::Superscript { children, .. } => flatten_inlines(children, out),
            Inline::Tag { children, .. } => {
                out.push('#');
                flatten_inlines(children, out);
            }
            Inline::Link { url, label, .. } => match url {
                Url::BlockRef { .. } => {} // opaque uuid reads as noise
                _ if label.is_empty() => out.push_str(&url_dest(url)),
                _ => flatten_inlines(label, out),
            },
            Inline::NestedLink { content, .. } => out.push_str(content),
            Inline::Target { text, .. } => out.push_str(text),
            Inline::Entity { unicode, .. } => out.push_str(unicode),
            Inline::Latex { body, .. } => out.push_str(body),
            Inline::Hiccup { v, .. } => out.push_str(v),
            _ => {}
        }
    }
}

fn push_inlines(inlines: &[Inline], out: &mut String) {
    out.push(' ');
    flatten_inlines(inlines, out);
}

fn flatten_list(items: &[lsdoc::ast::ListItem], out: &mut String) {
    for it in items {
        if !it.name.is_empty() {
            push_inlines(&it.name, out);
        }
        flatten_blocks(&it.content, out);
        flatten_list(&it.items, out);
    }
}

/// Walk a block tree, accumulating its displayed text (the recursive analogue of
/// `flatten_inlines`). Properties / standalone-planning blocks are filtered by the
/// caller, so they never reach here.
fn flatten_blocks(blocks: &[Block], out: &mut String) {
    for b in blocks {
        match b {
            Block::Paragraph { inline, .. }
            | Block::Bullet { inline, .. }
            | Block::Heading { inline, .. }
            | Block::FootnoteDef { inline, .. } => push_inlines(inline, out),
            Block::List { items, .. } => flatten_list(items, out),
            Block::Src { code, .. } | Block::Example { code, .. } => {
                out.push(' ');
                out.push_str(code);
            }
            Block::Quote { children, .. } | Block::Custom { children, .. } => {
                flatten_blocks(children, out)
            }
            Block::Table { header, rows, .. } => {
                if let Some(h) = header {
                    for cell in h {
                        push_inlines(cell, out);
                    }
                }
                for row in rows {
                    for cell in row {
                        push_inlines(cell, out);
                    }
                }
            }
            Block::DisplayedMath { text, .. } => {
                out.push(' ');
                out.push_str(text);
            }
            Block::LatexEnv { content, .. } => {
                out.push(' ');
                out.push_str(content);
            }
            _ => {}
        }
    }
}

/// Plain text of a block's (already property/planning-filtered) body, off the same
/// lsdoc AST the renderer uses — the unit indexed for search + snippets. Whitespace is
/// collapsed; empty for a structural-only block. NO second markup stripper.
fn ast_plain_text(blocks: &[Block]) -> String {
    let mut out = String::new();
    flatten_blocks(blocks, &mut out);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse + property/planning-filter one block body the way `render_block` does — the
/// shared front of the render and search-index paths (one lsdoc parse per call).
fn body_blocks(raw: &str) -> Vec<Block> {
    crate::doc::strip_planning_lines(crate::render::parse_block(raw, false), raw)
        .into_iter()
        .filter(|b| !matches!(b, Block::Properties { .. }))
        .collect()
}

struct BeginQuery {
    title: Option<String>,
    query: String,
}

enum BeginQueryInspection {
    Supported(BeginQuery),
    Unsupported,
}

/// Return the authored payload only when `raw` is exactly one terminated
/// `#+BEGIN_QUERY` container. This mirrors `WHOLE_BEGIN_QUERY` in the frontend:
/// spaces/tabs are accepted around the delimiters, all three line endings are
/// accepted, and neither a prefix nor a suffix may share the block.
fn whole_begin_query_payload(raw: &str) -> Option<&str> {
    const BEGIN: &str = "#+BEGIN_QUERY";
    const END: &str = "#+END_QUERY";

    let bytes = raw.as_bytes();
    let mut begin = 0;
    while matches!(bytes.get(begin), Some(b' ' | b'\t')) {
        begin += 1;
    }
    let begin_end = begin.checked_add(BEGIN.len())?;
    if !raw.get(begin..begin_end)?.eq_ignore_ascii_case(BEGIN) {
        return None;
    }
    let mut payload_start = begin_end;
    while matches!(bytes.get(payload_start), Some(b' ' | b'\t')) {
        payload_start += 1;
    }
    payload_start += match bytes.get(payload_start) {
        Some(b'\r') if bytes.get(payload_start + 1) == Some(&b'\n') => 2,
        Some(b'\r' | b'\n') => 1,
        _ => return None,
    };

    // JavaScript's terminal `$` accepts one final line ending. Account for it
    // before locating the closing-delimiter line.
    let mut closing_end = raw.len();
    if raw[..closing_end].ends_with("\r\n") {
        closing_end -= 2;
    } else if matches!(bytes.get(closing_end.wrapping_sub(1)), Some(b'\r' | b'\n')) {
        closing_end -= 1;
    }
    while closing_end > payload_start && matches!(bytes[closing_end - 1], b' ' | b'\t') {
        closing_end -= 1;
    }

    let mut newline_start = closing_end;
    while newline_start > payload_start && !matches!(bytes[newline_start - 1], b'\r' | b'\n') {
        newline_start -= 1;
    }
    if newline_start == payload_start {
        return None;
    }
    let closing_line_start = newline_start;
    newline_start -= 1;
    if bytes[newline_start] == b'\n'
        && newline_start > payload_start
        && bytes[newline_start - 1] == b'\r'
    {
        newline_start -= 1;
    }
    let mut delimiter_start = closing_line_start;
    while delimiter_start < closing_end && matches!(bytes[delimiter_start], b' ' | b'\t') {
        delimiter_start += 1;
    }
    if !raw
        .get(delimiter_start..closing_end)?
        .eq_ignore_ascii_case(END)
    {
        return None;
    }
    Some(&raw[payload_start..newline_start])
}

fn skip_edn_trivia(source: &str, mut from: usize) -> usize {
    while from < source.len() {
        let c = source[from..].chars().next().expect("in bounds");
        if c.is_whitespace() || c == ',' {
            from += c.len_utf8();
        } else if c == ';' {
            from += 1;
            while from < source.len() && !matches!(source.as_bytes()[from], b'\n' | b'\r') {
                from += 1;
            }
        } else {
            break;
        }
    }
    from
}

fn edn_string_end(source: &str, from: usize) -> Option<usize> {
    let mut at = from + 1;
    while at < source.len() {
        let c = source[at..].chars().next()?;
        if c == '\\' {
            at += 1;
            let escaped = source.get(at..)?.chars().next()?;
            at += escaped.len_utf8();
        } else if c == '"' {
            return Some(at + 1);
        } else {
            at += c.len_utf8();
        }
    }
    None
}

fn edn_balanced_end(source: &str, from: usize) -> Option<usize> {
    fn closer(c: char) -> Option<char> {
        match c {
            '(' => Some(')'),
            '[' => Some(']'),
            '{' => Some('}'),
            _ => None,
        }
    }

    let first = source.get(from..)?.chars().next()?;
    let mut stack = vec![closer(first)?];
    let mut at = from + first.len_utf8();
    while at < source.len() {
        let c = source[at..].chars().next()?;
        if c == '"' {
            at = edn_string_end(source, at)?;
            continue;
        }
        if c == ';' {
            while at < source.len() && !matches!(source.as_bytes()[at], b'\n' | b'\r') {
                at += 1;
            }
            continue;
        }
        if let Some(close) = closer(c) {
            stack.push(close);
        } else if matches!(c, ')' | ']' | '}') {
            if stack.pop() != Some(c) {
                return None;
            }
            if stack.is_empty() {
                return Some(at + c.len_utf8());
            }
        }
        at += c.len_utf8();
    }
    None
}

fn edn_token_end(source: &str, mut from: usize) -> usize {
    while from < source.len() {
        let c = source[from..].chars().next().expect("in bounds");
        if c.is_whitespace() || c == ',' || matches!(c, '(' | ')' | '[' | ']' | '{' | '}') {
            break;
        }
        from += c.len_utf8();
    }
    from
}

fn edn_value_end(source: &str, from: usize) -> Option<usize> {
    match source.get(from..)?.chars().next()? {
        '"' => edn_string_end(source, from),
        '(' | '[' | '{' => edn_balanced_end(source, from),
        _ => {
            let end = edn_token_end(source, from);
            (end > from).then_some(end)
        }
    }
}

fn unquote_begin_query_title(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek().copied() {
                Some('\n' | '\r') | None => out.push(c),
                Some(_) => out.push(chars.next().expect("peeked character")),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn has_edn_keyword(source: &str, keyword: &str) -> bool {
    source.match_indices(keyword).any(|(start, _)| {
        source[start + keyword.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
    })
}

fn parse_begin_query_map(payload: &str) -> Option<BeginQuery> {
    let source = payload.trim();
    if !source.starts_with('{') {
        return None;
    }
    let map_end = edn_balanced_end(source, 0)?;
    if skip_edn_trivia(source, map_end) != source.len() {
        return None;
    }

    let mut title = None;
    let mut query = None;
    let mut at = 1;
    while at < map_end - 1 {
        at = skip_edn_trivia(source, at);
        if at >= map_end - 1 {
            break;
        }
        if source.as_bytes()[at] != b':' {
            return None;
        }
        let key_end = edn_token_end(source, at);
        let key = &source[at..key_end];
        at = skip_edn_trivia(source, key_end);
        let value_end = edn_value_end(source, at)?;
        if value_end > map_end - 1 {
            return None;
        }
        let value = &source[at..value_end];
        match key {
            ":query" if query.is_none() => query = Some(value.to_string()),
            ":query" => return None,
            ":title" if title.is_none() && value.starts_with('"') => {
                title = Some(unquote_begin_query_title(&value[1..value.len() - 1]));
            }
            ":title" => return None,
            _ => {}
        }
        at = value_end;
    }

    let query = query?;
    if !query.starts_with('[')
        || !has_edn_keyword(&query, ":find")
        || !has_edn_keyword(&query, ":where")
    {
        return None;
    }
    Some(BeginQuery { title, query })
}

/// Inspect raw authored text and use the parsed AST only as a confirmation that
/// the whole container is the one custom/query node the frontend would dispatch.
/// EDN is always sliced from `raw`; rendered/flattened AST text is never rebuilt.
fn inspect_begin_query(raw: &str, blocks: &[Block]) -> Option<BeginQueryInspection> {
    let payload = whole_begin_query_payload(raw)?;
    let body = if matches!(
        blocks.first(),
        Some(Block::Bullet { .. } | Block::Heading { .. })
    ) {
        &blocks[1..]
    } else {
        blocks
    };
    if !matches!(body, [Block::Custom { name, .. }] if name.eq_ignore_ascii_case("query")) {
        return Some(BeginQueryInspection::Unsupported);
    }
    Some(match parse_begin_query_map(payload) {
        Some(query) => BeginQueryInspection::Supported(query),
        None => BeginQueryInspection::Unsupported,
    })
}

/// The first visible block's plain text — a `((block ref))`'s shown label when it has none.
fn ref_target_text(raw: &str) -> String {
    let first: Vec<Block> = body_blocks(raw).into_iter().take(1).collect();
    ast_plain_text(&first)
}

/// Render context threaded through `render_block`/`decorate`: the block-ref index
/// (always) and the graph (present in a real export, absent in inline-decorator unit
/// tests — when absent, data macros drop while query macros refuse explicitly).
struct Ctx<'a> {
    refs: &'a RefIndex,
    reverse_refs: Option<&'a ReverseRefIndex>,
    graph: Option<&'a Graph>,
    /// The per-export unique/nonempty page-name→slug map (whole-graph site
    /// export). `None` for the single-page print export and inline-decorator unit
    /// tests, where there are no cross-page files — `page_slug` then falls back to
    /// a raw `slug()`.
    slugs: Option<&'a SlugMap>,
    /// When true (the single-page PDF/print export), rewrite each `data-asset`
    /// image `src` to a self-contained `data:` URI by reading the asset bytes,
    /// so the printed document needs no sibling `assets/` folder. The whole-graph
    /// site export keeps the relative `../assets/<file>` links (`false`).
    inline_assets: bool,
    /// Shared admission state for every image in one print document. Present
    /// exactly when `inline_assets` is true; all images therefore consume one
    /// cumulative byte ceiling before base64/IPC/DOM amplification.
    print_asset_budget: Option<&'a RefCell<PrintAssetBudget>>,
    /// The publication boundary's one operation-owned current-main reader.
    /// Inline/print renderers have no such owner and fail query surfaces
    /// explicitly instead of manufacturing an empty result.
    query_reader: Option<&'a dyn PublicationQueryRead>,
    print_error: Option<&'a RefCell<Option<PrintPreparationError>>>,
    /// Pass-1 parsed public page documents, keyed by Logseq page identity. Page
    /// embeds use this before falling back to disk for non-public/unseen pages.
    pages: Option<&'a HashMap<String, &'a doc::Document>>,
    /// Exact captured physical-path capabilities for `@page` rows. A page row
    /// must agree on path, title and kind before it may become a public link.
    page_links: Option<&'a HashMap<String, PublicationPageLink>>,
    /// A query export is a closed set chosen by the user: a `[[page]]` or `#tag`
    /// whose target is outside it renders as inert text instead of a dangling
    /// `<a href>` to a file that was never written. The graph site (`publish/`)
    /// keeps its historical dangling links (`false`); changing that is a
    /// separate contract, not a side effect of this flag.
    inert_outside_links: bool,
    /// A query export is a movable folder: every referenced local asset is
    /// copied into `<leaf>/assets/` and the link rewritten, through this sink.
    /// `None` keeps the graph site's `../assets/<file>` links.
    asset_sink: Option<&'a RefCell<AssetSink<'a>>>,
    /// Query exports only (Stage 2): where the renderer is, so each query
    /// runs exactly as the app will ask for it (its page as `current page`,
    /// its host block's `tine.*` properties merged into the view).
    scope: Option<&'a RefCell<app_export::RenderScope>>,
    /// Query exports with an app: every executed query, keyed as the app asks.
    recorder: Option<&'a RefCell<app_export::QueryRecorder>>,
}

/// One export's asset copier: bounded reads from the ORIGINAL graph's
/// `assets/`, one copy per referenced file, under ONE user-adjustable byte
/// budget. A missing or unreadable asset is omitted with a warning; an asset
/// that would take the export over budget FAILS the export (Martin,
/// 2026-09-14: a silently partial export is worse than a refusal that names
/// the limit and where to raise it).
struct AssetSink<'a> {
    source_root: &'a Path,
    stage: &'a PublishStage,
    /// Authored asset path → published relative URL.
    copied: HashMap<String, String>,
    budget: u64,
    remaining: u64,
    warnings: Vec<String>,
    /// Set once an asset did not fit; the export is then refused after render.
    over_budget: Option<String>,
}

/// The default byte budget for one query export's copied assets (Settings →
/// Graph → "Query export size limit" overrides it per device).
pub const QUERY_EXPORT_DEFAULT_ASSET_BUDGET_BYTES: u64 = 1024 * 1024 * 1024;

/// The export's copied assets would exceed the budget; carried inside the
/// `io::Error` so the query-export layer can hand the user a typed refusal
/// with the message intact.
#[derive(Debug)]
pub struct AssetBudgetExceeded(pub String);

impl std::fmt::Display for AssetBudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AssetBudgetExceeded {}

fn format_mib(bytes: u64) -> String {
    let mib = bytes as f64 / (1024.0 * 1024.0);
    if mib >= 100.0 {
        format!("{mib:.0} MiB")
    } else {
        format!("{mib:.1} MiB")
    }
}

impl AssetSink<'_> {
    /// The graph-relative `assets/…` path an authored reference names, or
    /// `None` for remote/data/unsafe/non-asset references.
    fn asset_relative(src: &str) -> Option<String> {
        if src.contains("://") || src.starts_with("data:") || src.contains('\\') {
            return None;
        }
        let trimmed = src.trim_start_matches("./");
        let rel = trimmed
            .strip_prefix("../assets/")
            .or_else(|| trimmed.strip_prefix("assets/"))?;
        let rel = rel.split(['?', '#']).next().unwrap_or(rel);
        if rel.is_empty()
            || Path::new(rel)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return None;
        }
        Some(rel.to_string())
    }

    /// Copy `src` into the stage once; returns the published relative URL, or
    /// `None` when the asset is missing (warning recorded) or would break the
    /// budget (the export is refused once rendering finishes).
    fn copy(&mut self, src: &str) -> Option<String> {
        let rel = Self::asset_relative(src)?;
        if let Some(published) = self.copied.get(&rel) {
            return Some(published.clone());
        }
        if self.over_budget.is_some() {
            return None;
        }
        let published = format!("assets/{rel}");
        let path = self.source_root.join("assets").join(&rel);
        let len = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => metadata.len(),
            Ok(_) => {
                self.warnings
                    .push(format!("Asset {rel} was omitted: not a regular file."));
                return None;
            }
            Err(error) => {
                self.warnings
                    .push(format!("Asset {rel} was omitted: {error}"));
                return None;
            }
        };
        if len > self.remaining {
            self.over_budget = Some(format!(
                "Export stopped: copying {rel} ({}) would take the export's assets past the {} limit \
                 ({} already copied). Raise \"Query export size limit\" in Settings → Graph, or \
                 take the asset off the exported pages.",
                format_mib(len),
                format_mib(self.budget),
                format_mib(self.budget - self.remaining),
            ));
            return None;
        }
        match read_file_bounded(&path, self.remaining) {
            Ok(Some(bytes)) => {
                if let Err(error) = write_publish_stage_asset(self.stage, &published, &bytes) {
                    self.warnings
                        .push(format!("Couldn't copy asset {rel}: {error}"));
                    return None;
                }
                self.remaining = self.remaining.saturating_sub(bytes.len() as u64);
                self.copied.insert(rel, published.clone());
                Some(published)
            }
            // Grew under an external editor between the size check and the read.
            Ok(None) => {
                self.over_budget = Some(format!(
                    "Export stopped: {rel} grew past the {} asset limit while it was being copied. \
                     Raise \"Query export size limit\" in Settings → Graph and try again.",
                    format_mib(self.budget)
                ));
                None
            }
            Err(error) => {
                self.warnings
                    .push(format!("Asset {rel} was omitted: {error}"));
                None
            }
        }
    }
}

/// Read a regular file, refusing (`Ok(None)`) once more than `max` bytes are
/// seen — bounded DURING the read, so a file growing under an external editor
/// cannot exceed the allowance between a metadata check and the read.
fn read_file_bounded(path: &Path, max: u64) -> io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file",
        ));
    }
    if metadata.len() > max {
        return Ok(None);
    }
    let file = fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Ok(None);
    }
    Ok(Some(bytes))
}

/// Write one copied asset at `assets/<rel>` inside the stage. Only that
/// prefix, only normal path components; parents are created inside the bound
/// stage handle.
fn write_publish_stage_asset(
    stage: &PublishStage,
    published: &str,
    bytes: &[u8],
) -> io::Result<()> {
    write_publish_stage_nested(stage, "assets", published, bytes)
}

/// Write one file under a nested prefix (`assets/…` for copied assets,
/// `app/…` for the read-only app bundle) inside the stage: only that prefix,
/// only normal path components; parents are created inside the bound stage
/// handle.
fn write_publish_stage_nested(
    stage: &PublishStage,
    prefix: &str,
    published: &str,
    bytes: &[u8],
) -> io::Result<()> {
    let relative = Path::new(published);
    let ok = relative.starts_with(prefix)
        && relative != Path::new(prefix)
        && relative
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        && relative.file_name().is_some();
    if !ok {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("static-publish file must live under {prefix}/"),
        ));
    }
    publish_stage_write_race_hook(stage)?;
    if let Some(parent) = relative.parent().filter(|p| !p.as_os_str().is_empty()) {
        stage.dir.create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = stage.dir.open_with(relative, &options)?;
    file.write_all(bytes)?;
    crate::durability_counters::sync_file(&file)
}

struct PublicationPageLink {
    name: String,
    kind: PageKind,
    slug: String,
}

/// lsdoc render options for a Markdown block body (the canonical skeleton the export decorates).
fn md_opts() -> lsdoc::RenderOpts {
    lsdoc::RenderOpts {
        format: lsdoc::Format::Md,
    }
}

/// Parse `data-args` (lsdoc emits a JSON array of strings, attribute-escaped) into its items.
fn macro_args(attr: Option<&str>) -> Vec<String> {
    attr.map(unescape)
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

/// A task marker's checkbox state, mirroring the app's `taskCheckboxState`
/// (`src/markers.ts`): DONE = checked, CANCELED/CANCELLED = no box, any other
/// marker = an empty box.
fn checkbox_state(marker: &str) -> Option<bool> {
    match marker {
        "DONE" => Some(true),
        "CANCELED" | "CANCELLED" => None,
        _ => Some(false),
    }
}

/// The header-line facet chrome that precedes a block's body text: the task
/// checkbox + marker badge and the `[#A]` priority badge (matches the app's Block header).
fn emit_header_facets(marker: Option<&str>, priority: Option<&str>, out: &mut String) {
    if let Some(m) = marker {
        match checkbox_state(m) {
            Some(true) => out.push_str("<span class=\"task-checkbox checked\"></span>"),
            Some(false) => out.push_str("<span class=\"task-checkbox\"></span>"),
            None => {}
        }
        out.push_str(&format!(
            "<span class=\"task-marker m-{}\">{}</span> ",
            m.to_ascii_lowercase(),
            esc(m)
        ));
    }
    if let Some(p) = priority {
        out.push_str(&format!(
            "<span class=\"priority p-{}\">[#{}]</span> ",
            p.to_ascii_lowercase(),
            esc(p)
        ));
    }
}

/// A block property is chrome we hide from the rendered page (the app hides these too):
/// the block `id::`, the collapsed flag, and any `logseq.*` internal key.
fn is_hidden_prop(key: &str) -> bool {
    key == "id" || key == "collapsed" || key.starts_with("logseq.")
}

/// The trailing facet chrome shown BELOW a block's body: SCHEDULED / DEADLINE
/// planning lines, the time-tracking summary, and the block's visible
/// `key:: value` properties.
fn emit_trailer_facets(
    scheduled: Option<&str>,
    deadline: Option<&str>,
    raw: &str,
    props: &[(String, String)],
    out: &mut String,
) {
    if let Some(s) = scheduled {
        out.push_str(&format!(
            "<div class=\"planning scheduled\"><span class=\"pk\">SCHEDULED:</span> {}</div>",
            esc(s)
        ));
    }
    if let Some(d) = deadline {
        out.push_str(&format!(
            "<div class=\"planning deadline\"><span class=\"pk\">DEADLINE:</span> {}</div>",
            esc(d)
        ));
    }
    // The app shows an elapsed-time badge on blocks with LOGBOOK clock rows
    // while keeping the drawer itself hidden. The static export hides the
    // drawer the same way, so it must carry the badge — otherwise the
    // time-tracking evidence vanishes from the page.
    let clocked = crate::logbook::clock_summary_seconds(raw);
    if clocked > 0 {
        out.push_str(&format!(
            "<div class=\"planning logbook\"><span class=\"pk\">CLOCK:</span> {:02}:{:02}:{:02}</div>",
            clocked / 3600,
            (clocked / 60) % 60,
            clocked % 60
        ));
    }
    let visible: Vec<&(String, String)> =
        props.iter().filter(|(k, _)| !is_hidden_prop(k)).collect();
    if !visible.is_empty() {
        out.push_str("<div class=\"block-props\">");
        for (k, v) in visible {
            out.push_str(&format!(
                "<div class=\"prop\"><span class=\"pk\">{}::</span> <span class=\"pv\">{}</span></div>",
                esc(k),
                esc(v)
            ));
        }
        out.push_str("</div>");
    }
}

/// Render one block's inner: header facets + the decorated body + trailer facets.
/// Shared by the top-level renderer and the embedded/query-result renderers so a
/// task in a query result looks exactly like a task on its own page.
fn emit_block_inner(raw: &str, out: &mut String, ctx: &Ctx, depth: u8) {
    let blk = DocBlock::new(raw);
    out.push_str(if blk.marker() == Some("DONE") {
        "<div class=\"b done\">"
    } else {
        "<div class=\"b\">"
    });
    emit_header_facets(blk.marker(), blk.priority(), out);
    let body = decorate(
        &lsdoc::render_html(&body_blocks(raw), &md_opts()),
        ctx,
        depth,
    );
    out.push_str(&body);
    out.push_str("</div>");
    emit_trailer_facets(blk.scheduled(), blk.deadline(), raw, &blk.properties(), out);
}

/// Render a query/embed result block (a `BlockDto` from the query engine) as an
/// `<li>` with its facets + children, at `depth` (bounds recursion).
fn render_result_block(dto: &BlockDto, out: &mut String, ctx: &Ctx, depth: u8) {
    out.push_str("<li>");
    emit_block_inner(&dto.raw, out, ctx, depth);
    if !dto.children.is_empty() {
        out.push_str("<ul>");
        for c in &dto.children {
            render_result_block(c, out, ctx, depth);
        }
        out.push_str("</ul>");
    }
    out.push_str("</li>");
}

fn collect_wanted_doc_blocks<'a>(
    blocks: &'a [DocBlock],
    wanted: &std::collections::HashSet<&str>,
    found: &mut std::collections::HashMap<&'a str, &'a DocBlock>,
) {
    for block in blocks {
        if wanted.contains(block.uuid.as_str()) {
            found.insert(block.uuid.as_str(), block);
        }
        // OG can retain a matching descendant below a non-matching child of a
        // retained ancestor. Keep walking so both roots hydrate from source.
        collect_wanted_doc_blocks(&block.children, wanted, found);
    }
}

struct HydratedQueryBlock<'a> {
    page: &'a str,
    block: &'a DocBlock,
}

/// Query DTOs intentionally carry shallow membership rows. Static publishing
/// has the source graph in-process, so hydrate each result subtree directly from
/// its page once instead of shipping/caching overlapping owned DTO trees.
/// A result without a source in this exact projection has no publication
/// capability. Never fall back to cached DTO bytes.
fn with_hydrated_query_groups(
    graph: &Graph,
    groups: &[RefGroup],
    action: impl FnOnce(&[HydratedQueryBlock<'_>]),
) {
    graph.with_pages(|pages| {
        // One lookup index for the complete query avoids O(pages * groups)
        // source-page scans during static/print export.
        let page_by_key = pages
            .iter()
            .map(|(entry, doc)| ((entry.name.as_str(), entry.kind), doc.as_ref()))
            .collect::<std::collections::HashMap<_, _>>();
        let mut hydrated = Vec::new();
        for group in groups {
            let Some(doc) = page_by_key.get(&(group.page.as_str(), group.kind)) else {
                continue;
            };
            let wanted = group
                .blocks
                .iter()
                .map(|block| block.id.as_str())
                .collect::<std::collections::HashSet<_>>();
            let mut found = std::collections::HashMap::with_capacity(wanted.len());
            collect_wanted_doc_blocks(&doc.roots, &wanted, &mut found);
            for block in &group.blocks {
                if let Some(source) = found.get(block.id.as_str()) {
                    hydrated.push(HydratedQueryBlock {
                        page: group.page.as_str(),
                        block: source,
                    });
                }
            }
        }
        action(&hydrated);
    });
}

fn render_query_groups(graph: &Graph, groups: &[RefGroup], out: &mut String, ctx: &Ctx, depth: u8) {
    with_hydrated_query_groups(graph, groups, |hydrated| {
        for row in hydrated {
            render_embedded_block(row.block, out, ctx, depth);
        }
    });
}

/// Render an embedded page's block (a `DocBlock`) as an `<li>`, mirroring `render_result_block`.
/// Stage 2: while a block's body renders, its `tine.*` properties are the ones
/// the app hands `parseQuery` for any macro in it. Returns the previous set.
fn enter_block_scope(ctx: &Ctx, b: &DocBlock) -> Option<Vec<(String, String)>> {
    ctx.scope.map(|scope| {
        std::mem::replace(
            &mut scope.borrow_mut().block_properties,
            app_export::tine_block_properties(b),
        )
    })
}

fn leave_block_scope(ctx: &Ctx, previous: Option<Vec<(String, String)>>) {
    if let (Some(scope), Some(previous)) = (ctx.scope, previous) {
        scope.borrow_mut().block_properties = previous;
    }
}

fn render_embedded_block(b: &DocBlock, out: &mut String, ctx: &Ctx, depth: u8) {
    out.push_str("<li>");
    let scope_properties = enter_block_scope(ctx, b);
    emit_block_inner(&b.raw, out, ctx, depth);
    leave_block_scope(ctx, scope_properties);
    if !b.children.is_empty() {
        out.push_str("<ul>");
        for c in &b.children {
            render_embedded_block(c, out, ctx, depth);
        }
        out.push_str("</ul>");
    }
    out.push_str("</li>");
}

// ================= Sheets: static read-only views =================
//
// A block carrying `tine.view:: table|board|grid` is an interactive sheet in
// the app (`src/sheet/*` + SheetTable/SheetBoard/SheetGrid); its plain-text
// twin is a nested outline that Logseq reads as ordinary bullets with harmless
// `tine.*` properties. Before this section, the export published exactly that
// plain-text twin — heading, visible config chips, nested bullets — so the
// Guide's Sheets pages lost the presentation entirely. A static document
// cannot offer the editing surface, but it must still carry the view's
// MEANING: this section renders each supported sheet as read-only HTML that
// preserves content, column/field labels, board grouping, grid positions, and
// links. Field discovery, labels, group ordering, and aggregate semantics
// mirror the app (`src/sheet/{config,fields,aggregate}.ts`), narrowed to what
// a static export computes: no editing affordances; formula columns evaluate
// only the arithmetic subset below — anything richer shows its stored
// definition in the column header instead of a wrong value.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SheetView {
    Table,
    Board,
    Grid,
}

/// A sheet column identity — the static mirror of the app's `FieldId`
/// (`src/sheet/fields.ts`). Display labels match `fieldLabel`.
#[derive(Clone, PartialEq, Eq, Debug)]
enum SheetField {
    State,
    Priority,
    Scheduled,
    Deadline,
    Tags,
    Page,
    Prop(String),
    Formula(String),
}

impl SheetField {
    fn label(&self) -> &str {
        match self {
            SheetField::State => "State",
            SheetField::Priority => "Priority",
            SheetField::Scheduled => "Scheduled",
            SheetField::Deadline => "Deadline",
            SheetField::Tags => "Tags",
            SheetField::Page => "Page",
            SheetField::Prop(name) | SheetField::Formula(name) => name,
        }
    }
    /// Machine identity used by `tine.col-aggregates` keys (`prop:estimate`,
    /// `state`, `formula:effort`, …).
    fn id(&self) -> String {
        match self {
            SheetField::State => "state".into(),
            SheetField::Priority => "priority".into(),
            SheetField::Scheduled => "scheduled".into(),
            SheetField::Deadline => "deadline".into(),
            SheetField::Tags => "tags".into(),
            SheetField::Page => "page".into(),
            SheetField::Prop(name) => format!("prop:{name}"),
            SheetField::Formula(name) => format!("formula:{name}"),
        }
    }
}

/// The sheet config carried by a view-owning block's `tine.*` properties
/// (static mirror of `sheetConfig`; widths/filters have no static meaning and
/// are ignored).
struct SheetConfig {
    view: SheetView,
    group_by: Option<String>,
    header: bool,
    /// `(aggregate key, fn)` in declared order, e.g. `("prop:estimate", "sum")`.
    aggregates: Vec<(String, String)>,
    /// Declared `tine.fields` columns in declaration order.
    declared: Vec<SheetDecl>,
    /// `(name, expression)` formula columns in declared order.
    formulas: Vec<(String, String)>,
    /// Which of those columns a QUERY-backed face shows, resolved by the ONE
    /// column resolver (`query::view::resolve_query_columns`) rather than by a
    /// second precedence written here. A children-backed sheet ignores it:
    /// selecting visible columns is a query-face question, and ordinary sheets
    /// keep their existing behaviour exactly.
    columns: crate::query::view::QueryColumns,
    /// Which field a QUERY-backed board groups by, resolved by the ONE grouping
    /// resolver (`query::view::resolve_query_grouping`). A children-backed
    /// board ignores it and keeps reading `group_by` exactly as it does today:
    /// the new key and its legacy reinterpretation are a query-face question
    /// (P5B).
    grouping: crate::query::view::QueryGrouping,
}

/// One `tine.fields` declaration: its column, whether cells render as
/// checkboxes, and (for `enum:` types) the declared value order — which is
/// also a board's column order when grouped by that field.
struct SheetDecl {
    field: SheetField,
    checkbox: bool,
    enum_values: Vec<String>,
}

const SHEET_AGGREGATE_FNS: &[&str] = &[
    "sum",
    "average",
    "median",
    "min",
    "max",
    "range",
    "stddev",
    "earliest",
    "latest",
    "empty",
    "filled",
    "unique",
    "checked",
    "unchecked",
    "count",
];

fn sheet_aggregate_label(f: &str) -> &str {
    match f {
        "sum" => "Sum",
        "average" => "Average",
        "median" => "Median",
        "min" => "Min",
        "max" => "Max",
        "range" => "Range",
        "stddev" => "Stddev",
        "earliest" => "Earliest",
        "latest" => "Latest",
        "empty" => "Empty",
        "filled" => "Filled",
        "unique" => "Unique",
        "checked" => "Checked",
        "unchecked" => "Unchecked",
        _ => "Count",
    }
}

/// The app hides these keys from rendered output (`isRenderHiddenProp`,
/// case-insensitive) and never offers them as sheet columns: internal ids,
/// styling hints, table config. `tine.*` config is likewise never a column.
fn sheet_hidden_key(key: &str, user_hidden: &[String]) -> bool {
    let norm = crate::doc::property_key_norm(key);
    if norm.starts_with("tine.") || norm.starts_with("logseq.") {
        return true;
    }
    const HIDDEN: &[&str] = &[
        "id",
        "collapsed",
        "hl-page",
        "hl-color",
        "hl-type",
        "ls-type",
        "background-color",
        "heading",
        "title",
        "filters",
        "created-at",
        "updated-at",
        "last-modified-at",
        "query-table",
        "query-properties",
        "query-sort-by",
        "query-sort-desc",
    ];
    HIDDEN.contains(&norm.as_str())
        || user_hidden
            .iter()
            .any(|k| crate::doc::property_key_norm(k) == norm)
}

fn sheet_config(props: &[(String, String)]) -> Option<SheetConfig> {
    let mut cfg = SheetConfig {
        view: SheetView::Table,
        group_by: None,
        header: false,
        aggregates: Vec::new(),
        declared: Vec::new(),
        formulas: Vec::new(),
        columns: crate::query::view::resolve_query_columns(props),
        grouping: crate::query::view::resolve_query_grouping(
            props,
            &crate::query::ir::ViewSettings::default(),
        ),
    };
    let mut seen_declared: HashSet<String> = HashSet::new();
    let mut view: Option<SheetView> = None;
    for (raw_key, raw_value) in props {
        let key = crate::doc::property_key_norm(raw_key);
        let value = raw_value.trim();
        match key.as_str() {
            "tine.view" => {
                view = match value.to_ascii_lowercase().as_str() {
                    "table" => Some(SheetView::Table),
                    "board" => Some(SheetView::Board),
                    "grid" => Some(SheetView::Grid),
                    _ => None,
                };
            }
            "tine.group-by" => {
                cfg.group_by = if value.is_empty() {
                    None
                } else {
                    Some(value.into())
                }
            }
            "tine.header" => cfg.header = value.eq_ignore_ascii_case("true"),
            "tine.col-aggregates" => {
                for part in value.split(';') {
                    if let Some((k, f)) = part.split_once('=') {
                        let (k, f) = (k.trim(), f.trim().to_ascii_lowercase());
                        if !k.is_empty() && SHEET_AGGREGATE_FNS.contains(&f.as_str()) {
                            cfg.aggregates.push((k.into(), f));
                        }
                    }
                }
            }
            "tine.fields" => {
                for part in value.split(';') {
                    let Some((name, token)) = part.split_once('=') else {
                        continue;
                    };
                    let name = name.trim();
                    let token = token.trim();
                    if name.is_empty() || !seen_declared.insert(name.into()) {
                        continue;
                    }
                    let builtin = match name {
                        "state" => Some(SheetField::State),
                        "priority" => Some(SheetField::Priority),
                        "scheduled" => Some(SheetField::Scheduled),
                        "deadline" => Some(SheetField::Deadline),
                        "tags" => Some(SheetField::Tags),
                        "page" => Some(SheetField::Page),
                        _ => None,
                    };
                    if let Some(field) = builtin {
                        // Builtin declarations are `name=name` (mirrors parseFields).
                        if token == name {
                            cfg.declared.push(SheetDecl {
                                field,
                                checkbox: false,
                                enum_values: Vec::new(),
                            });
                        }
                        continue;
                    }
                    let enum_values: Vec<String> = if let Some(list) = token.strip_prefix("enum:") {
                        list.split(',')
                            .map(|v| v.trim().to_string())
                            .filter(|v| !v.is_empty())
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let valid = !enum_values.is_empty()
                        || matches!(
                            token,
                            "text" | "number" | "date" | "datetime" | "checkbox" | "list" | "ref"
                        );
                    if valid {
                        cfg.declared.push(SheetDecl {
                            field: SheetField::Prop(name.into()),
                            checkbox: token == "checkbox",
                            enum_values,
                        });
                    }
                }
            }
            _ => {
                if let Some(name) = key.strip_prefix("tine.formula.") {
                    let name = name.trim();
                    // Mirrors the app's `formulaNameValid` (/^[a-z0-9-]+$/).
                    if !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    {
                        cfg.formulas.push((name.into(), value.into()));
                    }
                }
            }
        }
    }
    view.map(|v| {
        cfg.view = v;
        cfg
    })
}

/// One sheet row: the source block (page bullet or hydrated query result).
struct SheetRow<'a> {
    block: &'a DocBlock,
    /// The result's page (query-backed rows only; needed for the Page column).
    page: Option<&'a str>,
}

impl<'a> SheetRow<'a> {
    /// The plain-text value used for cells (pre-decoration), aggregates, and
    /// formula operands — mirrors `readField`/`groupKeysForBlock` text.
    fn field_text(&self, field: &SheetField, formulas: &[(String, String)]) -> String {
        let b = self.block;
        match field {
            SheetField::State => b.marker().unwrap_or("").into(),
            SheetField::Priority => b.priority().unwrap_or("").into(),
            SheetField::Scheduled => b.scheduled().unwrap_or("").into(),
            SheetField::Deadline => b.deadline().unwrap_or("").into(),
            SheetField::Tags => b.tags().join(" "),
            SheetField::Page => self.page.unwrap_or("").into(),
            SheetField::Prop(name) => b
                .properties()
                .into_iter()
                .find(|(k, _)| {
                    crate::doc::property_key_norm(k) == crate::doc::property_key_norm(name)
                })
                .map(|(_, v)| v)
                .unwrap_or_default(),
            SheetField::Formula(name) => {
                let expr = formulas
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, e)| e.as_str());
                let Some(expr) = expr else {
                    return String::new();
                };
                let Some(ast) = parse_sheet_arith(expr) else {
                    return String::new();
                };
                eval_sheet_arith(&ast, self, formulas)
                    .map(|v| format!("{v}"))
                    .unwrap_or_default()
            }
        }
    }
}

// ---- Formula columns: the arithmetic subset ----
//
// The full formula language (src/sheet/formula: lexer, parser, eval over
// numbers/text/booleans/dates/durations/lists, visual-builder faces) is an
// interactive-editing engine; cloning it into the export would duplicate a
// whole subsystem. The static subset — number literals, row-field references,
// `+ - * /`, parentheses, unary minus — covers derived numeric columns like
// the Guide's `rating * 2`. Anything outside it is not a failure: the column
// keeps its label and stored definition, cells stay empty (the app's
// null-propagation for unresolvable rows behaves the same).

enum SheetArith {
    Num(f64),
    Field(String),
    Neg(Box<SheetArith>),
    Bin(u8, Box<SheetArith>, Box<SheetArith>),
}

fn parse_sheet_arith(src: &str) -> Option<SheetArith> {
    fn ws(s: &[u8], mut i: usize) -> usize {
        while i < s.len() && s[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    }
    fn number(s: &[u8], mut i: usize) -> Option<(f64, usize)> {
        let start = i;
        while i < s.len()
            && (s[i].is_ascii_digit()
                || s[i] == b'.'
                || (matches!(s[i], b'e' | b'E') && i > start && s[i - 1].is_ascii_digit()))
        {
            i += 1;
        }
        // Optional exponent tail (`e-3`, `E+2`).
        if i > start && matches!(s[i - 1], b'e' | b'E') {
            let mut j = i;
            if matches!(s.get(j), Some(b'+') | Some(b'-')) {
                j += 1;
            }
            let digits = j;
            while j < s.len() && s[j].is_ascii_digit() {
                j += 1;
            }
            if j == digits {
                i = start + (i - start) - 1; // bare 'e' is not part of the number
            } else {
                i = j;
            }
        }
        let text = std::str::from_utf8(&s[start..i]).ok()?;
        text.parse::<f64>().ok().map(|n| (n, i))
    }
    fn ident(s: &[u8], mut i: usize) -> Option<(String, usize)> {
        let start = i;
        while i < s.len() && (s[i].is_ascii_alphanumeric() || matches!(s[i], b'_' | b'.' | b'-')) {
            i += 1;
        }
        if i == start || !s[start].is_ascii_alphabetic() && s[start] != b'_' {
            return None;
        }
        Some((String::from_utf8_lossy(&s[start..i]).into_owned(), i))
    }
    fn factor(s: &[u8], i: usize) -> Option<(SheetArith, usize)> {
        let i = ws(s, i);
        match s.get(i)? {
            b'(' => {
                let (inner, j) = expr(s, i + 1)?;
                let j = ws(s, j);
                if s.get(j) != Some(&b')') {
                    return None;
                }
                Some((inner, j + 1))
            }
            b'-' => {
                let (inner, j) = factor(s, i + 1)?;
                Some((SheetArith::Neg(Box::new(inner)), j))
            }
            c if c.is_ascii_digit() || *c == b'.' => {
                let (n, j) = number(s, i)?;
                Some((SheetArith::Num(n), j))
            }
            c if c.is_ascii_alphabetic() || *c == b'_' => {
                let (name, j) = ident(s, i)?;
                Some((SheetArith::Field(name), j))
            }
            _ => None,
        }
    }
    fn term(s: &[u8], i: usize) -> Option<(SheetArith, usize)> {
        let (mut left, mut i) = factor(s, i)?;
        loop {
            let j = ws(s, i);
            match s.get(j) {
                Some(op @ (b'*' | b'/')) => {
                    let (right, k) = factor(s, j + 1)?;
                    left = SheetArith::Bin(*op, Box::new(left), Box::new(right));
                    i = k;
                }
                _ => return Some((left, i)),
            }
        }
    }
    fn expr(s: &[u8], i: usize) -> Option<(SheetArith, usize)> {
        let (mut left, mut i) = term(s, i)?;
        loop {
            let j = ws(s, i);
            match s.get(j) {
                Some(op @ (b'+' | b'-')) => {
                    let (right, k) = term(s, j + 1)?;
                    left = SheetArith::Bin(*op, Box::new(left), Box::new(right));
                    i = k;
                }
                _ => return Some((left, i)),
            }
        }
    }
    let s = src.as_bytes();
    let (ast, end) = expr(s, 0)?;
    if ws(s, end) == s.len() {
        Some(ast)
    } else {
        None
    }
}

fn eval_sheet_arith(
    ast: &SheetArith,
    row: &SheetRow,
    formulas: &[(String, String)],
) -> Option<f64> {
    match ast {
        SheetArith::Num(n) => Some(*n),
        SheetArith::Field(name) => {
            let builtin = match name.as_str() {
                "state" => Some(SheetField::State),
                "priority" => Some(SheetField::Priority),
                "scheduled" => Some(SheetField::Scheduled),
                "deadline" => Some(SheetField::Deadline),
                "tags" => Some(SheetField::Tags),
                "page" => Some(SheetField::Page),
                _ => None,
            };
            let field = builtin.unwrap_or_else(|| SheetField::Prop(name.clone()));
            js_parse_float(&row.field_text(&field, formulas))
        }
        SheetArith::Neg(a) => Some(-eval_sheet_arith(a, row, formulas)?),
        SheetArith::Bin(op, a, b) => {
            let (a, b) = (
                eval_sheet_arith(a, row, formulas)?,
                eval_sheet_arith(b, row, formulas)?,
            );
            let v = match op {
                b'+' => a + b,
                b'-' => a - b,
                b'*' => a * b,
                _ => {
                    if b == 0.0 {
                        return None;
                    }
                    a / b
                }
            };
            if v.is_finite() {
                Some(v)
            } else {
                None
            }
        }
    }
}

// ---- Aggregates (static port of src/sheet/aggregate.ts) ----

/// Leading-number parse with JS `parseFloat` prefix semantics ("8abc" → 8).
fn js_parse_float(text: &str) -> Option<f64> {
    let s = text.trim_start();
    let bytes = s.as_bytes();
    let mut end = 0;
    let mut seen_digit = false;
    let mut seen_dot = false;
    while end < bytes.len() {
        match bytes[end] {
            b'0'..=b'9' => {
                seen_digit = true;
                end += 1;
            }
            b'.' if !seen_dot => {
                seen_dot = true;
                end += 1;
            }
            b'+' | b'-' if end == 0 => end += 1,
            b'e' | b'E' if seen_digit => {
                let mut j = end + 1;
                if matches!(bytes.get(j), Some(b'+') | Some(b'-')) {
                    j += 1;
                }
                let digits = j;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j == digits {
                    break;
                }
                end = j;
                break;
            }
            _ => break,
        }
    }
    if !seen_digit {
        return None;
    }
    s[..end].parse::<f64>().ok().filter(|n| n.is_finite())
}

/// `Math.round(n * 1000) / 1000` with JS number-string formatting (`-0` → 0).
fn format_sheet_number(n: f64) -> String {
    let rounded = (n * 1000.0).round() / 1000.0;
    let rounded = if rounded == 0.0 { 0.0 } else { rounded };
    format!("{rounded}")
}

/// `^<?(\d{4}-\d{2}-\d{2})` — the TS `isoDatePrefix`.
fn sheet_iso_date_prefix(text: &str) -> Option<&str> {
    let s = text.strip_prefix('<').unwrap_or(text);
    let b = s.as_bytes();
    if b.len() >= 10
        && b[0..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
    {
        Some(&s[..10])
    } else {
        None
    }
}

fn sheet_is_checked_text(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "done" | "true" | "yes" | "checked" | "x" | "[x]"
    )
}

fn sheet_with_skipped(text: String, skipped: usize) -> String {
    if skipped == 0 {
        return text;
    }
    if text.is_empty() {
        format!("({skipped} skipped)")
    } else {
        format!("{text} ({skipped} skipped)")
    }
}

fn sheet_aggregate_numbers(f: &str, values: &[String]) -> String {
    let mut skipped = 0;
    let mut nums: Vec<f64> = Vec::new();
    for value in values {
        match js_parse_float(value) {
            Some(n) => nums.push(n),
            None => skipped += 1,
        }
    }
    if nums.is_empty() {
        return sheet_with_skipped("0".into(), skipped);
    }
    nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let v = match f {
        "sum" => nums.iter().sum::<f64>(),
        "average" => nums.iter().sum::<f64>() / nums.len() as f64,
        "median" => {
            let mid = nums.len() / 2;
            if nums.len() % 2 == 1 {
                nums[mid]
            } else {
                (nums[mid - 1] + nums[mid]) / 2.0
            }
        }
        "min" => nums[0],
        "max" => nums[nums.len() - 1],
        "range" => nums[nums.len() - 1] - nums[0],
        _ => {
            let mean = nums.iter().sum::<f64>() / nums.len() as f64;
            let variance =
                nums.iter().map(|n| (n - mean) * (n - mean)).sum::<f64>() / nums.len() as f64;
            variance.sqrt()
        }
    };
    sheet_with_skipped(format_sheet_number(v), skipped)
}

fn sheet_aggregate_dates(f: &str, values: &[String]) -> String {
    let mut skipped = 0;
    let mut dates: Vec<&str> = Vec::new();
    for value in values {
        match sheet_iso_date_prefix(value.trim()) {
            Some(d) => dates.push(d),
            None => skipped += 1,
        }
    }
    dates.sort_unstable();
    if dates.is_empty() {
        return sheet_with_skipped(String::new(), skipped);
    }
    match f {
        "earliest" => sheet_with_skipped(dates[0].into(), skipped),
        "latest" => sheet_with_skipped(dates[dates.len() - 1].into(), skipped),
        _ => sheet_with_skipped(
            format!("{} - {}", dates[0], dates[dates.len() - 1]),
            skipped,
        ),
    }
}

fn sheet_aggregate(f: &str, values: &[String]) -> String {
    match f {
        "empty" => format!("{}", values.iter().filter(|v| v.trim().is_empty()).count()),
        "filled" | "count" => format!("{}", values.iter().filter(|v| !v.trim().is_empty()).count()),
        "unique" => {
            let set: HashSet<&str> = values
                .iter()
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
                .collect();
            format!("{}", set.len())
        }
        "checked" => format!(
            "{}",
            values.iter().filter(|v| sheet_is_checked_text(v)).count()
        ),
        "unchecked" => format!(
            "{}",
            values.iter().filter(|v| !sheet_is_checked_text(v)).count()
        ),
        "earliest" | "latest" => sheet_aggregate_dates(f, values),
        "range" => {
            let has_date = values
                .iter()
                .any(|v| sheet_iso_date_prefix(v.trim()).is_some());
            let numeric_non_dates = values
                .iter()
                .filter(|v| {
                    sheet_iso_date_prefix(v.trim()).is_none() && js_parse_float(v).is_some()
                })
                .count();
            if has_date && numeric_non_dates == 0 {
                sheet_aggregate_dates(f, values)
            } else {
                sheet_aggregate_numbers(f, values)
            }
        }
        _ => sheet_aggregate_numbers(f, values),
    }
}

// ---- Column derivation + rendering ----

/// Fields observed in the row data, mirroring `fieldIdsForBlocks`: builtins
/// that occur at all, in fixed order; then non-hidden property keys in
/// first-seen order; the Page column only for query-backed row sets.
fn sheet_observed_fields(
    rows: &[SheetRow],
    query_backed: bool,
    user_hidden: &[String],
) -> Vec<SheetField> {
    let mut out = Vec::new();
    let mut props: Vec<SheetField> = Vec::new();
    let mut prop_seen: HashSet<String> = HashSet::new();
    let (mut state, mut priority, mut scheduled, mut deadline, mut tags) =
        (false, false, false, false, false);
    for row in rows {
        let b = row.block;
        state |= b.marker().is_some();
        priority |= b.priority().is_some();
        scheduled |= b.scheduled().is_some();
        deadline |= b.deadline().is_some();
        tags |= !b.tags().is_empty();
        for (key, _) in b.properties() {
            if sheet_hidden_key(&key, user_hidden) {
                continue;
            }
            let norm = crate::doc::property_key_norm(&key);
            if prop_seen.insert(norm) {
                props.push(SheetField::Prop(key));
            }
        }
    }
    if state {
        out.push(SheetField::State);
    }
    if priority {
        out.push(SheetField::Priority);
    }
    if scheduled {
        out.push(SheetField::Scheduled);
    }
    if deadline {
        out.push(SheetField::Deadline);
    }
    if tags {
        out.push(SheetField::Tags);
    }
    out.append(&mut props);
    if query_backed {
        out.push(SheetField::Page);
    }
    out
}

/// One `tine.columns` token as a sheet column (P5A). The six sheet builtins
/// keep their identity; every other string is an ordinary property name, so it
/// maps to `prop:<name>` HERE, at the renderer — the property bytes stay the
/// bare name the author wrote, in both formats and in both languages.
fn sheet_field_for_column(name: &str) -> SheetField {
    match name {
        "state" => SheetField::State,
        "priority" => SheetField::Priority,
        "scheduled" => SheetField::Scheduled,
        "deadline" => SheetField::Deadline,
        "tags" => SheetField::Tags,
        "page" => SheetField::Page,
        _ => SheetField::Prop(name.into()),
    }
}

/// A CANONICAL grouping `FieldId` as a sheet column identity (P5B). The
/// resolver has already decided what the token means, so this is a pure
/// spelling map with no fallback of its own — the `prop:`/`formula:` prefixes
/// are guaranteed to carry a nonempty name, and anything else is one of the six
/// builtins. Mirrors `src/sheet/fields.ts::isFieldId`'s reading.
fn sheet_field_for_group(field: &str) -> SheetField {
    match field {
        "state" => SheetField::State,
        "priority" => SheetField::Priority,
        "scheduled" => SheetField::Scheduled,
        "deadline" => SheetField::Deadline,
        "tags" => SheetField::Tags,
        "page" => SheetField::Page,
        other => match other.strip_prefix("formula:") {
            Some(name) => SheetField::Formula(name.into()),
            None => SheetField::Prop(other.strip_prefix("prop:").unwrap_or(other).into()),
        },
    }
}

/// The static `columns` memo: title (implicit) + declared fields + formula
/// fields + observed fields not already present — in that order.
///
/// A QUERY-backed face then applies the block's `tine.columns` selection over
/// that list — **after** the schema/type lookup, so a selected column keeps the
/// checkbox rendering and enum order its `tine.fields` declaration gave it, and
/// a field definition is never lost merely because its column is not shown. A
/// selected column that no row carries is kept and renders empty cells; the
/// implicit title column and the row anchors are outside the selection and stay
/// reachable. No selection (absent, or an explicit empty/invalid one) leaves the
/// default column set exactly as it is today.
fn sheet_columns(
    cfg: &SheetConfig,
    rows: &[SheetRow],
    query_backed: bool,
    user_hidden: &[String],
) -> Vec<(SheetField, bool)> {
    let mut out: Vec<(SheetField, bool)> = Vec::new();
    let mut ids: HashSet<String> = HashSet::new();
    for decl in &cfg.declared {
        if ids.insert(decl.field.id()) {
            out.push((decl.field.clone(), decl.checkbox));
        }
    }
    for (name, _) in &cfg.formulas {
        let field = SheetField::Formula(name.clone());
        if ids.insert(field.id()) {
            out.push((field, false));
        }
    }
    for field in sheet_observed_fields(rows, query_backed, user_hidden) {
        if ids.insert(field.id()) {
            out.push((field, false));
        }
    }
    if !query_backed {
        return out;
    }
    let crate::query::view::QueryColumns::Named(selection) = &cfg.columns else {
        return out;
    };
    let mut selected: Vec<(SheetField, bool)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for column in selection {
        let field = sheet_field_for_column(column.as_str());
        let id = field.id();
        if !seen.insert(id.clone()) {
            continue;
        }
        match out.iter().find(|(known, _)| known.id() == id) {
            Some((known, checkbox)) => selected.push((known.clone(), *checkbox)),
            None => selected.push((field, false)),
        }
    }
    selected
}

/// Rendered cell for one row/field: header-facet chrome for state/priority,
/// tag/page links, inline-decorated property text (so `[[refs]]` and `#tags`
/// stay links), checkbox glyphs for declared checkbox fields, computed formula
/// numbers.
fn sheet_cell_html(
    row: &SheetRow,
    field: &SheetField,
    checkbox: bool,
    cfg: &SheetConfig,
    ctx: &Ctx,
) -> String {
    let b = row.block;
    match field {
        SheetField::State => {
            let mut cell = String::new();
            if b.marker().is_some() {
                emit_header_facets(b.marker(), None, &mut cell);
            }
            cell
        }
        SheetField::Priority => {
            let mut cell = String::new();
            if b.priority().is_some() {
                emit_header_facets(None, b.priority(), &mut cell);
            }
            cell
        }
        SheetField::Tags => b
            .tags()
            .iter()
            .map(|t| {
                format!(
                    "<a class=\"tag\" href=\"{}.html\">#{}</a>",
                    page_slug(ctx, t),
                    esc(t)
                )
            })
            .collect::<Vec<_>>()
            .join(" "),
        SheetField::Page => {
            let page = row.page.unwrap_or("");
            if page.is_empty() {
                String::new()
            } else {
                format!(
                    "<a class=\"ref\" href=\"{}.html\">{}</a>",
                    page_slug(ctx, page),
                    esc(page)
                )
            }
        }
        SheetField::Formula(_) => {
            let text = row.field_text(field, &cfg.formulas);
            esc(&text)
        }
        SheetField::Scheduled | SheetField::Deadline => esc(&row.field_text(field, &cfg.formulas)),
        SheetField::Prop(_) => {
            if checkbox {
                let text = row.field_text(field, &cfg.formulas);
                return if sheet_is_checked_text(&text) {
                    "<span class=\"task-checkbox checked\"></span>".into()
                } else {
                    "<span class=\"task-checkbox\"></span>".into()
                };
            }
            let text = row.field_text(field, &cfg.formulas);
            decorate(&lsdoc::render_html(&body_blocks(&text), &md_opts()), ctx, 0)
        }
    }
}

/// Anchor + search-index entry for a sheet row/card, in the SAME emission
/// lock-step as top-level blocks (a row's `<tr id>`/`<li id>` is the
/// deep-link target for its search hit).
fn sheet_row_anchor(
    b: &DocBlock,
    counter: &mut u32,
    index: &mut Vec<serde_json::Value>,
    slug: &str,
    title: &str,
) -> String {
    let anchor = match block_id(&b.raw) {
        Some(id) => id,
        None => {
            let a = format!("b{}", *counter);
            *counter += 1;
            a
        }
    };
    let text = ast_plain_text(&body_blocks(&b.raw));
    if !text.is_empty() {
        index.push(json!({"slug": slug, "title": title, "anchor": anchor, "text": text}));
    }
    anchor
}

/// Everything the sheet renderers need from the outer render pass.
struct SheetEmit<'a, 'b> {
    ctx: &'a Ctx<'b>,
    slug: &'a str,
    title: &'a str,
    opts: PrintOpts,
    /// Whether rows get stable anchors + index entries (children-backed rows
    /// of this page). Query-hydrated rows mirror query results: no anchors.
    anchors: bool,
}

fn render_sheet_table(
    cfg: &SheetConfig,
    rows: &[SheetRow],
    emit: &SheetEmit,
    query_backed: bool,
    counter: &mut u32,
    index: &mut Vec<serde_json::Value>,
    out: &mut String,
) {
    let user_hidden: &[String] = match emit.ctx.graph {
        Some(graph) => &graph.config.block_hidden_properties,
        None => &[],
    };
    let columns = sheet_columns(cfg, rows, query_backed, user_hidden);
    out.push_str("<div class=\"sheet-scroll\"><table class=\"sheet-table\"><thead><tr><th></th>");
    for (field, _) in &columns {
        let mut label = field.label().to_string();
        // A formula column the static subset cannot evaluate keeps its
        // definition visible instead of computing a wrong value.
        if let SheetField::Formula(name) = field {
            let expr = cfg
                .formulas
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, e)| e.as_str())
                .unwrap_or("");
            if parse_sheet_arith(expr).is_none() && !expr.is_empty() {
                label = format!("{} ({})", name, expr);
            }
        }
        out.push_str(&format!("<th>{}</th>", esc(&label)));
    }
    out.push_str("</tr></thead>");
    let aggregate = !cfg.aggregates.is_empty();
    if aggregate {
        out.push_str("<tfoot><tr><td></td>");
        for (field, _) in &columns {
            let target = cfg
                .aggregates
                .iter()
                .find(|(key, _)| key == &field.id() || key == &field.label());
            match target {
                Some((_, f)) => {
                    let values: Vec<String> = rows
                        .iter()
                        .map(|row| row.field_text(field, &cfg.formulas))
                        .collect();
                    out.push_str(&format!(
                        "<td><span class=\"sheet-agg-label\">{}</span> {}</td>",
                        esc(sheet_aggregate_label(f)),
                        esc(&sheet_aggregate(f, &values))
                    ));
                }
                None => out.push_str("<td></td>"),
            }
        }
        out.push_str("</tr></tfoot>");
    }
    out.push_str("<tbody>");
    for row in rows {
        if emit.anchors {
            let anchor = sheet_row_anchor(row.block, counter, index, emit.slug, emit.title);
            out.push_str(&format!("<tr id=\"{}\">", esc_attr(&anchor)));
        } else {
            out.push_str("<tr>");
        }
        // Title cell: the row's own content (facets + inline formatting),
        // with any sub-bullets kept as a nested outline.
        out.push_str("<td>");
        let mut title_cell = String::new();
        emit_header_facets(row.block.marker(), row.block.priority(), &mut title_cell);
        title_cell.push_str(&decorate(
            &lsdoc::render_html(&body_blocks(&row.block.raw), &md_opts()),
            emit.ctx,
            0,
        ));
        out.push_str(&title_cell);
        if !row.block.children.is_empty() {
            out.push_str("<ul class=\"sheet-children\">");
            for c in &row.block.children {
                render_block(
                    c, out, emit.ctx, emit.slug, emit.title, counter, index, emit.opts,
                );
            }
            out.push_str("</ul>");
        }
        out.push_str("</td>");
        for (field, checkbox) in &columns {
            out.push_str(&format!(
                "<td>{}</td>",
                sheet_cell_html(row, field, *checkbox, cfg, emit.ctx)
            ));
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table></div>");
}

/// Board column order, mirroring `buildColumns` (SheetBoard.tsx): state →
/// workflow order + other present markers; priority → A/B/C; enum → declared
/// values then extras; tags/other fields → first-seen; null group last.
fn render_sheet_board(
    cfg: &SheetConfig,
    rows: &[SheetRow],
    emit: &SheetEmit,
    workflow: crate::Workflow,
    query_backed: bool,
    counter: &mut u32,
    index: &mut Vec<serde_json::Value>,
    out: &mut String,
) {
    // **A query board asks the ONE grouping resolver; an ordinary sheet board is
    // untouched** (P5B). `tine.group-field::` and the view-aware reinterpretation
    // of the legacy token are a query-face question — a children-backed board
    // keeps reading `tine.group-by::` exactly as it does today, so nothing about
    // an ordinary published sheet changes.
    //
    // `None` here is an explicit "no grouping": one ungrouped column, never a
    // silently reinstated `state` default.
    let group_field: Option<SheetField> = if query_backed {
        match &cfg.grouping {
            crate::query::view::QueryGrouping::Field(field) => {
                Some(sheet_field_for_group(field.as_str()))
            }
            crate::query::view::QueryGrouping::Cleared => None,
            crate::query::view::QueryGrouping::Unset => Some(SheetField::State),
        }
    } else {
        let raw = cfg.group_by.as_deref().unwrap_or("state");
        let norm = crate::doc::property_key_norm(raw);
        Some(match norm.as_str() {
            "state" => SheetField::State,
            "priority" => SheetField::Priority,
            "scheduled" => SheetField::Scheduled,
            "deadline" => SheetField::Deadline,
            "tags" => SheetField::Tags,
            "page" => SheetField::Page,
            other => {
                if let Some(name) = other.strip_prefix("prop:") {
                    SheetField::Prop(name.into())
                } else if let Some(name) = other.strip_prefix("formula:") {
                    SheetField::Formula(name.into())
                } else {
                    SheetField::Prop(raw.into())
                }
            }
        })
    };
    // Group keys per row (tags give multi-membership).
    let keys_for = |row: &SheetRow| -> Vec<Option<String>> {
        let Some(group_field) = &group_field else {
            // Explicitly ungrouped: every row lands in the single column.
            return vec![None];
        };
        match group_field {
            SheetField::Tags => {
                let tags = row.block.tags();
                if tags.is_empty() {
                    vec![None]
                } else {
                    tags.into_iter().map(Some).collect()
                }
            }
            SheetField::Formula(name) => {
                let text = row.field_text(&SheetField::Formula(name.clone()), &cfg.formulas);
                vec![if text.is_empty() { None } else { Some(text) }]
            }
            field => {
                let text = row.field_text(field, &cfg.formulas);
                vec![if text.trim().is_empty() {
                    None
                } else {
                    Some(text)
                }]
            }
        }
    };
    let mut buckets: Vec<(Option<String>, Vec<&SheetRow>)> = Vec::new();
    let mut first_keys: Vec<Option<String>> = Vec::new();
    for row in rows {
        for key in keys_for(row) {
            if !first_keys.contains(&key) {
                first_keys.push(key.clone());
            }
            match buckets.iter_mut().find(|(k, _)| k == &key) {
                Some((_, bucket)) => bucket.push(row),
                None => buckets.push((key, vec![row])),
            }
        }
    }
    let has_null = first_keys.contains(&None);
    let order: Vec<Option<String>> = match group_field.as_ref() {
        None => Vec::new(),
        Some(SheetField::State) => {
            let standard: [&str; 3] = match workflow {
                crate::Workflow::Todo => ["TODO", "DOING", "DONE"],
                crate::Workflow::Now => ["LATER", "NOW", "DONE"],
            };
            let mut order: Vec<Option<String>> =
                standard.iter().map(|m| Some(m.to_string())).collect();
            for m in crate::doc::MARKERS {
                let key = Some((*m).to_string());
                if standard.contains(m) || !first_keys.contains(&key) {
                    continue;
                }
                order.push(key);
            }
            order
        }
        Some(SheetField::Priority) => ["A", "B", "C"]
            .iter()
            .map(|m| Some((*m).to_string()))
            .collect(),
        Some(SheetField::Prop(name)) => {
            // A declared enum order wins; first-seen extras follow.
            let norm = crate::doc::property_key_norm(name);
            let mut order: Vec<Option<String>> = cfg
                .declared
                .iter()
                .find(|decl| {
                    matches!(&decl.field, SheetField::Prop(p)
                        if crate::doc::property_key_norm(p) == norm)
                })
                .map(|decl| decl.enum_values.iter().map(|v| Some(v.clone())).collect())
                .unwrap_or_default();
            for key in &first_keys {
                if key.is_some() && !order.contains(key) {
                    order.push(key.clone());
                }
            }
            order
        }
        Some(_) => {
            let mut order: Vec<Option<String>> = Vec::new();
            for key in &first_keys {
                if key.is_some() && !order.contains(key) {
                    order.push(key.clone());
                }
            }
            order
        }
    };
    let mut order = order;
    if has_null && !order.contains(&None) {
        order.push(None);
    }
    if order.is_empty() {
        order.push(None);
    }
    out.push_str("<div class=\"sheet-board\">");
    for key in &order {
        let label = match key {
            None => "(none)".to_string(),
            Some(k) if matches!(group_field, Some(SheetField::Priority)) => format!("[#{k}]"),
            Some(k) => k.clone(),
        };
        out.push_str(&format!(
            "<div class=\"sheet-board-col\"><h3>{}</h3><ul>",
            esc(&label)
        ));
        let cards: &[&SheetRow] = buckets
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, rows)| rows.as_slice())
            .unwrap_or(&[]);
        for card in cards {
            if emit.anchors {
                let anchor = sheet_row_anchor(card.block, counter, index, emit.slug, emit.title);
                out.push_str(&format!("<li id=\"{}\">", esc_attr(&anchor)));
                emit_block_inner(&card.block.raw, out, emit.ctx, 0);
                if !card.block.children.is_empty() {
                    out.push_str("<ul>");
                    for c in &card.block.children {
                        render_block(
                            c, out, emit.ctx, emit.slug, emit.title, counter, index, emit.opts,
                        );
                    }
                    out.push_str("</ul>");
                }
                out.push_str("</li>");
            } else {
                render_embedded_block(card.block, out, emit.ctx, 0);
            }
        }
        out.push_str("</ul></div>");
    }
    out.push_str("</div>");
}

/// The positional grid: each child is a row, each row's children are cells.
/// `tine.header:: true` promotes the first row to `<th>` cells; a cell that is
/// itself a sheet owner renders as a nested sheet (depth-bounded).
fn render_sheet_grid(
    cfg: &SheetConfig,
    children: &[DocBlock],
    emit: &SheetEmit,
    counter: &mut u32,
    index: &mut Vec<serde_json::Value>,
    depth: u8,
    out: &mut String,
) {
    out.push_str("<div class=\"sheet-scroll\"><table class=\"sheet-grid\">");
    let mut rows = children.iter();
    if cfg.header {
        if let Some(head) = rows.next() {
            out.push_str("<thead><tr>");
            for cell in &head.children {
                push_grid_cell(cell, emit, counter, index, depth, true, out);
            }
            if head.children.is_empty() {
                out.push_str("<th></th>");
            }
            out.push_str("</tr></thead>");
        }
    }
    out.push_str("<tbody>");
    for row in rows {
        out.push_str("<tr>");
        if row.children.is_empty() {
            out.push_str("<td></td>");
        }
        for cell in &row.children {
            push_grid_cell(cell, emit, counter, index, depth, false, out);
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table></div>");
}

fn push_grid_cell(
    cell: &DocBlock,
    emit: &SheetEmit,
    counter: &mut u32,
    index: &mut Vec<serde_json::Value>,
    depth: u8,
    header: bool,
    out: &mut String,
) {
    let tag = if header { "th" } else { "td" };
    // Keep search coverage + anchors for cells of the host page.
    let anchor = sheet_row_anchor(cell, counter, index, emit.slug, emit.title);
    out.push_str(&format!("<{tag} id=\"{}\">", esc_attr(&anchor)));
    let nested = sheet_config(&cell.properties());
    match &nested {
        Some(nested_cfg) if !cell.children.is_empty() && depth < 4 => {
            // The cell's own line is the nested view's label; render it above
            // the nested sheet inside the same cell.
            let body = decorate(
                &lsdoc::render_html(&body_blocks(&cell.raw), &md_opts()),
                emit.ctx,
                depth,
            );
            if !body.is_empty() {
                out.push_str(&format!("<div class=\"sheet-cell-label\">{body}</div>"));
            }
            render_children_sheet(
                nested_cfg,
                &cell.children,
                emit,
                counter,
                index,
                depth + 1,
                out,
            );
        }
        _ => {
            let body = decorate(
                &lsdoc::render_html(&body_blocks(&cell.raw), &md_opts()),
                emit.ctx,
                depth,
            );
            // An empty cell's lsdoc skeleton renders as a bare `<br>` — keep
            // the static cell honestly empty instead.
            if body.trim() != "<br>" && !body.trim().is_empty() {
                out.push_str(&body);
            }
        }
    }
    out.push_str(&format!("</{tag}>"));
}

/// Children-backed sheet dispatch for `render_block`.
fn render_children_sheet(
    cfg: &SheetConfig,
    children: &[DocBlock],
    emit: &SheetEmit,
    counter: &mut u32,
    index: &mut Vec<serde_json::Value>,
    depth: u8,
    out: &mut String,
) {
    match cfg.view {
        SheetView::Table => {
            let rows: Vec<SheetRow> = children
                .iter()
                .map(|block| SheetRow { block, page: None })
                .collect();
            render_sheet_table(cfg, &rows, emit, false, counter, index, out);
        }
        SheetView::Board => {
            let rows: Vec<SheetRow> = children
                .iter()
                .map(|block| SheetRow { block, page: None })
                .collect();
            let workflow = emit
                .ctx
                .graph
                .map(|g| g.config.preferred_workflow)
                .unwrap_or(crate::Workflow::Now);
            render_sheet_board(cfg, &rows, emit, workflow, false, counter, index, out);
        }
        SheetView::Grid => render_sheet_grid(cfg, children, emit, counter, index, depth, out),
    }
}

/// A query-backed sheet: run the blocks-selection query through the shared
/// static executor, hydrate results from source pages, then present them as
/// the requested view instead of the flat query list.
fn render_query_sheet(
    graph: &Graph,
    cfg: &SheetConfig,
    macro_name: &str,
    query: &str,
    ctx: &Ctx,
    emit: &SheetEmit,
    out: &mut String,
) {
    let (input, check_nesting) = if macro_name.eq_ignore_ascii_case("tine-query") {
        (crate::query::QueryInput::MacroTql, false)
    } else {
        (crate::query::QueryInput::MacroQuery, true)
    };
    let outcome = match run_static_query(query, input, check_nesting, ctx) {
        Ok(outcome) => outcome,
        Err(html) => {
            out.push_str(&html);
            return;
        }
    };
    let _nested = NestedResultsGuard::enter(ctx);
    let mut render = |hydrated: &[HydratedQueryBlock<'_>]| {
        let rows = hydrated
            .iter()
            .map(|row| SheetRow {
                block: row.block,
                page: Some(row.page),
            })
            .collect::<Vec<_>>();
        let mut counter = 0u32;
        let mut sink = Vec::new(); // query results are not indexed (macro parity)
        match cfg.view {
            SheetView::Table => {
                render_sheet_table(cfg, &rows, emit, true, &mut counter, &mut sink, out)
            }
            SheetView::Board => render_sheet_board(
                cfg,
                &rows,
                emit,
                graph.config.preferred_workflow,
                true,
                &mut counter,
                &mut sink,
                out,
            ),
            SheetView::Grid => {
                out.push_str("<div class=\"query-unsupported\">A grid is positional; it cannot present query results.</div>");
            }
        }
    };
    match outcome.rows {
        StaticQueryRows::Block(groups) => with_hydrated_query_groups(graph, &groups, render),
        StaticQueryRows::Subtrees(roots) => {
            let documents = roots.iter().map(|root| {
                crate::model::dto_blocks_to_doc_checked(
                    std::slice::from_ref(&root.block),
                    crate::model::Format::from_path(Path::new(&root.path)) == crate::model::Format::Org,
                )
            }).collect::<io::Result<Vec<_>>>();
            match documents {
                Ok(documents) => {
                    let hydrated = roots.iter().zip(&documents).map(|(root, blocks)| HydratedQueryBlock {
                        page: &root.page, block: &blocks[0],
                    }).collect::<Vec<_>>();
                    render(&hydrated);
                }
                Err(error) => record_print_error(ctx, PrintPreparationError::Io(error)),
            }
        }
        StaticQueryRows::Page(_) => out.push_str("<div class=\"query-unsupported\" role=\"alert\">Page queries cannot be presented as block sheets.</div>"),
    }
}

/// The whole-block `{{query …}}` detection used for query-backed sheets: the
/// rendered body being exactly one macro element means the block IS the query
/// (its `tine.view` chooses the sheet presentation for the results).
/// The macro NAME when the rendered body is exactly one query macro element,
/// else `None`. The argument is not read here — §4.3.1 takes it from the raw
/// source, because `data-args` has already lost the options brace by this point.
fn whole_query_macro(rendered: &str) -> Option<String> {
    let t = rendered.trim();
    let inner = t.strip_prefix("<span ")?;
    let close = inner.find('>')?;
    let tag_inner = &inner[..close];
    if !has_class(tag_inner, "macro") {
        None?;
    }
    let name = tag_attr(tag_inner, "data-macro").map(unescape)?;
    // §7.9: both spellings are queries. A `{{tine-query …}}` block carrying
    // `tine.view:: board` is as much a sheet row source as a `{{query …}}` one.
    if !is_query_macro_name(&name) {
        None?;
    }
    if inner[close + 1..].trim() != "</span>" {
        None?;
    }
    Some(name)
}

/// Expand one query macro from its RAW argument (§4.3.1). Same bounds as
/// [`expand_macro`]; separate only because the argument arrives verbatim from the
/// block source rather than from lsdoc's comma-split `data-args`.
fn expand_query_macro(name: &str, argument: &str, ctx: &Ctx, depth: u8) -> String {
    let Some(graph) = ctx.graph else {
        return "<div class=\"query query-unsupported\" role=\"alert\">Query results are unavailable for this render.</div>".to_string();
    };
    if depth >= 4 {
        return format!("<span class=\"macro-raw\">{{{{{} …}}}}</span>", esc(name));
    }
    render_query_named(graph, name, argument, ctx, depth + 1)
}

/// Expand one `{{macro …}}`. Bounded by `depth` (a page can embed a block that embeds
/// a page …; a circular embed would otherwise loop). With no graph in context,
/// query macros refuse explicitly and other data macros drop.
fn expand_macro(name: &str, args: &[String], ctx: &Ctx, depth: u8) -> String {
    if depth >= 4 {
        return format!("<span class=\"macro-raw\">{{{{{} …}}}}</span>", esc(name));
    }
    let arg0 = args.first().map(|s| s.as_str()).unwrap_or("").trim();
    // §7.9: `{{tine-query …}}` renders through the same executor as `{{query …}}`.
    // A macro name this tree writes but cannot export would publish the author's
    // query as literal text (Y1) — which is exactly what this arm used to do.
    if is_query_macro_name(name) {
        return match ctx.graph {
            Some(graph) => render_query_named(graph, name, arg0, ctx, depth + 1),
            None => "<div class=\"query query-unsupported\" role=\"alert\">Query results are unavailable for this render.</div>".to_string(),
        };
    }
    let Some(graph) = ctx.graph else {
        return String::new();
    };
    match name {
        "embed" => render_embed(graph, arg0, ctx, depth + 1),
        "video" => render_video(arg0),
        "namespace" => render_namespace(graph, arg0, ctx),
        // Unknown / can't-render-statically macro → muted literal (better than a blank).
        _ => format!(
            "<span class=\"macro-raw\">{{{{{} {}}}}}</span>",
            esc(name),
            esc(&args.join(" "))
        ),
    }
}

/// Run a `{{query …}}` against the graph and render its results as a bordered block.
fn render_query(graph: &Graph, src: &str, ctx: &Ctx, depth: u8) -> String {
    render_query_with_title(graph, src, None, ctx, depth)
}

/// Render whichever query macro name the author wrote (§7.9, Y1).
///
/// `{{query …}}` keeps the legacy OG/advanced string path, unchanged. A
/// `{{tine-query …}}` argument is TQL, which that path cannot read at all, so it
/// goes through the shared parser and supplied reader. Publishing a macro name
/// this tree can write but not export is the
/// failure Y1 names, and it is why this arm exists.
fn render_query_named(graph: &Graph, name: &str, argument: &str, ctx: &Ctx, depth: u8) -> String {
    if !macro_name_is_tql(name) {
        return render_query(graph, argument, ctx, depth);
    }
    render_tql_query(graph, argument, ctx, depth)
}

/// Whether one authored query macro uses the TQL grammar. Both spellings route
/// through the same supplied reader after their grammar-specific parse.
fn macro_name_is_tql(name: &str) -> bool {
    name.eq_ignore_ascii_case("tine-query")
}

/// The `{{tine-query …}}` static-export path: parse the COMPLETE raw argument
/// with the one Rust splitter, then hand the IR to the publication boundary's
/// current-main reader.
fn render_tql_query(graph: &Graph, argument: &str, ctx: &Ctx, depth: u8) -> String {
    let outcome = match run_static_query(argument, crate::query::QueryInput::MacroTql, false, ctx) {
        Ok(outcome) => outcome,
        Err(html) => return html,
    };
    render_query_outcome(graph, outcome, None, ctx, depth)
}

/// The ONE static-export query executor, shared by the `{{query …}}` macro
/// renderer, TQL, `BEGIN_QUERY`, and query-backed sheet views. It parses each
/// use independently and routes every valid IR through the supplied coherent
/// reader. `Err` is the user-facing failure HTML.
fn run_static_query(
    src: &str,
    input: crate::query::QueryInput,
    check_nesting: bool,
    ctx: &Ctx,
) -> Result<StaticQueryOutcome, String> {
    run_static_query_with(src, input, check_nesting, ctx, QueryRunOverrides::default())
}

/// What one caller knows better than the render scope (Stage 2): the export's
/// own home query keeps its REVIEWED binding and view while the record's
/// lookup key stays the home page.
#[derive(Default)]
struct QueryRunOverrides {
    context: Option<ExecutionContext>,
    view: Option<crate::query::ir::ViewSettings>,
    properties: Option<Vec<(String, String)>>,
    host_block_id: Option<String>,
}

/// While query RESULT rows render, a macro inside them belongs to another
/// page's context and is not one the app will ask for: it is not recorded.
struct NestedResultsGuard<'a>(Option<&'a RefCell<app_export::RenderScope>>);

impl<'a> NestedResultsGuard<'a> {
    fn enter(ctx: &Ctx<'a>) -> Self {
        if let Some(scope) = ctx.scope {
            let mut scope = scope.borrow_mut();
            scope.nesting = scope.nesting.saturating_add(1);
        }
        NestedResultsGuard(ctx.scope)
    }
}

impl Drop for NestedResultsGuard<'_> {
    fn drop(&mut self) {
        if let Some(scope) = self.0 {
            let mut scope = scope.borrow_mut();
            scope.nesting = scope.nesting.saturating_sub(1);
        }
    }
}

fn run_static_query_with(
    src: &str,
    input: crate::query::QueryInput,
    check_nesting: bool,
    ctx: &Ctx,
    overrides: QueryRunOverrides,
) -> Result<StaticQueryOutcome, String> {
    const STATIC_QUERY_MAX_ROWS: usize = 20_000;
    const STATIC_QUERY_MAX_BYTES: usize = 32 * 1024 * 1024;
    if !crate::query::query_source_within_limit(src) {
        record_print_error(ctx, PrintPreparationError::Budget(PRINT_SOURCE_REFUSAL));
        return Err(format!(
            "<div class=\"query query-too-large\">Query source exceeds the {} KiB publication limit.</div>",
            crate::query::QUERY_SOURCE_MAX_BYTES / 1024
        ));
    }
    if check_nesting && !crate::query::query_nesting_within_limit(src) {
        record_print_error(ctx, PrintPreparationError::Budget(PRINT_NESTING_REFUSAL));
        return Err("<div class=\"query query-too-large\">Query nesting is too deep to publish safely.</div>".to_string());
    }
    // Publication parsing has no suggestion UI. As before, an empty registry
    // affects only suggestions; the supplied reader owns lowering against its
    // captured property registry.
    let registry =
        crate::query::registry::Registry::from_snapshot(&crate::query::ir::RegistrySnapshot {
            rows: Vec::new(),
            generation: 0,
        });
    // A query export (Stage 2) runs each query exactly as the app will ask
    // for it: `<% current page %>` bound to the page being rendered, the host
    // block's `tine.*` properties merged into the view, the page as the
    // execution context. The graph site and print keep their historical
    // context-free run.
    let scoped = ctx.scope.map(|scope| scope.borrow().clone());
    let mut record: Option<app_export::QuerySnapshot> = None;
    let (query, view, context) = match &scoped {
        Some(scope) => {
            let dialect = crate::query::wire_parse::QueryTextDialect::from_input(input);
            let properties = overrides
                .properties
                .clone()
                .unwrap_or_else(|| scope.block_properties.clone());
            let authored =
                crate::query::wire_parse::parse_query_pair(src, dialect, &properties, &registry);
            let execution = scope
                .page
                .as_deref()
                .and_then(|page| app_export::substitute_current_page(src, page))
                .map(|argument| {
                    let parsed = crate::query::wire_parse::parse_query_pair(
                        &argument,
                        dialect,
                        &properties,
                        &registry,
                    );
                    app_export::QueryExecutionParse { argument, parsed }
                });
            let effective = execution
                .as_ref()
                .map(|execution| &execution.parsed)
                .unwrap_or(&authored);
            let query = effective.query.clone();
            // The app runs a query under the view of its result kind — the
            // Pages section under the page-scoped settings, Blocks under the
            // block-scoped ones (`Macro.tsx` `pageResultView`/`blockResultView`).
            // Baking the singular merged view instead would record an
            // unsampled answer for a `tine.block-sample::` host.
            let view = overrides.view.clone().unwrap_or_else(|| {
                crate::query::wire_parse::anchored_view(effective, effective.query.anchor)
            });
            let lookup_context = match &scope.page {
                Some(page) => ExecutionContext::on_page(page.clone()),
                None => ExecutionContext::none(),
            };
            let context = overrides
                .context
                .clone()
                .unwrap_or_else(|| lookup_context.clone());
            if ctx.recorder.is_some() && scope.nesting == 0 {
                record = Some(app_export::QuerySnapshot {
                    host: scope.page.clone().unwrap_or_default(),
                    argument: src.to_string(),
                    dialect,
                    properties,
                    parsed: authored,
                    execution,
                    context: lookup_context,
                    executed_context: context.clone(),
                    view: view.clone(),
                    result: app_export::closed_result(
                        QueryRows::Page { pages: Vec::new() },
                        Vec::new(),
                        crate::query::ir::QueryReport::default(),
                    ),
                });
            }
            (query, view, context)
        }
        None => {
            let (query, view) = crate::query::parse_query_input(
                src,
                input,
                crate::date::JournalDate::today(),
                &registry,
            );
            (query, view, ExecutionContext::none())
        }
    };
    // Advanced source is intentionally unresolved at parse time. The shared
    // reader resolves it before deciding which diagnostics are actionable.
    let Some(reader) = ctx.query_reader else {
        return Err("<div class=\"query query-unsupported\" role=\"alert\">Query results are unavailable for this render.</div>".to_string());
    };
    // A published page is not the author's current editor page. Existing
    // advanced `?current-page` binding therefore remains absent.
    if ctx.print_error.is_some() && query.anchor == crate::query::ir::Anchor::Page {
        return Err("<div class=\"query query-unsupported\" role=\"alert\">Page query results are unavailable in Print.</div>".into());
    }
    let bounds = Bounds {
        max_rows: STATIC_QUERY_MAX_ROWS,
        max_bytes: STATIC_QUERY_MAX_BYTES,
    };
    let (result, subtrees) = if ctx.print_error.is_some() {
        reader
            .run_subtrees(&query, &view, bounds, &context)
            .map(|answer| (answer.result, Some(answer.roots)))
    } else {
        reader
            .run(&query, &view, bounds, &context)
            .map(|result| (result, None))
    }
    .map_err(|error| {
        record_print_error(ctx, PrintPreparationError::Query(error));
        format!(
            "<div class=\"query query-unsupported\" role=\"alert\">{}</div>",
            esc(&error.to_string())
        )
    })?;
    if !result.report.supported
        || result
            .diagnostics
            .iter()
            .any(|diagnostic| !diagnostic.disabled)
    {
        return Err(
            "<div class=\"query query-unsupported\" role=\"alert\">Unsupported query.</div>"
                .to_string(),
        );
    }
    if result.exceeded {
        record_print_error(ctx, PrintPreparationError::Budget(PRINT_SELECTION_REFUSAL));
        return Err(format!(
            "<div class=\"query query-too-large\">Query has {} matches; narrow it before publishing.</div>",
            result.matched_total.unwrap_or(result.total)
        ));
    }
    let sampled = view.sample.is_some();
    if let Some(roots) = subtrees {
        return Ok(StaticQueryOutcome {
            pre_filter_total: roots.len(),
            rows: StaticQueryRows::Subtrees(roots),
            sampled,
        });
    }
    let QueryResult {
        rows,
        diagnostics,
        report,
        ..
    } = result;
    let outcome = match rows {
        QueryRows::Block { groups } => {
            let pre_filter_total = groups.iter().map(|group| group.blocks.len()).sum();
            let groups: Vec<RefGroup> = groups
                .into_iter()
                .filter(|group| publish_page_allowed(ctx, &group.page))
                .collect();
            let groups = match overrides.host_block_id.as_deref() {
                Some(host) => query_export::without_host_block(groups, Some(host)),
                None => groups,
            };
            if let Some(record) = record.as_mut() {
                record.result = app_export::closed_result(
                    QueryRows::Block {
                        groups: groups.clone(),
                    },
                    diagnostics,
                    report,
                );
            }
            StaticQueryOutcome {
                rows: StaticQueryRows::Block(groups),
                pre_filter_total,
                sampled,
            }
        }
        QueryRows::Page { pages } => {
            let pre_filter_total = pages.len();
            let pages: Vec<PageRow> = pages
                .into_iter()
                .filter(|page| publication_page_link(ctx, page).is_some())
                .collect();
            if let Some(record) = record.as_mut() {
                record.result = app_export::closed_result(
                    QueryRows::Page {
                        pages: pages.clone(),
                    },
                    diagnostics,
                    report,
                );
            }
            StaticQueryOutcome {
                rows: StaticQueryRows::Page(pages),
                pre_filter_total,
                sampled,
            }
        }
    };
    if let (Some(record), Some(recorder)) = (record, ctx.recorder) {
        recorder.borrow_mut().queries.push(record);
    }
    Ok(outcome)
}

enum StaticQueryRows {
    Block(Vec<RefGroup>),
    Page(Vec<PageRow>),
    Subtrees(Vec<crate::query::export_results::HydratedRoot>),
}

fn record_print_error(ctx: &Ctx, error: PrintPreparationError) {
    if let Some(slot) = ctx.print_error {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(error);
        }
    }
}

struct StaticQueryOutcome {
    rows: StaticQueryRows,
    pre_filter_total: usize,
    /// The view sampled BEFORE the rows were filtered to the publication, so
    /// the drawn set was influenced by content outside it.
    sampled: bool,
}

fn render_query_with_title(
    graph: &Graph,
    src: &str,
    title: Option<&str>,
    ctx: &Ctx,
    depth: u8,
) -> String {
    let outcome = match run_static_query(src, crate::query::QueryInput::MacroQuery, true, ctx) {
        Ok(outcome) => outcome,
        Err(html) => return html,
    };
    render_query_outcome(graph, outcome, title, ctx, depth)
}

/// The shared result chrome: count, rows, and the non-public omission notice.
/// Both macro names reach it, so a TQL export and an OG export are the same
/// document (§7.9).
fn render_query_outcome(
    graph: &Graph,
    outcome: StaticQueryOutcome,
    title: Option<&str>,
    ctx: &Ctx,
    depth: u8,
) -> String {
    let total = match &outcome.rows {
        StaticQueryRows::Subtrees(roots) => roots.len(),
        StaticQueryRows::Block(groups) => groups.iter().map(|group| group.blocks.len()).sum(),
        StaticQueryRows::Page(pages) => pages.len(),
    };
    let omitted = outcome.pre_filter_total.saturating_sub(total);
    let _nested = NestedResultsGuard::enter(ctx);
    let mut out = format!(
        "<div class=\"query\"><div class=\"query-head\">{} <span class=\"query-count\">{}</span></div>",
        esc(title.unwrap_or("Query")),
        total
    );
    match outcome.rows {
        StaticQueryRows::Subtrees(roots) => {
            if roots.is_empty() {
                out.push_str("<div class=\"query-empty\">No matching blocks.</div>");
            } else {
                out.push_str("<ul class=\"query-results\">");
                for root in roots {
                    render_result_block(&root.block, &mut out, ctx, depth);
                }
                out.push_str("</ul>");
            }
        }
        StaticQueryRows::Block(groups) if groups.is_empty() => {
            out.push_str("<div class=\"query-empty\">No matching blocks.</div>");
        }
        StaticQueryRows::Block(groups) => {
            out.push_str("<ul class=\"query-results\">");
            render_query_groups(graph, &groups, &mut out, ctx, depth);
            out.push_str("</ul>");
        }
        StaticQueryRows::Page(pages) if pages.is_empty() => {
            out.push_str("<div class=\"query-empty\">No matching pages.</div>");
        }
        StaticQueryRows::Page(pages) => {
            out.push_str("<ul class=\"query-results query-page-results\">");
            for page in pages {
                let Some(link) = publication_page_link(ctx, &page) else {
                    continue;
                };
                out.push_str(&format!(
                    "<li><a class=\"ref query-page-result\" href=\"{}.html\">{}</a>{}</li>",
                    esc_attr(&link.slug),
                    esc(&link.name),
                    if link.kind == PageKind::Journal {
                        "<span class=\"k\">journal</span>"
                    } else {
                        ""
                    }
                ));
            }
            out.push_str("</ul>");
        }
    }
    // A query export says nothing about what it left out — the count of
    // omitted rows is itself information about pages outside the export
    // (Martin, 2026-09-14). The `publish/` site keeps its count: there the
    // reader already knows the graph has private pages.
    if omitted > 0 && !ctx.inert_outside_links {
        out.push_str(&format!(
            "<div class=\"query-omitted\">{} result{} on non-public pages omitted.</div>",
            omitted,
            if omitted == 1 { "" } else { "s" }
        ));
    }
    if ctx.inert_outside_links && outcome.sampled {
        out.push_str(
            "<div class=\"query-sampled\">Sampled across the whole graph before this export was selected; the draw may differ from the live graph.</div>",
        );
    }
    out.push_str("</div>");
    out
}

fn publication_page_link<'a>(ctx: &'a Ctx<'_>, page: &PageRow) -> Option<&'a PublicationPageLink> {
    ctx.page_links?
        .get(&page.path)
        .filter(|link| link.name == page.name && link.kind == page.kind)
}

/// Inline an `{{embed ((uuid))}}` or `{{embed [[Page]]}}`.
fn render_embed(graph: &Graph, arg: &str, ctx: &Ctx, depth: u8) -> String {
    if let Some(uuid) = arg.strip_prefix("((").and_then(|s| s.strip_suffix("))")) {
        let uuid = uuid.trim();
        if ctx.pages.is_some() && !ctx.refs.contains_key(uuid) {
            return "<div class=\"embed embed-missing\">Embedded content is not public.</div>"
                .into();
        }
        const STATIC_EMBED_BLOCK_LIMIT: usize = 10_000;
        const STATIC_EMBED_BYTE_LIMIT: usize = 8 * 1024 * 1024;
        return match crate::query::preview_block_with_budget(
            graph,
            uuid,
            STATIC_EMBED_BLOCK_LIMIT,
            STATIC_EMBED_BYTE_LIMIT,
        ) {
            Some(preview) if publish_page_allowed(ctx, &preview.group.page) => {
                let mut out = String::from(
                    "<div class=\"embed block-embed single-root\"><ul class=\"embed-outline\">",
                );
                for blk in &preview.group.blocks {
                    render_result_block(blk, &mut out, ctx, depth);
                }
                if preview.truncated > 0 {
                    out.push_str(&format!(
                        "<li class=\"query-truncated\">{} more blocks omitted</li>",
                        preview.truncated
                    ));
                }
                out.push_str("</ul></div>");
                out
            }
            Some(_) => {
                "<div class=\"embed embed-missing\">Embedded content is not public.</div>".into()
            }
            None => "<div class=\"embed embed-missing\">Embedded block not found.</div>".into(),
        };
    }
    if let Some(page) = arg.strip_prefix("[[").and_then(|s| s.strip_suffix("]]")) {
        let page = page.trim();
        if let Some(doc) = ctx
            .pages
            .and_then(|pages| pages.get(&crate::refs::page_key(page)).copied())
        {
            return render_page_embed_doc(page, doc, ctx, depth);
        }
        if ctx.pages.is_some() {
            return "<div class=\"embed embed-missing\">Embedded content is not public.</div>"
                .into();
        }
        return match load_page_doc(graph, page) {
            Some(doc) => render_page_embed_doc(page, &doc, ctx, depth),
            None => "<div class=\"embed embed-missing\">Embedded page not found.</div>".into(),
        };
    }
    format!(
        "<span class=\"macro-raw\">{{{{embed {}}}}}</span>",
        esc(arg)
    )
}

/// Embed a video: a YouTube/Vimeo URL becomes an iframe; anything else, a link.
fn render_video(url: &str) -> String {
    if let Some(id) = youtube_id(url) {
        return format!(
            "<div class=\"video-embed\"><iframe src=\"https://www.youtube.com/embed/{}\" \
             allowfullscreen loading=\"lazy\" frameborder=\"0\"></iframe></div>",
            esc_attr(&id)
        );
    }
    match safe_media_url(url) {
        Some(url) => format!(
            "<div class=\"video-embed\"><a href=\"{}\">{}</a></div>",
            esc_attr(url),
            esc(url)
        ),
        None => format!("<div class=\"video-embed unsafe-link\">{}</div>", esc(url)),
    }
}

/// Extract a YouTube video id from a watch/short/embed URL, if this is one.
fn youtube_id(url: &str) -> Option<String> {
    let u = url.trim();
    if let Some(rest) = u.split("v=").nth(1) {
        if u.contains("youtube.com") {
            return Some(rest.split(['&', '#']).next().unwrap_or(rest).to_string());
        }
    }
    if let Some(rest) = u.split("youtu.be/").nth(1) {
        return Some(
            rest.split(['?', '&', '#'])
                .next()
                .unwrap_or(rest)
                .to_string(),
        );
    }
    if let Some(rest) = u.split("youtube.com/embed/").nth(1) {
        return Some(
            rest.split(['?', '&', '#'])
                .next()
                .unwrap_or(rest)
                .to_string(),
        );
    }
    None
}

/// List the pages directly under a `{{namespace X}}` prefix as links.
fn render_namespace(graph: &Graph, ns: &str, ctx: &Ctx) -> String {
    let prefix = format!("{}/", ns.trim());
    let mut children: Vec<String> = graph
        .list_pages()
        .into_iter()
        .map(|e| e.name)
        .filter(|n| n.starts_with(&prefix) && publish_page_allowed(ctx, n))
        .collect();
    children.sort();
    children.dedup();
    if children.is_empty() {
        return format!(
            "<div class=\"namespace-macro\">No pages under {}.</div>",
            esc(ns)
        );
    }
    let mut out = format!(
        "<div class=\"namespace-macro\"><div class=\"ns-head\">{}</div><ul>",
        esc(ns)
    );
    for c in &children {
        out.push_str(&format!(
            "<li><a class=\"ref\" href=\"{}.html\">{}</a></li>",
            page_slug(ctx, c),
            esc(c)
        ));
    }
    out.push_str("</ul></div>");
    out
}

fn publish_page_allowed(ctx: &Ctx, page: &str) -> bool {
    ctx.pages
        .is_none_or(|pages| pages.contains_key(&crate::refs::page_key(page)))
}

fn render_page_embed_doc(page: &str, doc: &doc::Document, ctx: &Ctx, depth: u8) -> String {
    let mut out = format!(
        "<div class=\"embed page-embed\"><a class=\"embed-title ref\" href=\"{}.html\">{}</a><ul>",
        page_slug(ctx, page),
        esc(page)
    );
    for b in &doc.roots {
        render_embedded_block(b, &mut out, ctx, depth);
    }
    out.push_str("</ul></div>");
    out
}

/// Read + parse a page's file by name (for `{{embed [[Page]]}}`), case-insensitively.
fn load_page_doc(graph: &Graph, name: &str) -> Option<doc::Document> {
    #[cfg(test)]
    publish_test_counts::bump_page_doc_load(graph);
    graph.with_pages(|pages| {
        pages
            .iter()
            .find(|(entry, _)| entry.name.eq_ignore_ascii_case(name))
            .map(|(_, document)| document.as_ref().clone())
    })
}

/// A block's OWN `logseq.order-list-type:: number` makes its bullet an ordered
/// marker (the app's `isOrdered`; there is no inheritance). The marker's index
/// counts the run of consecutive own-ordered siblings (`orderedListMarker`).
fn own_ordered(b: &DocBlock) -> bool {
    b.property("logseq.order-list-type").as_deref() == Some("number")
}

/// `1 → a`, `2 → b` (`toLetters`).
fn ord_letters(mut n: u32) -> String {
    let mut s = Vec::new();
    while n > 0 {
        let r = (n - 1) % 26;
        s.insert(0, (b'a' + r as u8) as char);
        n = (n - 1) / 26;
    }
    if s.is_empty() {
        "a".into()
    } else {
        s.into_iter().collect()
    }
}

/// `1 → i`, `2 → ii`, … (`toRoman`).
fn ord_roman(mut n: u32) -> String {
    const MAP: &[(u32, &str)] = &[
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut s = String::new();
    for (v, sym) in MAP {
        while n >= *v {
            s.push_str(sym);
            n -= v;
        }
    }
    if s.is_empty() {
        "i".into()
    } else {
        s
    }
}

fn ord_glyph(kind: u8, idx: u32) -> String {
    match kind % 3 {
        0 => idx.to_string(),
        1 => ord_letters(idx),
        _ => ord_roman(idx),
    }
}

fn render_block(
    b: &DocBlock,
    out: &mut String,
    ctx: &Ctx,
    slug: &str,
    title: &str,
    counter: &mut u32,
    index: &mut Vec<serde_json::Value>,
    opts: PrintOpts,
) {
    render_block_ordered(b, out, ctx, slug, title, counter, index, opts, 0, None)
}

#[allow(clippy::too_many_arguments)]
fn render_block_ordered(
    b: &DocBlock,
    out: &mut String,
    ctx: &Ctx,
    slug: &str,
    title: &str,
    counter: &mut u32,
    index: &mut Vec<serde_json::Value>,
    opts: PrintOpts,
    ordered_parents: u8,
    marker: Option<(u32, u8)>,
) {
    // ONE lsdoc parse → the canonical body skeleton (M3), property/planning-filtered like
    // the app's `bodyBlocks`. No second hand-rolled inline parser (the old `render_inline`).
    let blocks = body_blocks(&b.raw);
    // BEGIN_QUERY is a static-site feature. The print context deliberately has
    // no public-page capability (`pages: None`) and therefore does not inspect
    // or execute the container.
    let begin_query = ctx.pages.and_then(|_| inspect_begin_query(&b.raw, &blocks));

    // Every block gets a stable anchor so a search hit can deep-link straight to it: its
    // `id::` uuid when present, else a generated per-page `b{n}` (never collides with a
    // 36-char uuid). Emitting the `<li id>` and the search-index entry in the SAME place
    // keeps the HTML anchor and the index in lock-step.
    let anchor = match block_id(&b.raw) {
        Some(id) => id,
        None => {
            let a = format!("b{}", *counter);
            *counter += 1;
            a
        }
    };
    if marker.is_some() {
        out.push_str(&format!(
            "<li id=\"{}\" class=\"ol-item\">",
            esc_attr(&anchor)
        ));
    } else {
        out.push_str(&format!("<li id=\"{}\">", esc_attr(&anchor)));
    }
    // The container payload is executable/configuration source, not visible
    // page prose. In particular, malformed payload bytes must not be copied to
    // the publication search index after the visible block fails closed.
    let text = if begin_query.is_some() {
        String::new()
    } else {
        ast_plain_text(&blocks)
    };
    if !text.is_empty() {
        index.push(json!({"slug": slug, "title": title, "anchor": anchor, "text": text}));
    }

    // A block carrying a supported `tine.view` is a SHEET owner: its body label
    // renders as usual, but a whole-body `{{query …}}` becomes the sheet's
    // row source (presented as that view instead of the flat result list) and
    // its children become the view's rows/cells instead of a nested outline
    // (children branch below). The `tine.*` view configuration is chrome — it
    // drives the rendering, so it must not also surface as prose chips.
    let sheet = sheet_config(&b.properties());
    let rendered_body = if begin_query.is_some() {
        None
    } else {
        Some(lsdoc::render_html(&blocks, &md_opts()))
    };
    // §4.3.1: the sheet's row source is the RAW macro too. `whole_query_macro`
    // only decides WHETHER the body is a single query macro (and under which
    // name); the argument itself comes from the block source, so a sheet whose
    // query carries an options map or a literal comma is not silently rewritten.
    let sheet_query_src = sheet.as_ref().and_then(|cfg| {
        if cfg.view != SheetView::Board && cfg.view != SheetView::Table {
            return None;
        }
        let name = whole_query_macro(rendered_body.as_deref().unwrap_or(""))?;
        let extent = crate::query::macro_text::query_macro_extent(&b.raw)?;
        Some((name, extent.argument))
    });

    // Header facets (task checkbox + marker, priority) → the decorated body (lsdoc's
    // canonical render_html; a `# heading` block is wrapped in `<span class="heading-text
    // h{n}">` by render_html itself, so the export markup matches the app) → trailer facets
    // (SCHEDULED/DEADLINE, block properties). Macros in the body are expanded via `ctx`.
    out.push_str(if b.marker() == Some("DONE") {
        "<div class=\"b done\">"
    } else {
        "<div class=\"b\">"
    });
    // An own-numbered block's bullet shows its ordinal, like the app's marker.
    if let Some((idx, kind)) = marker {
        out.push_str(&format!(
            "<span class=\"ord-marker\">{}.</span> ",
            ord_glyph(kind, idx)
        ));
    }
    emit_header_facets(b.marker(), b.priority(), out);
    let scope_properties = enter_block_scope(ctx, b);
    match &begin_query {
        Some(BeginQueryInspection::Supported(begin)) => {
            if let Some(graph) = ctx.graph {
                // The app renders this container as
                // `{{query <query> {:table-view? true}}}` (`BeginQuery.tsx`);
                // a query export records exactly that argument.
                let source = if ctx.scope.is_some() {
                    format!("{} {{:table-view? true}}", begin.query)
                } else {
                    begin.query.clone()
                };
                out.push_str(&render_query_with_title(
                    graph,
                    &source,
                    begin.title.as_deref(),
                    ctx,
                    0,
                ));
            } else {
                out.push_str("<div class=\"query-unsupported begin-query-unsupported\" role=\"alert\">Unsupported BEGIN_QUERY.</div>");
            }
        }
        Some(BeginQueryInspection::Unsupported) => out.push_str(
            "<div class=\"query-unsupported begin-query-unsupported\" role=\"alert\">Unsupported BEGIN_QUERY.</div>",
        ),
        None => match (
            &sheet,
            sheet_query_src.as_ref().map(|(name, argument)| (name.as_str(), argument.as_str())),
            ctx.graph,
        ) {
            (Some(cfg), Some((name, query)), Some(graph)) => {
                let emit = SheetEmit {
                    ctx,
                    slug,
                    title,
                    opts,
                    anchors: false,
                };
                render_query_sheet(graph, cfg, name, query, ctx, &emit, out);
            }
            // §4.3.1: the block's own raw source is the query transport, so a
            // `{:title "T"}` map and a literal comma survive publication.
            _ => out.push_str(&decorate_source(
                rendered_body.as_deref().unwrap_or(""),
                Some(&b.raw),
                ctx,
                0,
            )),
        },
    }
    leave_block_scope(ctx, scope_properties);
    out.push_str("</div>");
    let props = b.properties();
    let props = if sheet.is_some() {
        props
            .into_iter()
            .filter(|(k, _)| !crate::doc::property_key_norm(k).starts_with("tine."))
            .collect()
    } else {
        props
    };
    emit_trailer_facets(b.scheduled(), b.deadline(), &b.raw, &props, out);
    if let (Some(id), Some(reverse)) = (block_id(&b.raw), ctx.reverse_refs) {
        if let Some(referrers) = reverse.get(&id).filter(|items| !items.is_empty()) {
            let count = referrers.len();
            out.push_str(&format!(
                "<details class=\"block-referrers\"><summary class=\"ref-count\" aria-label=\"{count} block reference{}\">{count}</summary><ul>",
                if count == 1 { "" } else { "s" }
            ));
            for referrer in referrers {
                let text = if referrer.text.is_empty() {
                    "Referenced block"
                } else {
                    &referrer.text
                };
                out.push_str(&format!(
                    "<li><a href=\"{}.html#{}\"><span class=\"referrer-page\">{}</span>: {}</a></li>",
                    referrer.slug,
                    fragment(&referrer.anchor),
                    esc(&referrer.page),
                    esc(text)
                ));
            }
            out.push_str("</ul></details>");
        }
    }

    // A collapsed block hides its children on screen; the print export expands them
    // by default (a PDF usually wants the whole page), but the dialog can keep them
    // folded to match what's visible.
    if !b.children.is_empty() && (opts.expand_collapsed || !b.collapsed()) {
        // A children-backed sheet consumes its children as the view's
        // rows/cards/cells; only a query-backed owner keeps its (unrelated)
        // children as an ordinary outline below the results view.
        match &sheet {
            Some(cfg) if sheet_query_src.is_none() => {
                let emit = SheetEmit {
                    ctx,
                    slug,
                    title,
                    opts,
                    anchors: true,
                };
                render_children_sheet(cfg, &b.children, &emit, counter, index, 0, out);
            }
            _ => {
                out.push_str("<ul>");
                // Children keep their own-numbered markers: every own-ordered
                // child shows its index in the run of consecutive own-ordered
                // siblings, with the glyph cycling number → letter → roman by
                // consecutive own-ordered ancestors (mod 3), like the app.
                let depth_to_children = if own_ordered(b) {
                    ordered_parents + 1
                } else {
                    0
                };
                let mut ord_run = 0u32;
                for c in &b.children {
                    let child_ordered = own_ordered(c);
                    ord_run = if child_ordered { ord_run + 1 } else { 0 };
                    let marker = if child_ordered {
                        Some((ord_run, ordered_parents))
                    } else {
                        None
                    };
                    render_block_ordered(
                        c,
                        out,
                        ctx,
                        slug,
                        title,
                        counter,
                        index,
                        opts,
                        depth_to_children,
                        marker,
                    );
                }
                out.push_str("</ul>");
            }
        }
    }
    out.push_str("</li>");
}

fn page_html(
    title: &str,
    slug: &str,
    doc: &doc::Document,
    kind: PageKind,
    ctx: &Ctx,
    blocks: &mut Vec<serde_json::Value>,
    home_href: &str,
) -> String {
    let mut body = String::new();
    body.push_str("<ul class=\"outline\">");
    let mut counter = 0u32;
    // Root blocks take part in the same own-numbered run logic as any
    // sibling list (consecutive own-ordered siblings share the index run).
    let mut ord_run = 0u32;
    for b in &doc.roots {
        let ordered = own_ordered(b);
        ord_run = if ordered { ord_run + 1 } else { 0 };
        let marker = if ordered { Some((ord_run, 0u8)) } else { None };
        // The whole-graph site export always expands (no fold state on paper).
        render_block_ordered(
            b,
            &mut body,
            ctx,
            slug,
            title,
            &mut counter,
            blocks,
            PrintOpts::default(),
            0,
            marker,
        );
    }
    body.push_str("</ul>");
    // Journal titles get a leading calendar glyph, like Logseq.
    let cal = "<svg class=\"cal\" viewBox=\"0 0 24 24\" width=\"18\" height=\"18\" fill=\"none\" \
stroke=\"currentColor\" stroke-width=\"1.7\"><rect x=\"4\" y=\"5\" width=\"16\" height=\"16\" rx=\"2\"/>\
<line x1=\"4\" y1=\"9.5\" x2=\"20\" y2=\"9.5\"/><line x1=\"8.5\" y1=\"3\" x2=\"8.5\" y2=\"7\"/>\
<line x1=\"15.5\" y1=\"3\" x2=\"15.5\" y2=\"7\"/></svg>";
    let heading = if kind == PageKind::Journal {
        format!("<h1 class=\"page\">{}{}</h1>", cal, esc(title))
    } else {
        format!("<h1 class=\"page\">{}</h1>", esc(title))
    };
    shell(title, &format!("{heading}{body}"), home_href)
}

/// The shared two-column document shell used by every generated page: `<head>` +
/// the persistent sidebar (home link, search box, and a `#tine-pages` list filled
/// by `app.js`) + the page's `<main>` + the export scripts. The sidebar markup is
/// identical on every page; `app.js` reads the embedded `search-index.js` globals
/// (`window.__tinePages` / `__tineBlocks`) — read as `<script>` globals, never
/// `fetch`ed — so navigation and search work offline / opened straight off disk
/// (`file://`, where `fetch` of a sibling file is blocked but `<script src>` is not).
fn shell(title: &str, main: &str, home_href: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'self'; base-uri 'none'; object-src 'none'; form-action 'none'; script-src 'self' https://cdn.jsdelivr.net; style-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; font-src 'self' data: https://cdn.jsdelivr.net; img-src 'self' data: https: http:; media-src 'self' blob: https: http:; frame-src https://www.youtube.com https://player.vimeo.com\">\
<title>{title}</title>\
<link rel=\"stylesheet\" href=\"style.css\">{katex}{hljs}</head><body>\
<aside class=\"sidebar\">\
<a class=\"home\" href=\"{home_href}\">\u{2302} Home</a>\
<a class=\"home pages-link\" href=\"pages.html\">All pages</a>\
<input id=\"tine-search\" type=\"search\" placeholder=\"Search\u{2026}\" autocomplete=\"off\" spellcheck=\"false\">\
<div id=\"tine-results\" hidden></div>\
<nav id=\"tine-pages\"></nav>\
</aside><main>{main}</main>\
<script src=\"search-index.js\"></script><script src=\"fuse.min.js\"></script><script src=\"app.js\"></script><script defer src=\"enhance.js\"></script>\
</body></html>",
        title = esc(title),
        katex = KATEX_HEAD,
        hljs = HLJS_HEAD,
        main = main,
        home_href = esc_attr(home_href),
    )
}

/// A **self-contained single page** for the print-to-PDF export: the same block
/// render as a published page, but with the stylesheet + print rules inlined and
/// no sidebar / search / scripts — so the returned document cannot execute in the
/// privileged Tauri origin. Assets are inlined as `data:` URIs upstream
/// (`inline_assets`). The frontend upgrades math/code with its locally bundled
/// KaTeX/highlight.js before placing this static document in a script-disabled
/// sandbox.
// Inter faces bundled INTO the print document as `@font-face` data URIs. The print
// doc is a separate document from the app, so it does NOT inherit the app's Inter
// `@font-face` rules; without this it falls back to a system font, and if that font
// lacks an italic face WebKitGTK *synthesizes* one — which its PDF (Cairo) backend
// renders garbled (emphasis in particular). Embedding real normal/italic/bold faces
// fixes that and makes the PDF self-contained + Inter-faithful. Latin subset only
// (~120 KB); non-latin falls back to the system font, same as before.
const INTER_FACES: &[(&[u8], u32, &str)] = &[
    (
        include_bytes!("../assets/fonts/inter-400-normal.woff2"),
        400,
        "normal",
    ),
    (
        include_bytes!("../assets/fonts/inter-400-italic.woff2"),
        400,
        "italic",
    ),
    (
        include_bytes!("../assets/fonts/inter-600-normal.woff2"),
        600,
        "normal",
    ),
    (
        include_bytes!("../assets/fonts/inter-700-normal.woff2"),
        700,
        "normal",
    ),
    (
        include_bytes!("../assets/fonts/inter-700-italic.woff2"),
        700,
        "italic",
    ),
];

fn print_fontface() -> String {
    let mut css = String::new();
    for (bytes, weight, style) in INTER_FACES {
        css.push_str(&format!(
            "@font-face{{font-family:'Inter';font-weight:{weight};font-style:{style};font-display:swap;\
src:url(data:font/woff2;base64,{}) format('woff2')}}\n",
            base64_encode(bytes),
        ));
    }
    css
}

fn print_shell(title: &str, main: &str, opts: PrintOpts) -> String {
    // Dialog-driven knobs (font size + page margin) are appended AFTER PRINT_STYLE so
    // they win over its `@page`/font defaults. Clamped to sane bounds.
    let font = opts.font_px.clamp(8, 40);
    let margin = opts.margin_mm.clamp(0, 50);
    let tuned = format!("@page{{margin:{margin}mm}}\nbody.print{{font-size:{font}px}}");
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{title}</title>\
<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; script-src 'none'; style-src 'self' 'unsafe-inline'; font-src 'self' data:; img-src 'self' data:; media-src 'self' data:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'\">\
<style>{fonts}{style}\n{print}\n{tuned}</style></head><body class=\"print\">\
<main>{main}</main></body></html>",
        title = esc(title),
        fonts = print_fontface(),
        style = STYLE,
        print = PRINT_STYLE,
        tuned = tuned,
        main = main,
    )
}

/// Print/PDF-only overrides layered on top of `STYLE`. The published-site `STYLE`
/// lays out a two-column app shell (flex body + sticky sidebar); with no sidebar
/// here we drop the flex row, widen `main` to the page, set page margins, and add
/// print niceties (avoid breaking a block across pages, un-style links to plain
/// text so a printed link isn't a mystery blue word).
const PRINT_STYLE: &str = r#"
@page{margin:16mm 14mm}
/* Force a LIGHT, printable document even if the OS/webview is in dark mode — a PDF
   should never be white-on-black. These come AFTER STYLE in the same <style>, so
   they override STYLE's own prefers-color-scheme:dark block (both the default and
   the dark media query are pinned to the light palette). */
:root{color-scheme:light;
  --bg:#fff;--fg:#1c1d1e;--muted:#8a8f98;--line:#e4e4e8;--accent:#10b981;--link:#0b5cad;--code:#f4f5f7;}
@media (prefers-color-scheme:dark){:root{
  --bg:#fff;--fg:#1c1d1e;--muted:#8a8f98;--line:#e4e4e8;--accent:#10b981;--link:#0b5cad;--code:#f4f5f7;}}
body.print{display:block;background:#fff;color:var(--fg);
  /* WebKitGTK renders Inter's `->` / `--` / `-->` ligatures as arrow/dash glyphs
     that look garbled in the export (the editor disables ligatures for the same
     reason). Keep the literal characters in the PDF. */
  font-variant-ligatures:none;font-feature-settings:"liga" 0,"calt" 0;
  /* Only ever use the REAL embedded Inter faces (normal/italic/bold above) — never
     a synthesized oblique/bold, which WebKitGTK's PDF backend renders garbled. */
  font-synthesis:none;}
body.print main{max-width:none;margin:0;padding:0 4mm 8mm}
body.print h1.page{margin-top:0}
/* No bullet guide-rails on paper: the connecting lines are a screen-navigation
   affordance and misalign against the bullet dots when printed — dots alone read
   cleaner. */
body.print ul.outline ul{border-left:none}
.print-asset-omitted{display:inline-block;color:#777;font-style:italic;margin:.3rem 0}
@media print{
  body.print main{padding:0}
  a.ref,a.tag,a.block-ref{color:inherit;text-decoration:none}
  li,pre,table,.video-embed,img{break-inside:avoid}
  h1,h2,h3,h4,h5,h6,.heading-text{break-after:avoid}
  /* Print the accent colors (task-checkbox fills, callout tints) instead of
     dropping them to grey. */
  *{-webkit-print-color-adjust:exact;print-color-adjust:exact;}
}
"#;

/// Render ONE page to a self-contained HTML document for print-to-PDF. `name` is
/// the page's display name (as shown in the app / `PageEntry.name`). Returns
/// `Ok(None)` if no such page file exists. Block references resolve within this
/// page (in-page `((ref))` → an in-document anchor); a ref to a block on another
/// page degrades to its label / muted text (there's no second file to link to),
/// which is correct for a single-page export.
pub fn page_print_html(
    graph: &Graph,
    name: &str,
    opts: PrintOpts,
) -> Result<Option<String>, PrintPreparationError> {
    let Some(entry) = graph.list_pages().into_iter().find(|e| e.name == name) else {
        return Ok(None);
    };
    let content = fs::read_to_string(&entry.path)?;
    let parsed = doc::parse(&content);
    graph
        .with_print_query_reader(|reader| {
            page_print_html_document(graph, &entry.name, &parsed, opts, reader)
        })
        .map(Some)
}

/// Render one already-authoritative page document. [`page_print_html`] enters
/// here after reading the page.
pub(crate) fn page_print_html_document(
    graph: &Graph,
    name: &str,
    parsed: &doc::Document,
    opts: PrintOpts,
    reader: &dyn PublicationQueryRead,
) -> Result<String, PrintPreparationError> {
    let slug = slug(name);
    let mut refs = RefIndex::new();
    collect_block_refs(&parsed.roots, &slug, &mut refs);
    let print_asset_budget = RefCell::new(PrintAssetBudget::standard());
    let print_error = RefCell::new(None);
    let ctx = Ctx {
        refs: &refs,
        reverse_refs: None,
        graph: Some(graph),
        slugs: None,
        inline_assets: true,
        print_asset_budget: Some(&print_asset_budget),
        query_reader: Some(reader),
        print_error: Some(&print_error),
        pages: None,
        page_links: None,
        inert_outside_links: false,
        scope: None,
        recorder: None,
        asset_sink: None,
    };
    // `page_html` builds the heading + outline and wraps it in `shell`; we want the
    // same body but the print shell, so mirror its body build here.
    let mut blocks: Vec<serde_json::Value> = Vec::new();
    let mut body = String::new();
    body.push_str("<ul class=\"outline\">");
    let mut counter = 0u32;
    let mut ord_run = 0u32;
    for b in &parsed.roots {
        let ordered = own_ordered(b);
        ord_run = if ordered { ord_run + 1 } else { 0 };
        let marker = if ordered { Some((ord_run, 0u8)) } else { None };
        render_block_ordered(
            b,
            &mut body,
            &ctx,
            &slug,
            name,
            &mut counter,
            &mut blocks,
            opts,
            0,
            marker,
        );
    }
    body.push_str("</ul>");
    let heading = format!("<h1 class=\"page\">{}</h1>", esc(name));
    let html = print_shell(name, &format!("{heading}{body}"), opts);
    if let Some(error) = print_error.into_inner() {
        return Err(error);
    }
    reader.ensure_current()?;
    Ok(html)
}

/// Options for the single-page print/PDF export, chosen in the pre-export dialog.
/// `#[serde(default)]` so a partial object from the frontend fills the rest.
#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(default)]
pub struct PrintOpts {
    /// Render the children of a `collapsed:: true` block anyway (true = expand the
    /// whole page, the usual PDF want; false = print it folded as on screen).
    pub expand_collapsed: bool,
    /// Base body font size in px.
    pub font_px: u32,
    /// Page margin in mm (all four sides).
    pub margin_mm: u32,
}

impl Default for PrintOpts {
    fn default() -> Self {
        Self {
            expand_collapsed: true,
            font_px: 16,
            margin_mm: 16,
        }
    }
}

// KaTeX (from CDN) typesets the `\(..\)` / `\[..\]` math the decorator emits from
// lsdoc's `data-tex` hook, client-side in the published pages. mhchem (\ce{…}) must
// register before auto-render runs; `defer` preserves script order, so auto-render's
// onload fires only after katex.min.js and mhchem have executed. Math therefore
// typesets when the page is viewed online; an offline viewer shows the raw TeX.
const KATEX_HEAD: &str = r#"<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/katex@0.16.47/dist/katex.min.css"><script defer src="https://cdn.jsdelivr.net/npm/katex@0.16.47/dist/katex.min.js"></script><script defer src="https://cdn.jsdelivr.net/npm/katex@0.16.47/dist/contrib/mhchem.min.js"></script><script defer src="https://cdn.jsdelivr.net/npm/katex@0.16.47/dist/contrib/auto-render.min.js"></script>"#;

// highlight.js (from CDN) syntax-highlights the `<pre class="code-block"><code
// class="hljs language-X">` blocks lsdoc emits (the export's `data-lang` → `language-X`).
// `highlightAll()` reads the `language-X` class; `defer` + onload runs it after the body
// parses. Offline / no network → plain (already-escaped) code, never broken.
const HLJS_HEAD: &str = r#"<link rel="stylesheet" href="https://cdn.jsdelivr.net/gh/highlightjs/cdn-release@11.11.1/build/styles/github.min.css"><script defer src="https://cdn.jsdelivr.net/gh/highlightjs/cdn-release@11.11.1/build/highlight.min.js"></script>"#;

const ENHANCE_JS: &str = r#"(function () {
  'use strict';
  if (window.renderMathInElement) {
    window.renderMathInElement(document.body, {
      delimiters: [
        {left: '\\[', right: '\\]', display: true},
        {left: '\\(', right: '\\)', display: false}
      ],
      throwOnError: false
    });
  }
  if (window.hljs) window.hljs.highlightAll();
})();
"#;

const STYLE: &str = r#":root{
  --bg:#fff;--fg:#2e2e2e;--muted:#8a8f98;--line:#e9e9ec;--accent:#10b981;--link:#0b6ec9;--code:#f4f5f7;
}
@media (prefers-color-scheme:dark){:root{--bg:#1b1c1d;--fg:#d8dadd;--muted:#7a7f87;--line:#2d2f31;--link:#5aa9ef;--code:#26282a;}}
*{box-sizing:border-box}
body{font-family:'Inter',-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;
  background:var(--bg);color:var(--fg);margin:0;line-height:1.6;
  -webkit-font-smoothing:antialiased;font-size:16px;display:flex;align-items:flex-start}
main{flex:1 1 0;min-width:0;max-width:740px;margin:0 auto;padding:48px 24px 96px}
/* sidebar */
.sidebar{flex:0 0 260px;width:260px;position:sticky;top:0;align-self:stretch;height:100vh;overflow-y:auto;
  border-right:1px solid var(--line);padding:22px 14px;font-size:.9rem}
.sidebar a.home{display:block;color:var(--fg);font-size:.95rem;font-weight:650;text-decoration:none;margin:0 4px 12px}
.sidebar a.home:hover{color:var(--link)}
#tine-search{width:100%;padding:7px 10px;border:1px solid var(--line);border-radius:7px;background:var(--bg);
  color:var(--fg);font-size:.9rem;outline:none;font-family:inherit}
#tine-search:focus{border-color:var(--link)}
#tine-results{margin-top:10px}
#tine-results .res{display:block;padding:6px 8px;border-radius:6px;text-decoration:none;color:var(--fg)}
#tine-results .res:hover{background:var(--code)}
#tine-results .res-title{display:block;font-weight:650;font-size:.84rem;color:var(--link)}
#tine-results .res-snip{display:block;font-size:.8rem;color:var(--muted);line-height:1.4;margin-top:1px}
#tine-results mark{background:rgba(245,196,66,.38);color:inherit;border-radius:2px;padding:0 1px}
#tine-results .empty{color:var(--muted);font-size:.85rem;padding:6px 8px}
#tine-pages{margin-top:14px}
#tine-pages .sec{margin-bottom:14px}
#tine-pages h3{font-size:.68rem;text-transform:uppercase;letter-spacing:.06em;color:var(--muted);
  margin:0 0 4px 8px;font-weight:700}
#tine-pages ul{list-style:none;margin:0;padding:0}
#tine-pages li{margin:0;position:static}
#tine-pages li::before{display:none}
#tine-pages a{display:block;padding:3px 8px;border-radius:5px;text-decoration:none;color:var(--fg);
  white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
#tine-pages a:hover{background:var(--code)}
#tine-pages a.active{background:var(--code);font-weight:650;color:var(--link)}
@media (max-width:720px){
  body{flex-direction:column}
  .sidebar{position:static;height:auto;width:100%;flex-basis:auto;align-self:auto;
    border-right:none;border-bottom:1px solid var(--line)}
  main{padding:24px 18px 64px}
}
h1.page{font-size:1.9rem;font-weight:700;letter-spacing:-.02em;margin:.4rem 0 1.4rem}
h1.page .cal{color:var(--muted);margin-right:.45rem;vertical-align:-3px;opacity:.7}
ul.outline,ul.outline ul{list-style:none}
ul.outline{padding-left:0;margin:0}
ul.outline ul{padding-left:1.25rem;margin:.1rem 0;border-left:0;position:relative}
/* Put each connector through the child bullet's center. The old border sat on
   the ul box edge, roughly 7px left of every bullet. */
ul.outline ul::before{content:"";position:absolute;left:.45rem;top:0;bottom:0;border-left:1px solid var(--line)}
li{margin:1px 0;position:relative}
li::before{content:"";position:absolute;left:-0.95rem;top:.62em;width:5px;height:5px;border-radius:50%;
  background:var(--muted);opacity:.45}
ul.outline>li::before{display:none}
.b{padding:1px 0}
h1,h2,h3,h4,h5,h6{line-height:1.3;margin:.5rem 0 .2rem;letter-spacing:-.01em}
h2{font-size:1.4rem}h3{font-size:1.18rem}h4{font-size:1.04rem}
/* block-level `# heading` bodies render as `.heading-text.h{n}` spans (lsdoc render_html). */
.heading-text{display:block;font-weight:600;line-height:1.3;letter-spacing:-.01em;margin:.4rem 0 .15rem}
.heading-text.h1{font-size:1.7em}.heading-text.h2{font-size:1.4em}.heading-text.h3{font-size:1.2em}
.heading-text.h4{font-size:1.1em}.heading-text.h5{font-size:1em}.heading-text.h6{font-size:.9em}
a.ref,a.tag{color:var(--link);text-decoration:none}
a.ref:hover,a.tag:hover{text-decoration:underline}
a.block-ref,span.block-ref{background:var(--code);border-radius:4px;padding:0 .28em;font-size:.95em}
a.block-ref{color:var(--link);text-decoration:none}
a.block-ref:hover{text-decoration:underline}
span.block-ref{color:var(--muted)}
.block-referrers{display:inline-block;margin-left:.4rem;vertical-align:middle}
.block-referrers summary.ref-count{display:inline-flex;align-items:center;justify-content:center;min-width:1.45em;height:1.35em;
  padding:0 .35em;border:1px solid var(--line);border-radius:999px;color:var(--muted);font-size:.72em;cursor:pointer;list-style:none}
.block-referrers summary.ref-count::-webkit-details-marker{display:none}
.block-referrers[open]{display:block;margin:.3rem 0 .55rem .2rem}
.block-referrers ul{margin:.3rem 0 0;padding-left:1.2rem;border-left:1px solid var(--line)}
.block-referrers li{font-size:.86em;color:var(--muted)}
.block-referrers a{color:var(--link);text-decoration:none}.block-referrers a:hover{text-decoration:underline}
.referrer-page{font-weight:600}
a.tag{font-size:.92em}
a[href^="http"]{color:var(--link)}
code,.inline-code{background:var(--code);border-radius:4px;padding:.05em .35em;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.9em}
/* fenced code blocks (highlight.js) — not the inline pill */
pre.code-block{background:var(--code);border:1px solid var(--line);border-radius:8px;padding:.7em .9em;overflow:auto;margin:.4rem 0}
pre.code-block code,pre.code-block code.hljs{background:none;border:0;padding:0;font-size:.86em;display:block}
img,.inline-image{max-width:100%;border-radius:6px;margin:.3rem 0}
.inline-image-wrap{display:inline-block;max-width:100%}
.media-embed{max-width:100%;border-radius:6px;margin:.3rem 0}
.math-display{display:block;text-align:center;margin:.5rem 0}
strong{font-weight:650}
/* in-block markdown lists (.b-scoped so they win over the outline ul rules) */
.b ul.md-list,.b ol.md-list{margin:.2rem 0;padding-left:1.3rem;border-left:none}
.b ul.md-list{list-style:disc}.b ol.md-list{list-style:decimal}
.b li.md-list-item{margin:.1rem 0;position:static}
.b li.md-list-item::before{display:none}
.md-list-term{font-weight:650}
.block-checkbox{display:inline-block;width:.95em;height:.95em;border:1.5px solid var(--muted);border-radius:3px;vertical-align:-2px;margin-right:.15em}
.block-checkbox.checked{background:var(--accent);border-color:var(--accent)}
/* task facets: checkbox + marker badge, priority badge (match the app header) */
.task-checkbox{display:inline-block;width:.95em;height:.95em;border:1.5px solid var(--muted);border-radius:3px;vertical-align:-2px;margin-right:.35em;position:relative}
.task-checkbox.checked{background:var(--accent);border-color:var(--accent)}
.task-checkbox.checked::after{content:"";position:absolute;left:.28em;top:.08em;width:.2em;height:.42em;border:solid #fff;border-width:0 .12em .12em 0;transform:rotate(45deg)}
.task-marker{font-size:.68rem;font-weight:700;letter-spacing:.03em;padding:.05em .35em;border-radius:4px;background:var(--code);color:var(--muted);vertical-align:.05em}
.task-marker.m-doing,.task-marker.m-now{color:#c2410c;background:#fff2e8}
.task-marker.m-done{color:var(--accent);background:#e7f7f0}
.task-marker.m-waiting{color:#a16207;background:#fdf6e3}
.task-marker.m-canceled,.task-marker.m-cancelled{color:var(--muted);text-decoration:line-through}
.b.done>.heading-text,.b.done{color:var(--muted)}
.priority{font-size:.72rem;font-weight:700;padding:.02em .3em;border-radius:4px;background:var(--code);color:var(--muted)}
.priority.p-a{color:#b91c1c;background:#fdeaea}.priority.p-b{color:#c2410c;background:#fff2e8}
/* planning (SCHEDULED/DEADLINE) + block properties */
.planning{font-size:.85em;color:var(--muted);margin:.05rem 0}
/* own-numbered blocks: the ordinal replaces the bullet dot, like the app */
li.ol-item::before{display:none}
.ord-marker{color:var(--muted);font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.88em;font-weight:600;margin-right:.35em}
.planning.deadline .pk{color:#b91c1c}
.planning .pk,.block-props .pk{font-weight:650;letter-spacing:.02em}
.block-props{font-size:.85em;color:var(--muted);margin:.1rem 0;display:flex;flex-wrap:wrap;gap:.1rem .8rem}
.block-props .pv{color:var(--fg)}
/* query results + embeds + video + namespace macro */
.query{border:1px solid var(--line);border-radius:8px;margin:.4rem 0;overflow:hidden}
.query-head{font-size:.72rem;text-transform:uppercase;letter-spacing:.05em;color:var(--muted);background:var(--code);padding:.3em .7em}
.query-count{background:var(--muted);color:var(--bg);border-radius:8px;padding:0 .45em;margin-left:.3em;font-size:.9em}
.query-results{padding:.3rem .7rem}
.query-empty{padding:.5rem .7rem;color:var(--muted);font-size:.9em}
.query-omitted{padding:.35rem .7rem;color:var(--muted);font-size:.82em;border-top:1px solid var(--line)}
.query-unsupported{border:1px solid #b91c1c;border-radius:8px;margin:.4rem 0;padding:.5rem .7rem;color:#b91c1c;background:#fdeaea;font-size:.9em}
.embed{border-left:3px solid var(--line);padding:.1rem 0 .1rem .8rem;margin:.35rem 0}
/* A block embed is already hosted by one outline li. Remove the embedded ul's
   second bullet/connector and the generic embed border, leaving one root marker. */
.block-embed.single-root{border-left:0;padding-left:0}
.block-embed.single-root>ul.embed-outline{padding-left:0;margin:0}
.block-embed.single-root>ul.embed-outline::before,
.block-embed.single-root>ul.embed-outline>li::before{content:none}
.embed-title{display:inline-block;font-size:.78rem;color:var(--muted);margin-bottom:.1rem}
.embed-missing{color:var(--muted);font-style:italic;font-size:.9em}
.video-embed{position:relative;width:100%;max-width:560px;aspect-ratio:16/9;margin:.4rem 0}
.video-embed iframe{position:absolute;inset:0;width:100%;height:100%;border:0;border-radius:8px}
.namespace-macro{margin:.3rem 0}
.namespace-macro .ns-head{font-size:.72rem;text-transform:uppercase;letter-spacing:.05em;color:var(--muted)}
.macro-raw{color:var(--muted);font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.9em}
/* sheets: read-only renderings of tine.view blocks (app semantics, static form).
   The scroll wrapper must cap against the viewport as well as its container —
   the Guide's phone layout lets content widen the flex-stretched main column,
   which a bare max-width:100% would trust. */
.sheet-scroll{max-width:min(100%,calc(100vw - 72px));overflow-x:auto;margin:.5rem 0;contain:inline-size}
table.sheet-table,table.sheet-grid{border-collapse:collapse;font-size:.94em;margin:0}
table.sheet-table th,table.sheet-table td,table.sheet-grid th,table.sheet-grid td{border:1px solid var(--line);padding:.3em .6em;text-align:left;vertical-align:top}
table.sheet-table thead th,table.sheet-grid thead th{background:var(--code);font-weight:650}
table.sheet-table tfoot td{color:var(--muted);font-size:.92em;white-space:nowrap}
.sheet-agg-label{font-weight:650;letter-spacing:.02em;margin-right:.35em}
.sheet-board{display:flex;align-items:stretch;gap:.8rem;overflow-x:auto;margin:.5rem 0;max-width:min(100%,calc(100vw - 72px));contain:inline-size}
.sheet-board-col{flex:1 1 11rem;min-width:10rem;border:1px solid var(--line);border-radius:8px}
.sheet-board-col>h3{font-size:.72rem;text-transform:uppercase;letter-spacing:.05em;color:var(--muted);background:var(--code);margin:0;padding:.3em .7em;border-radius:8px 8px 0 0;font-weight:650}
.sheet-board-col>ul{list-style:none;margin:0;padding:.3rem .7rem}
.sheet-board-col li{margin:.3rem 0;position:static}
.sheet-board-col li::before{display:none}
ul.sheet-children{list-style:none;padding-left:1.25rem;margin:.2rem 0}
.sheet-cell-label{font-size:.86em;color:var(--muted);margin-bottom:.2rem}
/* tables (data-align is the export's beyond-OG column alignment) */
table.md-table{border-collapse:collapse;margin:.5rem 0;font-size:.94em}
table.md-table th,table.md-table td{border:1px solid var(--line);padding:.3em .6em;text-align:left}
table.md-table th{background:var(--code);font-weight:650}
table.md-table [data-align="center"]{text-align:center}
table.md-table [data-align="right"]{text-align:right}
/* blockquote + callouts */
blockquote.md-quote{margin:.5rem 0;padding:.2rem 0 .2rem .9rem;border-left:3px solid var(--line);color:var(--muted)}
.callout{margin:.5rem 0;padding:.5rem .8rem;border-radius:8px;border-left:3px solid var(--accent);background:var(--code)}
.callout-title{font-weight:700;font-size:.82rem;text-transform:uppercase;letter-spacing:.04em;color:var(--accent);margin-bottom:.2rem}
/* org timestamps + footnotes */
.org-timestamp{color:var(--muted);font-size:.92em;font-family:ui-monospace,SFMono-Regular,Menlo,monospace}
.org-timestamp.inactive{opacity:.6}
.footnote-def{font-size:.9em;color:var(--muted);margin:.2rem 0}
.footnote-ref{color:var(--link);font-size:.85em}
.index-list li{margin:.15rem 0}
.index-list .k{color:var(--muted);font-size:.8rem;margin-left:.4rem}
.md-hr,hr{border:none;border-top:1px solid var(--line);margin:1.2rem 0}
footer{margin-top:64px;color:var(--muted);font-size:.78rem;border-top:1px solid var(--line);padding-top:12px}
"#;

// Sidebar + search behaviour for the published site. Vanilla JS, no build step; the
// only dependency is the vendored Fuse.js (loaded separately). Reads the embedded
// `window.__tinePages` / `__tineBlocks` globals (never `fetch`ed) so it works offline
// and over `file://`. Fuse is configured to mirror OG's published block search
// (threshold 0.35, block-level content). Search hits deep-link to `slug.html#anchor`.
const APP_JS: &str = r#"(function () {
  'use strict';
  var pages = window.__tinePages || [];
  var blocks = window.__tineBlocks || [];

  function esc(s) {
    return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  }
  function basename(p) {
    var parts = String(p).split('/');
    return decodeURIComponent(parts[parts.length - 1] || '');
  }
  var here = basename(location.pathname);

  var input = document.getElementById('tine-search');
  var results = document.getElementById('tine-results');
  var nav = document.getElementById('tine-pages');

  // ---- sidebar page list ----
  function section(title, items) {
    if (!items.length) return '';
    var lis = items.map(function (p) {
      var file = p.slug + '.html';
      var cls = basename(file) === here ? ' class="active"' : '';
      return '<li><a href="' + file + '"' + cls + '>' + esc(p.title) + '</a></li>';
    }).join('');
    return '<div class="sec"><h3>' + esc(title) + '</h3><ul>' + lis + '</ul></div>';
  }
  function byTitleAsc(a, b) { return a.title < b.title ? -1 : a.title > b.title ? 1 : 0; }
  function byTitleDesc(a, b) { return a.title < b.title ? 1 : a.title > b.title ? -1 : 0; }
  function renderPages() {
    if (!nav) return;
    var favs = pages.filter(function (p) { return p.favorite; });
    var journals = pages.filter(function (p) { return p.journal; }).slice().sort(byTitleDesc);
    var plain = pages.filter(function (p) { return !p.journal; }).slice().sort(byTitleAsc);
    nav.innerHTML = section('Favorites', favs) + section('Journals', journals) + section('Pages', plain);
  }

  // ---- fuzzy search (Fuse, OG params) ----
  var fuse = window.Fuse ? new window.Fuse(blocks, {
    keys: ['text', 'title'],
    threshold: 0.35,
    ignoreLocation: true,
    minMatchCharLength: 1,
    includeMatches: true
  }) : null;

  function snippet(entry, matches) {
    var text = entry.text || '';
    var at = -1, len = 0;
    if (matches) {
      for (var i = 0; i < matches.length; i++) {
        var m = matches[i];
        if (m.key === 'text' && m.indices && m.indices.length) {
          at = m.indices[0][0];
          len = m.indices[0][1] - at + 1;
          break;
        }
      }
    }
    if (at < 0) {
      return esc(text.slice(0, 100)) + (text.length > 100 ? '…' : '');
    }
    var start = Math.max(0, at - 28);
    var pre = (start > 0 ? '…' : '') + text.slice(start, at);
    var hit = text.slice(at, at + len);
    var rest = at + len;
    var post = text.slice(rest, rest + 52) + (text.length > rest + 52 ? '…' : '');
    return esc(pre) + '<mark>' + esc(hit) + '</mark>' + esc(post);
  }

  function showList() {
    if (results) { results.hidden = true; results.innerHTML = ''; }
    if (nav) nav.hidden = false;
  }
  function run(q) {
    q = (q || '').trim();
    if (!fuse || !q) { showList(); return; }
    var hits = fuse.search(q, { limit: 20 });
    if (!results) return;
    if (!hits.length) {
      results.innerHTML = '<div class="empty">No matches</div>';
    } else {
      results.innerHTML = hits.map(function (h) {
        var e = h.item;
        var href = e.slug + '.html#' + encodeURIComponent(String(e.anchor));
        return '<a class="res" href="' + href + '">' +
          '<span class="res-title">' + esc(e.title) + '</span>' +
          '<span class="res-snip">' + snippet(e, h.matches) + '</span></a>';
      }).join('');
    }
    results.hidden = false;
    if (nav) nav.hidden = true;
  }

  if (input) {
    input.addEventListener('input', function () { run(input.value); });
    input.addEventListener('keydown', function (ev) {
      if (ev.key === 'Escape') { input.value = ''; showList(); input.blur(); }
      else if (ev.key === 'Enter') {
        var first = results && results.querySelector('a.res');
        if (first) { ev.preventDefault(); location.href = first.getAttribute('href'); }
      }
    });
  }

  renderPages();
})();
"#;

/// True if a page's property pre-block marks it `public:: true`.
fn page_is_public(pre_block: Option<&str>) -> bool {
    let Some(pre) = pre_block else { return false };
    pre.lines().any(|line| {
        crate::doc::parse_property_line(line).is_some_and(|(key, value)| {
            crate::doc::property_key_norm(key) == "public" && value == "true"
        })
    })
}

struct PublishStage {
    path: PathBuf,
    root: Dir,
    dir: Dir,
    identity: FileIdentity,
}

/// The temporary tree ONE publication snapshot exclusively created, removed
/// when it drops.
///
/// It is a field rather than a `Drop` on the snapshot itself because
/// `Drop::drop` runs BEFORE a struct's fields drop: removing the tree from
/// there would remove the resolver root while the captured graph still holds
/// it. As the LAST field it is removed last.
struct PublicationSnapshotRoot(PathBuf);

impl Drop for PublicationSnapshotRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One publication's immutable document capture. The captured graph remains
/// the resolver for embeds, namespaces and query-result hydration; query
/// selection belongs to the caller-supplied current-main reader.
struct PublicationGraphSnapshot {
    graph: Graph,
    /// Held for its `Drop`, which removes the snapshot directory.
    _root: PublicationSnapshotRoot,
}

pub mod app_export;
#[cfg(windows)]
mod private_directory;
pub mod query_export;

/// Create `path` as a directory only its owner may read, write or traverse,
/// failing if it already exists.
///
/// The permission is chosen AT creation rather than relaxed afterwards, so no
/// window exists in which the publication's private rows are readable by
/// another account. Windows uses an explicit protected DACL and validates it
/// before the caller can write private rows; temp-directory inheritance is not
/// a privacy guarantee.
fn create_owner_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(windows)]
    {
        private_directory::create(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private publication directories are unsupported",
        ))
    }
}

impl PublicationGraphSnapshot {
    fn new(pages: Vec<(crate::model::PageEntry, Arc<doc::Document>)>) -> io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let temp = std::env::temp_dir();
        for _ in 0..128 {
            let root = temp.join(format!(
                "tine-publication-snapshot-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            match create_owner_private_dir(&root) {
                Ok(()) => {
                    return Ok(Self {
                        graph: Graph::from_page_snapshot(&root, pages),
                        _root: PublicationSnapshotRoot(root),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve an immutable publication snapshot root",
        ))
    }
}

struct PublishRecovery {
    /// Ambient spelling, reported to the user so retired content can be found.
    path: PathBuf,
    dir: Dir,
}

#[cfg(target_os = "windows")]
fn dir_identity(dir: &Dir, path: &Path) -> io::Result<FileIdentity> {
    let capability = identity_from_file(dir.try_clone()?.into_std_file())?;
    let share_delete = identity_from_path(path)?;
    if capability != share_delete {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "static-publish staging path changed while binding its identity",
        ));
    }
    Ok(share_delete)
}

#[cfg(not(target_os = "windows"))]
fn dir_identity(dir: &Dir, _path: &Path) -> io::Result<FileIdentity> {
    identity_from_file(dir.try_clone()?.into_std_file())
}

fn reserve_publish_stage(graph: &Graph) -> io::Result<PublishStage> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let root = Dir::open_ambient_dir(&graph.root, ambient_authority())?;
    for _ in 0..128 {
        let name = format!(
            ".tine-publish-stage-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let path = graph.root.join(&name);
        graph.ensure_write_target(&path)?;
        match root.create_dir(&name) {
            Ok(()) => {
                let dir = root.open_dir(&name)?;
                let identity = dir_identity(&dir, &path)?;
                return Ok(PublishStage {
                    path,
                    root,
                    dir,
                    identity,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a unique static-publish staging directory",
    ))
}

fn write_publish_stage_file(stage: &PublishStage, name: &str, bytes: &[u8]) -> io::Result<()> {
    let relative = Path::new(name);
    if relative.file_name().is_none_or(|value| value != name)
        || relative
            .parent()
            .is_some_and(|parent| parent != Path::new(""))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "static-publish output name must be one file",
        ));
    }
    publish_stage_write_race_hook(stage)?;
    // All generation is relative to the directory handle reserved above. A
    // rename plus symlink/junction replacement of the ambient stage pathname
    // therefore cannot redirect an open or truncate outside the graph.
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = stage.dir.open_with(relative, &options)?;
    file.write_all(bytes)?;
    crate::durability_counters::sync_file(&file)
}

#[cfg(test)]
thread_local! {
    static PUBLISH_STAGE_WRITE_SWAP: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    static PUBLISH_RECOVERY_SWAP: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

#[cfg(test)]
fn replace_bound_dir_path(path: &Path, outside: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let displaced = path.with_file_name(format!(
            "{}.displaced",
            path.file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("bound")
        ));
        fs::rename(path, &displaced)?;
        symlink(outside, path)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, outside);
        Ok(())
    }
}

#[cfg(test)]
fn publish_stage_write_race_hook(stage: &PublishStage) -> io::Result<()> {
    PUBLISH_STAGE_WRITE_SWAP.with(|outside| match outside.borrow_mut().take() {
        Some(outside) => replace_bound_dir_path(&stage.path, &outside),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn publish_stage_write_race_hook(_stage: &PublishStage) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn publish_recovery_race_hook(recovery: &PublishRecovery) -> io::Result<()> {
    PUBLISH_RECOVERY_SWAP.with(|outside| match outside.borrow_mut().take() {
        Some(outside) => replace_bound_dir_path(&recovery.path, &outside),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn publish_recovery_race_hook(_recovery: &PublishRecovery) -> io::Result<()> {
    Ok(())
}

fn reserve_publish_recovery(graph: &Graph, root: &Dir, leaf: &str) -> io::Result<PublishRecovery> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let recovery_rel = Path::new("logseq").join(".tine-trash").join("conflicts");
    let recovery = graph.root.join(&recovery_rel);
    graph.ensure_write_target(&recovery)?;
    root.create_dir_all(&recovery_rel)?;
    let recovery_root = root.open_dir(&recovery_rel)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    for _ in 0..128 {
        let name = format!(
            "{stamp}-{}__previous-{leaf}",
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        match recovery_root.create_dir(&name) {
            Ok(()) => {
                return Ok(PublishRecovery {
                    path: recovery.join(&name),
                    dir: recovery_root.open_dir(&name)?,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve static-publish recovery directory",
    ))
}

/// Install a complete stage at `output`. Returns where the previous occupant
/// was retired, when there was one. `replace: false` is create-only: a
/// directory that appeared since the user reviewed the destination makes the
/// commit fail with the stage retired into recovery and nothing else touched.
fn commit_publish_stage(
    graph: &Graph,
    stage: PublishStage,
    output: &PublicationOutput,
) -> io::Result<Option<PathBuf>> {
    let out = output.path(graph);
    let out = out.as_path();
    graph.ensure_write_target(out)?;
    // The parent (`published-queries/`) is created through the bound graph
    // capability; the graph site's parent is the root itself.
    let leaf_rel = output.relative();
    if output
        .parent
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
        || output.leaf.is_empty()
        || Path::new(&output.leaf)
            .file_name()
            .is_none_or(|value| value != output.leaf.as_str())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "static-publish output must be one directory name under the graph",
        ));
    }
    if !output.parent.as_os_str().is_empty() {
        graph.ensure_write_target(&graph.root.join(&output.parent))?;
        stage.root.create_dir_all(&output.parent)?;
    }
    // cap-std may represent a directory capability with an O_PATH descriptor on
    // Linux, which cannot itself be fsynced. Every generated file is fsynced;
    // directory durability remains best-effort, matching the other atomic paths.
    let _ = crate::durability_counters::sync_directory(&stage.dir.try_clone()?.into_std_file());
    let PublishStage {
        path,
        root,
        dir,
        identity,
    } = stage;
    // Windows refuses to rename a directory while this capability is open.
    // Every file is already synced and the stable identity above survives the
    // close for the post-move replacement check.
    drop(dir);

    // Reject a pre-existing alias without touching it. A replacement racing the
    // check is moved as an inode into bound recovery and rejected there; it is
    // never followed for a write.
    let old_recovery = match root.symlink_metadata(&leaf_rel) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "static-publish output is not a real directory",
                ));
            }
            if !output.replace {
                // Create-only: the reviewed destination was free and is not
                // any more. Retire our own stage so nothing dangles; the
                // occupant is untouched.
                let bad = reserve_publish_recovery(graph, &root, &output.leaf)?;
                let _ = root.rename(&path_rel(&graph.root, &path), &bad.dir, "unused-stage");
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "Another export appeared at {}; choose a different name or replace it.",
                        out.display()
                    ),
                ));
            }
            let recovery = reserve_publish_recovery(graph, &root, &output.leaf)?;
            publish_recovery_race_hook(&recovery)?;
            root.rename(&leaf_rel, &recovery.dir, "previous")?;
            let retired = recovery.dir.symlink_metadata("previous")?;
            if !retired.is_dir() || retired.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "static-publish output changed during retirement",
                ));
            }
            Some(recovery)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let retired_path = old_recovery
        .as_ref()
        .map(|recovery| recovery.path.join("previous"));

    if let Err(error) = crate::model::move_file_noreplace(&path, out) {
        // The previous site stays complete in conflict recovery. Avoid a
        // compare-then-replace restoration that could clobber a late winner.
        let _ = old_recovery;
        return Err(error);
    }
    let out_meta = fs::symlink_metadata(out)?;
    let same_stage = out_meta.is_dir()
        && !out_meta.file_type().is_symlink()
        && identity_from_path(out).is_ok_and(|live| live == identity);
    if same_stage {
        return Ok(retired_path);
    }

    // A replaced stage must never remain live. Move it through the bound graph
    // and recovery directory handles; the previous complete site is already
    // retained separately and is not overwritten during automatic recovery.
    let bad = reserve_publish_recovery(graph, &root, &output.leaf)?;
    let _ = root.rename(&leaf_rel, &bad.dir, "invalid-stage");
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "static-publish staging directory changed during commit",
    ))
}

/// Graph-relative spelling of a path known to sit directly under the root.
fn path_rel(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Export public pages to `<root>/publish/`. Returns (output dir, page count).
/// Only pages with `public:: true` are published, unless
/// `:publishing/all-pages-public?` is set in config (matching Logseq).
pub fn publish_graph(graph: &Graph) -> io::Result<(String, usize)> {
    let (pages, sources) = capture_direct_publication_sources(graph)?;
    graph.with_publication_query_reader(&sources, |reader| {
        publish_graph_documents_with_queries(graph, pages, reader)
    })
}

/// Fresh parse of every listed page: the documents the renderer will see and
/// the `(entry, revision)` pairs the query reader must correspond to.
#[allow(clippy::type_complexity)]
pub(crate) fn capture_direct_publication_sources(
    graph: &Graph,
) -> io::Result<(
    Vec<(crate::model::PageEntry, doc::Document)>,
    Vec<(crate::model::PageEntry, String)>,
)> {
    let mut pages = Vec::new();
    let mut sources = Vec::new();
    for listed in graph.list_pages() {
        let content = fs::read_to_string(&listed.path)?;
        let (entry, mut document, revision) =
            crate::model::parse_exact_page(graph, &listed, &content)?;
        crate::model::assign_doc_runtime_ids(&mut document.roots, &entry.rel_path);
        sources.push((entry.clone(), revision));
        pages.push((entry, document));
    }
    Ok((pages, sources))
}

/// Plan a query export on Direct Files: fresh capture, one coherent reader.
pub fn plan_query_publication(
    graph: &Graph,
    request: &query_export::QueryPublicationRequest,
) -> Result<query_export::QueryPublicationPlan, query_export::QueryPublicationError> {
    let (_, sources) = capture_direct_publication_sources(graph)?;
    graph.with_publication_query_reader(&sources, |reader| {
        Ok(
            query_export::plan_query_publication(graph, &sources, reader, request)
                .map(|(plan, _)| plan),
        )
    })?
}

/// Confirmed query export on Direct Files.
pub fn publish_query(
    graph: &Graph,
    request: &query_export::QueryPublicationRequest,
    fingerprint: &str,
) -> Result<PublishOutcome, query_export::QueryPublicationError> {
    let (pages, sources) = capture_direct_publication_sources(graph)?;
    let capture = pages
        .into_iter()
        .zip(sources.iter())
        .map(|((entry, document), (_, revision))| (entry, document, revision.clone()))
        .collect();
    graph.with_publication_query_reader(&sources, |reader| {
        Ok(query_export::publish_query_documents(
            graph,
            capture,
            reader,
            request,
            fingerprint,
        ))
    })?
}

/// Publish one already-authoritative graph snapshot. Direct Files parses its
/// fresh files immediately before entering the same renderer.
#[cfg(test)]
fn publish_graph_documents(
    graph: &Graph,
    pages: Vec<(crate::model::PageEntry, doc::Document)>,
) -> io::Result<(String, usize)> {
    publish_graph_documents_inner(graph, pages, None, &PublicationTarget::graph_site())
        .map(|outcome| (outcome.path, outcome.pages))
}

/// Render one already-authoritative document capture while every query surface
/// reads the supplied coherent current-main snapshot.
pub(crate) fn publish_graph_documents_with_queries(
    graph: &Graph,
    pages: Vec<(crate::model::PageEntry, doc::Document)>,
    queries: &dyn PublicationQueryRead,
) -> io::Result<(String, usize)> {
    publish_graph_documents_inner(
        graph,
        pages,
        Some(queries),
        &PublicationTarget::graph_site(),
    )
    .map(|outcome| (outcome.path, outcome.pages))
}

/// Which pages a static export contains. The renderer computes every closure
/// index (slugs, block refs, backlinks, embeds, nested-query hydration, search)
/// over exactly this set, so the selection is the whole privacy boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationSelection {
    /// The graph site: pages carrying `public:: true`, or every page under
    /// `:publishing/all-pages-public?`. Ambiguous identities are skipped.
    PublicProperty,
    /// A closed, user-reviewed set of graph-relative source paths (a query
    /// export). A selected path that is missing from the capture, or whose
    /// logical name has a physical twin, refuses the whole export: the user
    /// reviewed a list, so silently shipping fewer pages is the wrong outcome.
    Paths(std::collections::BTreeSet<String>),
}

/// Where a static export is installed: `<graph root>/<parent>/<leaf>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationOutput {
    /// Graph-relative parent directory; empty for the graph site.
    pub parent: PathBuf,
    /// The leaf directory name (`publish`, or a query export's folder).
    pub leaf: String,
    /// `true`: whatever occupies the leaf at commit time is retired into
    /// conflict recovery before the stage is installed. `false`: the install
    /// is create-only and fails if anything is there.
    pub replace: bool,
}

impl PublicationOutput {
    /// `<graph root>/<parent>/<leaf>`.
    pub fn path(&self, graph: &Graph) -> PathBuf {
        graph.root.join(&self.parent).join(&self.leaf)
    }
    fn relative(&self) -> PathBuf {
        self.parent.join(&self.leaf)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationTarget {
    pub selection: PublicationSelection,
    pub output: PublicationOutput,
    /// `Some(budget)`: the output is a movable folder — copy every referenced
    /// local asset into `<leaf>/assets/` under this byte budget. `None`: the
    /// site keeps linking the graph's sibling `assets/` directory.
    pub asset_budget_bytes: Option<u64>,
    /// `Some`: also publish the read-only app under `app/` (query exports).
    pub app: Option<app_export::AppPublication>,
}

impl PublicationTarget {
    /// The historical `publish/` site: `public::` pages, replaced in place.
    pub fn graph_site() -> Self {
        PublicationTarget {
            selection: PublicationSelection::PublicProperty,
            output: PublicationOutput {
                parent: PathBuf::new(),
                leaf: "publish".to_string(),
                replace: true,
            },
            asset_budget_bytes: None,
            app: None,
        }
    }
}

/// What one static export produced.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishOutcome {
    /// Installed output directory.
    pub path: String,
    /// Pages written.
    pub pages: usize,
    /// Where the previous occupant of the output directory was retired, when
    /// there was one. Always reported: the user must be able to find content
    /// that appeared between review and commit.
    pub retired: Option<String>,
    /// Non-fatal omissions (an asset over budget or missing), user-facing.
    #[serde(default)]
    pub warnings: Vec<String>,
}

pub(crate) fn publish_graph_documents_inner(
    graph: &Graph,
    pages: Vec<(crate::model::PageEntry, doc::Document)>,
    queries: Option<&dyn PublicationQueryRead>,
    target: &PublicationTarget,
) -> io::Result<PublishOutcome> {
    let out = target.output.path(graph);
    graph.ensure_write_target(&out)?;
    let stage = reserve_publish_stage(graph)?;
    write_publish_stage_file(&stage, "style.css", STYLE.as_bytes())?;
    // Sidebar + fuzzy search are JS-driven: Fuse (vendored, OG's version) + our tiny
    // app.js, both loaded as `<script src>` so they work offline / over file://.
    write_publish_stage_file(
        &stage,
        "fuse.min.js",
        include_str!("../assets/fuse.min.js").as_bytes(),
    )?;
    write_publish_stage_file(&stage, "app.js", APP_JS.as_bytes())?;
    write_publish_stage_file(&stage, "enhance.js", ENHANCE_JS.as_bytes())?;
    let all_public = graph.config.all_pages_public;
    let favorites: HashSet<&str> = graph.config.favorites.iter().map(|s| s.as_str()).collect();

    let snapshot_pages = pages
        .into_iter()
        .map(|(entry, document)| (entry, Arc::new(document)))
        .collect::<Vec<_>>();
    // Query/reference DTOs currently identify their source by logical page name.
    // If two physical files claim that identity, a name-only authorization check
    // cannot prove which file produced a result. Fail closed for that identity:
    // publish neither twin rather than let a private twin borrow the public
    // capability. Ordinary unique pages retain the exact one-file capability.
    let mut source_identity_counts: HashMap<String, usize> = HashMap::new();
    for (page, _) in &snapshot_pages {
        *source_identity_counts
            .entry(crate::refs::page_key(&page.name))
            .or_default() += 1;
    }
    let mut entries = snapshot_pages.iter().collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| left.name.cmp(&right.name));

    // Pass 1: retain the immutable captured documents, while keeping only
    // authorized pages in the publication projection. `entries` is already
    // sorted by name, so `public` (and hence the slug assignment below) is
    // deterministic across runs. Query selection sees the complete capture
    // through the supplied reader; result hydration still comes exclusively
    // from `public` below.
    let mut public: Vec<(&str, PageKind, Arc<doc::Document>)> = Vec::new();
    let mut public_paths = Vec::new();
    let mut selected_seen = 0usize;
    for (e, parsed) in entries {
        let is_public = match &target.selection {
            PublicationSelection::PublicProperty => {
                all_public || page_is_public(parsed.pre_block.as_deref())
            }
            PublicationSelection::Paths(paths) => paths.contains(&e.rel_path),
        };
        if !is_public {
            continue;
        }
        if source_identity_counts
            .get(&crate::refs::page_key(&e.name))
            .copied()
            .unwrap_or(0)
            != 1
        {
            if let PublicationSelection::Paths(_) = &target.selection {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "Two files claim the page \"{}\"; resolve the duplicate before exporting it.",
                        e.name
                    ),
                ));
            }
            if crate::backend_error::runtime_debug_diagnostics_enabled() {
                eprintln!("tine export: refusing one ambiguous public page identity");
            }
            continue;
        }
        selected_seen += 1;
        public.push((e.name.as_str(), e.kind, Arc::clone(parsed)));
        public_paths.push((e.rel_path.clone(), e.name.clone(), e.kind));
    }
    if let PublicationSelection::Paths(paths) = &target.selection {
        if selected_seen != paths.len() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "The reviewed pages changed before the export ran; review them again.",
            ));
        }
    }

    // Every downstream document resolver gets the same exact revision as the
    // visibility pass. This captured graph serves embeds, namespaces and
    // admitted-result hydration only; it is not a query selection source. The
    // public-page maps remain the sole HTML hydration/link capabilities.
    let mut snapshot = PublicationGraphSnapshot::new(snapshot_pages.clone())?;
    // The render pass reads user presentation settings from the render-time
    // graph's config (sheet board workflow order, user's hidden block
    // properties). The snapshot graph starts from a bare temp root, so copy
    // exactly those presentation settings — never scope-affecting settings
    // like `:hidden` or `:publishing/all-pages-public?`, which the publish
    // projection resolves from the source graph above.
    snapshot.graph.config.preferred_workflow = graph.config.preferred_workflow;
    snapshot.graph.config.block_hidden_properties = graph.config.block_hidden_properties.clone();

    // ONE source of truth: a unique, nonempty name→slug map for the exported set.
    // Every filename, cross-page link, block-ref target, and search-index entry is
    // driven from this map, so a link can never point at a file that a later page
    // overwrote (DS#4). `slug(name)` is never recomputed independently downstream.
    let names: Vec<&str> = public.iter().map(|(n, _, _)| *n).collect();
    let (slugs, collisions) = build_slug_map(&names);
    if !collisions.is_empty() && crate::backend_error::runtime_debug_diagnostics_enabled() {
        eprintln!(
            "tine export: resolved {} public-page slug collisions",
            collisions.len()
        );
    }
    let slug_of = |name: &str| -> String {
        slugs
            .get(&name.to_lowercase())
            .cloned()
            .unwrap_or_else(|| slug(name))
    };
    let welcome_slug = slugs.get("welcome to tine").cloned();
    let home_file = welcome_slug
        .as_ref()
        .map(|slug| format!("{slug}.html"))
        .unwrap_or_else(|| "index.html".to_string());
    let page_links = public_paths
        .into_iter()
        .map(|(path, name, kind)| {
            let slug = slug_of(&name);
            (path, PublicationPageLink { name, kind, slug })
        })
        .collect::<HashMap<_, _>>();

    // Build the block-ref index from the public pages, keyed to their final slugs
    // (a `((ref))` only resolves to a block that's actually exported).
    let mut refs = RefIndex::new();
    for (name, _, parsed) in &public {
        collect_block_refs(&parsed.roots, &slug_of(name), &mut refs);
    }

    let page_docs: HashMap<String, &doc::Document> = public
        .iter()
        .map(|(name, _, parsed)| (crate::refs::page_key(name), parsed.as_ref()))
        .collect();
    let mut reverse_refs = ReverseRefIndex::new();
    for (name, _, parsed) in &public {
        let mut counter = 0;
        collect_reverse_refs(
            &parsed.roots,
            &slug_of(name),
            name,
            &mut counter,
            &refs,
            &mut reverse_refs,
        );
    }
    // Pass 2: render each public page (collecting the per-block search index along
    // the way), accumulate the sidebar page index (`__tinePages`) and the static
    // no-JS all-pages list shown in the index page's <main>.
    let mut index_list = String::new();
    let mut all_blocks: Vec<serde_json::Value> = Vec::new();
    let mut sidebar_pages: Vec<serde_json::Value> = Vec::new();
    let mut welcome_html: Option<String> = None;
    let mut count = 0;
    // A closed export travels: copy its referenced assets in. The graph site
    // keeps linking the sibling `assets/` folder as it always has.
    let asset_sink = target.asset_budget_bytes.map(|budget| {
        RefCell::new(AssetSink {
            source_root: &graph.root,
            stage: &stage,
            copied: HashMap::new(),
            budget,
            remaining: budget,
            warnings: Vec::new(),
            over_budget: None,
        })
    });
    // Stage 2: a query export runs every query as the app will ask for it
    // (`scope`) and, when it also ships the app, records each answer.
    let is_query_export = matches!(target.selection, PublicationSelection::Paths(_));
    let scope = is_query_export.then(|| RefCell::new(app_export::RenderScope::default()));
    let recorder = target
        .app
        .as_ref()
        .filter(|app| app.bundle.index().is_some())
        .map(|_| RefCell::new(app_export::QueryRecorder::default()));
    // The render context: the block-ref index + the graph (so `{{query}}`/`{{embed}}`/
    // `{{namespace}}` macros can resolve against real data at publish time) + the
    // slug map (so cross-page links resolve to the actual written files).
    let ctx = Ctx {
        refs: &refs,
        reverse_refs: Some(&reverse_refs),
        graph: Some(&snapshot.graph),
        slugs: Some(&slugs),
        inline_assets: false,
        print_asset_budget: None,
        query_reader: queries,
        print_error: None,
        pages: Some(&page_docs),
        page_links: Some(&page_links),
        inert_outside_links: is_query_export,
        scope: scope.as_ref(),
        recorder: recorder.as_ref(),
        asset_sink: asset_sink.as_ref(),
    };
    for (name, kind, parsed) in &public {
        if let Some(scope) = &scope {
            let mut scope = scope.borrow_mut();
            scope.page = Some((*name).to_string());
            scope.block_properties.clear();
            scope.nesting = 0;
        }
        let slug = slug_of(name);
        let file = format!("{slug}.html");
        let html = page_html(
            name,
            &slug,
            parsed,
            *kind,
            &ctx,
            &mut all_blocks,
            &home_file,
        );
        if name.eq_ignore_ascii_case("Welcome to Tine") {
            welcome_html = Some(html.clone());
        }
        write_publish_stage_file(&stage, &file, html.as_bytes())?;
        let journal = *kind == PageKind::Journal;
        let tag = if journal {
            "<span class=\"k\">journal</span>"
        } else {
            ""
        };
        index_list.push_str(&format!(
            "<li><a class=\"ref\" href=\"{}\">{}</a>{}</li>",
            file,
            esc(name),
            tag
        ));
        sidebar_pages.push(json!({
            "title": *name,
            "slug": slug,
            "journal": journal,
            "favorite": favorites.contains(*name),
        }));
        count += 1;
    }

    // Embedded search data, read by app.js as `<script>` globals (never fetched, so
    // the site works offline / over file://). External .js ⇒ serde escaping +
    // no `</script>`-in-content break.
    let data = format!(
        "window.__tinePages={};\nwindow.__tineBlocks={};\n",
        serde_json::to_string(&sidebar_pages).unwrap_or_else(|_| "[]".into()),
        serde_json::to_string(&all_blocks).unwrap_or_else(|_| "[]".into()),
    );
    write_publish_stage_file(&stage, "search-index.js", data.as_bytes())?;

    // Keep the alphabetical page list separately discoverable. When the public
    // set contains Welcome to Tine, index.html is that actual rendered page and
    // every persistent Home link targets its slug. Without it, retain the old
    // page-list index as a safe fallback.
    let main = format!(
        "<h1 class=\"page\">Pages</h1><ul class=\"outline index-list\">{}</ul>\
<footer>Published with Tine</footer>",
        index_list
    );
    let pages_html = shell("Pages", &main, &home_file);
    write_publish_stage_file(&stage, "pages.html", pages_html.as_bytes())?;
    let mut entry_html = welcome_html.unwrap_or(pages_html);
    let mut app_warnings = Vec::new();
    if let Some(app) = &target.app {
        match (app.bundle.index(), &recorder) {
            (Some(index), Some(recorder)) => {
                let index_html = app_export::rewrite_index(index, &app.name)?;
                // The export's own query, run for the app's home page: its
                // lookup key is the home page, its binding and view are the
                // reviewed ones, and the host block stays out (GH #469).
                let taken: HashSet<String> = public
                    .iter()
                    .map(|(name, _, _)| crate::refs::page_key(name))
                    .collect();
                let home_name = app_export::home_page_name(&app.name, &taken);
                if let Some(scope) = &scope {
                    let mut scope = scope.borrow_mut();
                    scope.page = Some(home_name.clone());
                    scope.block_properties = app.query.properties();
                    scope.nesting = 0;
                }
                let input = app.query.dialect().input();
                let overrides = QueryRunOverrides {
                    context: Some(ExecutionContext {
                        current_page: app.query.current_page.clone(),
                    }),
                    view: app.query.view.clone(),
                    properties: Some(app.query.properties()),
                    host_block_id: app.query.host_block_id.clone(),
                };
                run_static_query_with(
                    &app.query.argument(),
                    input,
                    !matches!(input, crate::query::QueryInput::MacroTql),
                    &ctx,
                    overrides,
                )
                .map_err(|html| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "The export's query could not be run for the app: {}",
                            app_export::strip_tags(&html)
                        ),
                    )
                })?;
                // The closed sub-graph: every graph-shaped answer the app
                // needs (backlinks, block-ref counts, aliases, icons) comes
                // from the selected pages and nothing else (I-8).
                let selected: HashSet<String> = taken;
                let mut closed_pages: Vec<(crate::model::PageEntry, Arc<doc::Document>)> =
                    snapshot_pages
                        .iter()
                        .filter(|(entry, _)| selected.contains(&crate::refs::page_key(&entry.name)))
                        .map(|(entry, document)| {
                            let mut document = Arc::clone(document);
                            crate::model::assign_doc_runtime_ids(
                                &mut Arc::make_mut(&mut document).roots,
                                &entry.rel_path,
                            );
                            (entry.clone(), document)
                        })
                        .collect();
                closed_pages.sort_by(|(left, _), (right, _)| left.name.cmp(&right.name));
                let closed = PublicationGraphSnapshot::new(closed_pages.clone())?;
                let queries = std::mem::take(&mut recorder.borrow_mut().queries);
                let snapshot_json = app_export::build_snapshot(app_export::SnapshotInputs {
                    name: &app.name,
                    home: &app.query,
                    closed: &closed.graph,
                    pages: &closed_pages,
                    queries,
                    home_name: &home_name,
                    exported_at: app_export::exported_at_now(),
                })?;
                for (path, bytes) in &app.bundle.files {
                    if path == "index.html" {
                        continue;
                    }
                    write_publish_stage_nested(
                        &stage,
                        app_export::APP_DIR,
                        &format!("{}/{path}", app_export::APP_DIR),
                        bytes,
                    )?;
                }
                write_publish_stage_nested(
                    &stage,
                    app_export::APP_DIR,
                    &format!("{}/index.html", app_export::APP_DIR),
                    index_html.as_bytes(),
                )?;
                write_publish_stage_nested(
                    &stage,
                    app_export::APP_DIR,
                    &format!("{}/{}", app_export::APP_DIR, app_export::SNAPSHOT_FILE),
                    &snapshot_json,
                )?;
                // The static site stays the no-JS fallback; served over HTTP
                // its front door opens the app unless asked not to (`?static`).
                // The redirect is a separate file because the shell's CSP
                // allows no inline script.
                write_publish_stage_file(
                    &stage,
                    app_export::REDIRECT_FILE,
                    app_export::REDIRECT_JS.as_bytes(),
                )?;
                entry_html = entry_html
                    .replacen(
                        "<head>",
                        &format!(
                            "<head><script src=\"{}\"></script>",
                            app_export::REDIRECT_FILE
                        ),
                        1,
                    )
                    .replacen(
                        "<main>",
                        &format!("<main>{}", app_export::STATIC_APP_NOTE),
                        1,
                    );
            }
            _ => app_warnings.push(app_export::NO_BUNDLE_WARNING.to_string()),
        }
    }
    write_publish_stage_file(&stage, "index.html", entry_html.as_bytes())?;
    if let Some(queries) = queries {
        queries
            .ensure_current()
            .map_err(publication_query_io_error)?;
    }
    let (mut warnings, over_budget) = asset_sink
        .as_ref()
        .map(|sink| {
            let sink = sink.borrow();
            (sink.warnings.clone(), sink.over_budget.clone())
        })
        .unwrap_or_default();
    warnings.extend(app_warnings);
    drop(ctx);
    if let Some(message) = over_budget {
        // Nothing reaches the destination: the stage is removed with the
        // snapshot root, and whatever occupied the folder before stays.
        return Err(io::Error::other(AssetBudgetExceeded(message)));
    }
    let retired = commit_publish_stage(graph, stage, &target.output)?;
    Ok(PublishOutcome {
        path: out.display().to_string(),
        pages: count,
        retired: retired.map(|path| path.display().to_string()),
        warnings,
    })
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod tests;
