//! Publish a query, Stage 2: the read-only app over a baked snapshot.
//!
//! A query export (`published-queries/<folder>/`) additionally carries
//! `app/`: the frontend bundle the exporting binary itself embeds, plus
//! `app/snapshot.json` — every answer the app's `Backend` will need, computed
//! natively at export time by the ONE engine (I-12) from the selection-closed
//! sub-graph and nothing else (I-8). The static site stays the no-JS fallback;
//! its `index.html` redirects to `app/` when served over HTTP.
//!
//! Spec: `tine-agents/specs/notes/2026-09-14-publish-query-stage2.md`.

use crate::doc;
use crate::model::{Graph, PageDto, PageEntry, PageKind, RefGroup};
use crate::query::ir::{ExecutionContext, QueryResult, ViewSettings};
use crate::query::wire_parse::{ParsedQuery, QueryTextDialect};
use crate::query::QueryDialect;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::io;
use std::sync::Arc;

/// The app lives beside the static site, inside the same committed leaf.
pub const APP_DIR: &str = "app";
pub const SNAPSHOT_FILE: &str = "snapshot.json";
/// The `<meta name>` the exported shell carries; the frontend selects the
/// snapshot backend on its presence.
pub const PUBLISHED_META_NAME: &str = "tine-published";
pub const SNAPSHOT_SCHEMA: u32 = 1;
/// The static index's redirect lives in its own file: the static shell's CSP
/// allows only `'self'` scripts, never inline ones.
pub const REDIRECT_FILE: &str = "app-redirect.js";
/// §3 of the spec, asserted by a doc-code test.
pub const SNAPSHOT_TOP_LEVEL_KEYS: [&str; 11] = [
    "schema",
    "name",
    "exported_at",
    "home",
    "pages",
    "entries",
    "backlinks",
    "block_ref_counts",
    "aliases",
    "icons",
    "queries",
];
/// The warning a build that embeds no frontend reports instead of an `app/`.
pub const NO_BUNDLE_WARNING: &str =
    "App version not included: this build does not embed the frontend.";

/// The frontend bundle as the running binary embeds it. Collected by the
/// Tauri layer (tine-core never links Tauri) and attached to the request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PublishedAppBundle {
    /// Bundle-relative path (no leading slash) → bytes, already filtered to
    /// the shipped set (`index.html` and `assets/*`).
    pub files: Vec<(String, Vec<u8>)>,
}

impl PublishedAppBundle {
    /// Whether one embedded asset ships with an export: the shell and the
    /// hashed bundle only. Twemoji (17 MB; the app forces native emoji),
    /// theme thumbnails and the capture window stay behind.
    pub fn ships(path: &str) -> bool {
        let path = path.trim_start_matches('/');
        path == "index.html"
            || (path.starts_with("assets/")
                && !path.contains("/../")
                && !path.ends_with('/')
                && path.len() > "assets/".len())
    }

    /// Build from an embedded-asset iterator (Tauri's `AssetResolver::iter`).
    pub fn from_embedded<'a>(
        assets: impl Iterator<Item = (std::borrow::Cow<'a, str>, std::borrow::Cow<'a, [u8]>)>,
    ) -> Self {
        let mut files: Vec<(String, Vec<u8>)> = assets
            .filter(|(path, _)| Self::ships(path))
            .map(|(path, bytes)| (path.trim_start_matches('/').to_string(), bytes.into_owned()))
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        Self { files }
    }

    pub fn index(&self) -> Option<&[u8]> {
        self.files
            .iter()
            .find(|(path, _)| path == "index.html")
            .map(|(_, bytes)| bytes.as_slice())
    }
}

/// The exported query as the app's home page will carry it (§3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeQuery {
    /// The EFFECTIVE source the dialog reviewed (after any focused-page
    /// substitution), as the request carries it.
    pub source: String,
    pub advanced: bool,
    pub simple_dialect: Option<QueryDialect>,
    /// The reviewed `?current-page` binding.
    pub current_page: Option<String>,
    /// The reviewed view (sampling lives here).
    pub view: Option<ViewSettings>,
    /// The block the query was written in, dropped from its results exactly
    /// as the plan dropped it (GH #469).
    pub host_block_id: Option<String>,
    /// The host block's `tine.*` properties (view, sample, sort…). Written
    /// onto the home block so the app resolves the same display the query
    /// was exported under; a `#+BEGIN_QUERY` container cannot carry block
    /// properties, so an advanced home keeps its reviewed `view` only for
    /// the native run.
    pub host_properties: Vec<(String, String)>,
}

impl HomeQuery {
    /// Which macro the home block writes. Advanced queries take OG's
    /// `#+BEGIN_QUERY` container (a `{{query {…}}}` macro cannot carry a map).
    pub fn macro_name(&self) -> &'static str {
        match (self.advanced, self.simple_dialect) {
            (true, _) => "begin-query",
            (false, Some(QueryDialect::Tql)) => "tine-query",
            _ => "query",
        }
    }
    pub fn dialect(&self) -> QueryTextDialect {
        match self.macro_name() {
            "tine-query" => QueryTextDialect::MacroTql,
            _ => QueryTextDialect::MacroQuery,
        }
    }
    /// The argument the app's `parseQuery` receives for the home block: the
    /// macro argument, or for a `BEGIN_QUERY` container what `BeginQuery.tsx`
    /// composes from it (`<query> {:table-view? true}`).
    pub fn argument(&self) -> String {
        let source = self.source.trim();
        if self.advanced {
            format!("{source} {{:table-view? true}}")
        } else {
            source.to_string()
        }
    }
    /// The one block the home page opens with: the macro line, then the host
    /// block's `tine.*` properties (a simple macro only — see `host_properties`).
    pub fn block_text(&self) -> String {
        let source = self.source.trim();
        if self.advanced {
            format!("#+BEGIN_QUERY\n  {{:query {source}}}\n  #+END_QUERY")
        } else {
            let mut text = format!("{{{{{} {source}}}}}", self.macro_name());
            for (key, value) in self.properties() {
                text.push_str("\n  ");
                text.push_str(&key);
                text.push_str(":: ");
                text.push_str(&value);
            }
            text
        }
    }
    /// The properties the app's `parseQuery` receives for the home block:
    /// the host's `tine.*` properties, single-line values only (a multi-line
    /// value cannot be a block property), none for a `#+BEGIN_QUERY` home.
    pub fn properties(&self) -> Vec<(String, String)> {
        if self.advanced {
            return Vec::new();
        }
        self.host_properties
            .iter()
            .filter(|(key, value)| {
                key.starts_with("tine.") && !value.contains('\n') && !key.contains('\n')
            })
            .map(|(key, value)| (key.clone(), value.trim().to_string()))
            .collect()
    }
}

/// What a query export asks for beyond the static site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPublication {
    pub name: String,
    pub bundle: Arc<PublishedAppBundle>,
    pub query: HomeQuery,
}

/// The execution-side parse of a `<% current page %>` macro: the substituted
/// text and its own `parseQuery` answer, which is what the app runs.
#[derive(Debug, Clone, Serialize)]
pub struct QueryExecutionParse {
    pub argument: String,
    pub parsed: ParsedQuery,
}

/// One recorded query execution, keyed exactly as the frontend will ask
/// (§3.2): `parseQuery(argument, dialect, properties)` then
/// `queryRun(parsed.query, view, context)`.
#[derive(Debug, Clone, Serialize)]
pub struct QuerySnapshot {
    pub host: String,
    pub argument: String,
    pub dialect: QueryTextDialect,
    pub properties: Vec<(String, String)>,
    pub parsed: ParsedQuery,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<QueryExecutionParse>,
    /// The key the app asks with.
    pub context: ExecutionContext,
    /// What the run actually bound (differs from `context` only for the home
    /// page, whose query keeps its reviewed binding).
    pub executed_context: ExecutionContext,
    pub view: ViewSettings,
    /// The closed envelope: filtered rows, filtered counts, no statistics.
    pub result: QueryResult,
}

/// Collects [`QuerySnapshot`]s while the static renderer runs; attached to the
/// render context of a query export.
#[derive(Debug, Default)]
pub(crate) struct QueryRecorder {
    pub queries: Vec<QuerySnapshot>,
}

/// Where the renderer is: the page being rendered and the host block's
/// `tine.*` properties — the two inputs the frontend adds to a macro's text.
#[derive(Debug, Clone, Default)]
pub(crate) struct RenderScope {
    pub page: Option<String>,
    pub block_properties: Vec<(String, String)>,
    /// >0 while query RESULT rows are being rendered: a macro inside a result
    /// row belongs to another page's context and is not one the app asks for.
    pub nesting: u8,
}

/// The host block's `tine.*` properties exactly as `Macro.tsx` hands them to
/// `parseQuery` (`blockDirectives`: `key.startsWith("tine.")`).
pub(crate) fn tine_block_properties(block: &doc::DocBlock) -> Vec<(String, String)> {
    block
        .properties()
        .into_iter()
        .filter(|(key, _)| key.starts_with("tine."))
        .collect()
}

/// `Macro.tsx`'s execution substitution: `<% current page %>` → `[[page]]`,
/// case-insensitive, any inner whitespace. `None` when the marker is absent.
pub(crate) fn substitute_current_page(argument: &str, page: &str) -> Option<String> {
    let mut out = String::with_capacity(argument.len());
    let mut rest = argument;
    let mut replaced = false;
    while let Some(start) = rest.find("<%") {
        let Some(len) = rest[start..].find("%>") else {
            break;
        };
        let inner = &rest[start + 2..start + len];
        if inner.trim().eq_ignore_ascii_case("current page") {
            out.push_str(&rest[..start]);
            out.push_str("[[");
            out.push_str(page);
            out.push_str("]]");
            replaced = true;
        } else {
            out.push_str(&rest[..start + len + 2]);
        }
        rest = &rest[start + len + 2..];
    }
    if !replaced {
        return None;
    }
    out.push_str(rest);
    Some(out)
}

/// The result envelope the app receives: only what the filtered rows say.
/// `total`/`matched_total` are the filtered count, statistics are dropped
/// (aggregates over the whole graph would leak), `exceeded` is false; the
/// diagnostics and support report describe the query, not the data, and stay.
pub(crate) fn closed_result(
    rows: crate::query::ir::QueryRows,
    diagnostics: Vec<crate::query::ir::Diagnostic>,
    report: crate::query::ir::QueryReport,
) -> QueryResult {
    let total = match &rows {
        crate::query::ir::QueryRows::Block { groups } => {
            groups.iter().map(|group| group.blocks.len()).sum()
        }
        crate::query::ir::QueryRows::Page { pages } => pages.len(),
    };
    QueryResult {
        rows,
        diagnostics,
        report,
        total,
        matched_total: Some(total),
        statistics: None,
        exceeded: false,
    }
}

/// The home page's name: the export name, unless a selected page already owns
/// that identity — then `<name> (export)`, `<name> (export 2)`, … (§3.1).
pub(crate) fn home_page_name(export_name: &str, taken: &HashSet<String>) -> String {
    let base = export_name.trim();
    let base = if base.is_empty() { "Export" } else { base };
    if !taken.contains(&crate::refs::page_key(base)) {
        return base.to_string();
    }
    let mut n = 1usize;
    loop {
        let candidate = if n == 1 {
            format!("{base} (export)")
        } else {
            format!("{base} (export {n})")
        };
        if !taken.contains(&crate::refs::page_key(&candidate)) {
            return candidate;
        }
        n += 1;
    }
}

/// The home page's markdown: the exported macro, then one link per page.
pub(crate) fn home_markdown(query: &HomeQuery, pages: &[&str]) -> String {
    let mut out = format!("- {}\n", query.block_text());
    for page in pages {
        out.push_str(&format!("- [[{page}]]\n"));
    }
    out
}

/// `app/index.html`: the bundle's shell with the export name as its title and
/// the published marker in `<head>`. A shell without `<head>` cannot carry the
/// marker and is refused rather than shipped broken.
pub(crate) fn rewrite_index(index: &[u8], name: &str) -> io::Result<String> {
    let html = std::str::from_utf8(index).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "The app bundle is not exportable.",
        )
    })?;
    let lower = html.to_ascii_lowercase();
    let Some(head) = lower.find("<head>") else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "The app bundle is not exportable.",
        ));
    };
    let insert_at = head + "<head>".len();
    let meta = format!("<meta name=\"{PUBLISHED_META_NAME}\" content=\"{SNAPSHOT_FILE}\">",);
    let mut out = String::with_capacity(html.len() + meta.len() + name.len());
    out.push_str(&html[..insert_at]);
    out.push_str(&meta);
    out.push_str(&html[insert_at..]);
    let title = format!("<title>{}</title>", super::esc(name));
    Ok(
        match (
            out.to_ascii_lowercase().find("<title>"),
            out.to_ascii_lowercase().find("</title>"),
        ) {
            (Some(start), Some(end)) if end > start => {
                let close = end + "</title>".len();
                format!("{}{}{}", &out[..start], title, &out[close..])
            }
            _ => out,
        },
    )
}

/// The static site's redirect: over HTTP(S) open the app unless `?static`.
pub(crate) const REDIRECT_JS: &str = "(function () {\n\
  try {\n\
    var p = location.protocol;\n\
    if ((p === \"http:\" || p === \"https:\") && !/[?&]static(=|&|$)/.test(location.search)) {\n\
      location.replace(\"app/\" + location.hash);\n\
    }\n\
  } catch (_) {}\n\
})();\n";

/// The one sentence the static index says about the app version.
pub(crate) const STATIC_APP_NOTE: &str = "<p class=\"publish-app-note\">Serve this folder over HTTP to open it as an app (add <code>?static</code> to stay on this page).</p>";

pub(crate) struct SnapshotInputs<'a> {
    pub name: &'a str,
    pub home: &'a HomeQuery,
    /// The selection-closed sub-graph (built from the public projection only).
    pub closed: &'a Graph,
    /// The public projection: entry + captured document, sorted by name.
    pub pages: &'a [(PageEntry, Arc<doc::Document>)],
    pub queries: Vec<QuerySnapshot>,
    pub home_name: &'a str,
    pub exported_at: String,
}

#[derive(Serialize)]
struct Snapshot<'a> {
    schema: u32,
    name: &'a str,
    exported_at: &'a str,
    home: &'a str,
    pages: Vec<PageDto>,
    entries: Vec<PageEntry>,
    backlinks: BTreeMap<String, Vec<RefGroup>>,
    block_ref_counts: BTreeMap<String, usize>,
    aliases: Vec<(String, String)>,
    icons: BTreeMap<String, String>,
    queries: Vec<QuerySnapshot>,
}

/// `app/snapshot.json` bytes for one export (§3).
pub(crate) fn build_snapshot(inputs: SnapshotInputs<'_>) -> io::Result<Vec<u8>> {
    let closed = inputs.closed;
    let mut pages = Vec::with_capacity(inputs.pages.len() + 1);
    let names: Vec<&str> = inputs
        .pages
        .iter()
        .map(|(entry, _)| entry.name.as_str())
        .collect();
    let home_markdown = home_markdown(inputs.home, &names);
    let mut home =
        crate::model::markdown_page_dto(inputs.home_name, inputs.home_name, &home_markdown)?;
    home.read_only = true;
    pages.push(home);
    for (entry, document) in inputs.pages {
        let mut dto = crate::model::page_dto_checked(entry, document)?;
        dto.read_only = true;
        dto.path = entry.rel_path.clone();
        pages.push(dto);
    }
    let mut entries = Vec::with_capacity(inputs.pages.len() + 1);
    entries.push(PageEntry {
        name: inputs.home_name.to_string(),
        kind: PageKind::Page,
        date_key: None,
        rel_path: String::new(),
        path: std::path::PathBuf::new(),
    });
    entries.extend(closed.list_pages());
    let mut backlinks = BTreeMap::new();
    for (entry, _) in inputs.pages {
        let groups = closed.backlinks(&entry.name);
        if !groups.is_empty() {
            backlinks.insert(entry.name.clone(), groups.as_ref().clone());
        }
    }
    let block_ref_counts: BTreeMap<String, usize> = closed
        .block_ref_counts()?
        .iter()
        .map(|(id, count)| (id.clone(), *count))
        .collect();
    let owned_names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
    let icons: BTreeMap<String, String> = closed.page_icons(&owned_names).into_iter().collect();
    let snapshot = Snapshot {
        schema: SNAPSHOT_SCHEMA,
        name: inputs.name,
        exported_at: &inputs.exported_at,
        home: inputs.home_name,
        pages,
        entries,
        backlinks,
        block_ref_counts,
        aliases: closed.page_aliases(),
        icons,
        queries: inputs.queries,
    };
    serde_json::to_vec(&snapshot).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// The user-facing text of one failure HTML fragment (for an `io::Error`).
pub(crate) fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// RFC 3339 UTC, seconds precision, without a chrono dependency.
pub(crate) fn exported_at_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (Howard Hinnant), proleptic Gregorian.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_ships_only_the_shell_and_hashed_assets() {
        assert!(PublishedAppBundle::ships("/index.html"));
        assert!(PublishedAppBundle::ships("assets/main-abc.js"));
        assert!(!PublishedAppBundle::ships("/twemoji/1f600.svg"));
        assert!(!PublishedAppBundle::ships("theme-thumbnails/nord.png"));
        assert!(!PublishedAppBundle::ships("capture.html"));
        assert!(!PublishedAppBundle::ships("assets/"));
    }

    #[test]
    fn current_page_substitution_matches_the_frontend_regex() {
        assert_eq!(
            substitute_current_page("(and [[x]] <% current page %>)", "Home").as_deref(),
            Some("(and [[x]] [[Home]])")
        );
        assert_eq!(
            substitute_current_page("<%Current Page%> and <% today %>", "P").as_deref(),
            Some("[[P]] and <% today %>")
        );
        assert_eq!(substitute_current_page("(task TODO)", "P"), None);
    }

    #[test]
    fn index_rewrite_inserts_the_marker_and_title_or_refuses() {
        let html = b"<!doctype html><html><HEAD><title>Tine</title></HEAD><body></body></html>";
        let out = rewrite_index(html, "Open <tasks>").unwrap();
        assert!(out.contains("<HEAD><meta name=\"tine-published\" content=\"snapshot.json\">"));
        assert!(out.contains("<title>Open &lt;tasks&gt;</title>"));
        assert!(!out.contains("<title>Tine</title>"));
        assert!(rewrite_index(b"<html><body>no head</body></html>", "x").is_err());
    }

    #[test]
    fn home_name_steps_aside_for_a_selected_page() {
        let taken: HashSet<String> = ["open tasks", "open tasks (export)"]
            .into_iter()
            .map(|s| crate::refs::page_key(s))
            .collect();
        assert_eq!(
            home_page_name("Open Tasks", &taken),
            "Open Tasks (export 2)"
        );
        assert_eq!(home_page_name("Other", &taken), "Other");
        assert_eq!(home_page_name("  ", &HashSet::new()), "Export");
    }

    #[test]
    fn home_macro_uses_the_request_dialect() {
        let og = HomeQuery {
            source: "(task TODO)".into(),
            advanced: false,
            simple_dialect: None,
            current_page: None,
            view: None,
            host_block_id: None,
            host_properties: Vec::new(),
        };
        assert_eq!(og.block_text(), "{{query (task TODO)}}");
        let sampled = HomeQuery {
            host_properties: vec![
                ("tine.view".into(), "board".into()),
                ("tine.sample".into(), " 1 ".into()),
                ("id".into(), "0000".into()),
            ],
            ..og.clone()
        };
        assert_eq!(
            sampled.block_text(),
            "{{query (task TODO)}}\n  tine.view:: board\n  tine.sample:: 1"
        );
        assert_eq!(sampled.properties().len(), 2);
        assert_eq!(og.argument(), "(task TODO)");
        let tql = HomeQuery {
            simple_dialect: Some(QueryDialect::Tql),
            source: "task in ('TODO')".into(),
            ..og
        };
        assert_eq!(tql.block_text(), "{{tine-query task in ('TODO')}}");
        assert_eq!(tql.dialect(), QueryTextDialect::MacroTql);
        let advanced = HomeQuery {
            advanced: true,
            source: "[:find (pull ?b [*]) :where (task ?b \"TODO\")]".into(),
            host_properties: vec![("tine.sample".into(), "1".into())],
            ..tql
        };
        assert!(advanced.properties().is_empty());
        assert_eq!(
            advanced.block_text(),
            "#+BEGIN_QUERY\n  {:query [:find (pull ?b [*]) :where (task ?b \"TODO\")]}\n  #+END_QUERY"
        );
        assert_eq!(
            advanced.argument(),
            "[:find (pull ?b [*]) :where (task ?b \"TODO\")] {:table-view? true}"
        );
        assert_eq!(advanced.dialect(), QueryTextDialect::MacroQuery);
    }

    #[test]
    fn exported_at_is_rfc3339_utc() {
        let stamp = exported_at_now();
        assert_eq!(stamp.len(), 20, "{stamp}");
        assert!(stamp.ends_with('Z') && stamp.starts_with("20"), "{stamp}");
    }
}
