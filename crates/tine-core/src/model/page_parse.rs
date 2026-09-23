//! Parsing a page: session-stable page ids, parse_page_content and title
//! binding, the effective page entry, external and exact parses, the node
//! count, panic isolation, and the parse worker count.

use super::*;

/// Runtime identities for one exact page revision, independent of its parsed
/// document and SQLite projection. Child counts describe the complete preorder
/// shape so restoration never applies only a prefix of an incompatible tree.
pub(super) struct SessionPageIds {
    pub(super) revision: String,
    pub(super) config: ContentDigest,
    preorder: Vec<(String, usize)>,
    /// Event sequence of this publication (see `StructuralGeneration`); 0 for
    /// ids restored from the stored index, which record no new read.
    pub(super) published: u64,
}

impl SessionPageIds {
    pub(super) fn from_projection(
        revision: &str,
        config: ContentDigest,
        preorder: Vec<(String, usize)>,
    ) -> Option<Self> {
        let source = revision.rsplit(':').next()?;
        (crate::direct_projection::projection_source_revision(source, config) == revision).then(
            || Self {
                revision: source.to_owned(),
                config,
                preorder,
                published: 0,
            },
        )
    }

    pub(super) fn contains(&self, ids: &HashSet<String>) -> bool {
        self.preorder.iter().any(|(id, _)| ids.contains(id))
    }

    pub(super) fn capture(revision: &str, config: ContentDigest, doc: &Document) -> Self {
        let mut pending: Vec<_> = doc.roots.iter().rev().collect();
        let mut preorder = Vec::new();
        while let Some(block) = pending.pop() {
            preorder.push((block.uuid.clone(), block.children.len()));
            pending.extend(block.children.iter().rev());
        }
        Self {
            revision: revision.to_owned(),
            config,
            preorder,
            published: 0,
        }
    }

    fn restore(&self, doc: &mut Document) -> bool {
        // Check the entire tree before changing any ID. Exact source bytes and
        // config should imply this shape; a parser discrepancy must not apply a
        // prefix of one tree's identities to another tree.
        let mut pending: Vec<_> = doc.roots.iter().rev().collect();
        let mut count = 0;
        while let Some(block) = pending.pop() {
            if self
                .preorder
                .get(count)
                .is_none_or(|(_, children)| *children != block.children.len())
            {
                return false;
            }
            count += 1;
            pending.extend(block.children.iter().rev());
        }
        if count != self.preorder.len() {
            return false;
        }
        let mut pending: Vec<_> = doc.roots.iter_mut().rev().collect();
        let mut ids = self.preorder.iter();
        while let Some(block) = pending.pop() {
            block
                .uuid
                .clone_from(&ids.next().expect("complete shape checked").0);
            pending.extend(block.children.iter_mut().rev());
        }
        true
    }
}

impl Graph {
    pub(super) fn restore_session_page_ids(
        &self,
        entry: &PageEntry,
        revision: &str,
        doc: &mut Document,
    ) -> bool {
        let config = self.config().parse_config().digest();
        self.session_page_ids
            .read()
            .unwrap()
            .get(&entry.path)
            .is_some_and(|ids| ids.revision == revision && ids.config == config && ids.restore(doc))
    }

    pub(super) fn parse_session_page_content(
        &self,
        entry: &PageEntry,
        content: &str,
    ) -> (Document, String) {
        let (mut document, revision) = parse_page_content(entry, content);
        self.restore_session_page_ids(entry, &revision, &mut document);
        (document, revision)
    }
}

pub(super) fn parse_page_content(e: &PageEntry, content: &str) -> (Document, String) {
    #[cfg(test)]
    GRAPH_TEXT_PARSE_ATTEMPTS.with(|attempts| attempts.set(attempts.get().saturating_add(1)));
    let rev = content_rev(&content);
    let mut d = parse_doc(&e.path, &content);
    #[cfg(test)]
    if content.contains(TEST_PAGE_PARSE_PANIC_SENTINEL) {
        panic!("deterministic test sentinel for a page projection panic");
    }
    assign_doc_runtime_ids(&mut d.roots, &e.rel_path);
    (d, rev)
}

pub(super) fn parsed_page_title(document: &Document, format: Format) -> Option<String> {
    let preamble = document.pre_block.as_deref()?;
    for line in preamble.lines() {
        if let Some((key, value)) = doc::parse_property_line(line) {
            if key.eq_ignore_ascii_case("title") && !value.trim().is_empty() {
                return Some(value.trim().to_owned());
            }
        }
        if format == Format::Org {
            let trimmed = line.trim();
            let directive = trimmed
                .split_once(':')
                .and_then(|(key, value)| key.eq_ignore_ascii_case("#+title").then_some(value));
            let drawer = trimmed.strip_prefix(':').and_then(|rest| {
                rest.split_once(':')
                    .and_then(|(key, value)| key.eq_ignore_ascii_case("title").then_some(value))
            });
            if let Some(value) = directive.or(drawer) {
                if !value.trim().is_empty() {
                    return Some(value.trim().to_owned());
                }
            }
        }
    }
    None
}

fn replace_page_title_property(raw: &str, name: &str) -> Option<String> {
    let mut offset = 0;
    for chunk in raw.split_inclusive('\n') {
        let line = chunk.strip_suffix('\n').unwrap_or(chunk);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim_start();
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            break;
        }
        if doc::parse_property_line(line).is_some_and(|(key, _)| key.eq_ignore_ascii_case("title"))
        {
            let newline = if chunk.ends_with("\r\n") {
                "\r\n"
            } else if chunk.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            let mut output = String::with_capacity(raw.len() + name.len());
            output.push_str(&raw[..offset]);
            output.push_str("title:: ");
            output.push_str(name);
            output.push_str(newline);
            output.push_str(&raw[offset + chunk.len()..]);
            return Some(output);
        }
        offset += chunk.len();
    }
    None
}

/// Rebind a page's own Markdown `title::` identity during rename, but only
/// when it still names the page being moved. An unrelated/custom title is
/// user content and must not be rewritten merely because the filename moves.
pub(super) fn rebind_matching_page_title_property(
    raw: &str,
    old_name: &str,
    new_name: &str,
) -> Option<String> {
    for line in raw.lines() {
        let Some((key, value)) = doc::parse_property_line(line) else {
            let trimmed = line.trim_start();
            if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
                break;
            }
            continue;
        };
        if key.eq_ignore_ascii_case("title") {
            return crate::refs::same_page(value.trim(), old_name)
                .then(|| replace_page_title_property(raw, new_name))
                .flatten();
        }
    }
    None
}

pub(super) fn bind_markdown_title_property(content: &str, name: &str) -> String {
    replace_page_title_property(content, name)
        .unwrap_or_else(|| format!("title:: {name}\n\n{content}"))
}

pub(super) fn bind_document_title_property(document: &mut Document, name: &str) {
    let current = document.pre_block.take().unwrap_or_default();
    document.pre_block = Some(
        replace_page_title_property(&current, name).unwrap_or_else(|| {
            if current.is_empty() {
                format!("title:: {name}")
            } else {
                format!("title:: {name}\n{current}")
            }
        }),
    );
}

pub(super) fn effective_page_entry(
    journal_format: &JournalFormat,
    entry: &PageEntry,
    document: &Document,
) -> PageEntry {
    let mut effective = entry.clone();
    if let Some(title) = parsed_page_title(document, Format::from_path(&entry.path)) {
        effective.name = title;
    }
    match journal_format.parse(&effective.name) {
        Some(date) => {
            effective.name = journal_format.title(date);
            effective.kind = PageKind::Journal;
            effective.date_key = Some(date.ordinal_key());
        }
        None => {
            effective.kind = PageKind::Page;
            effective.date_key = None;
        }
    }
    effective
}

pub(super) fn parse_external_document(
    graph: &Graph,
    fallback: PageEntry,
    content: &str,
) -> io::Result<ParsedExternalDocument> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        #[cfg(test)]
        GRAPH_TEXT_PARSE_ATTEMPTS.with(|attempts| attempts.set(attempts.get().saturating_add(1)));
        let format = Format::from_path(&fallback.path);
        let mut parsed = match format {
            Format::Md => doc::parse_with_source_spans(content),
            Format::Org => crate::org::parse_org_with_source_spans(content),
        };
        #[cfg(test)]
        if content.contains(TEST_PAGE_PARSE_PANIC_SENTINEL) {
            panic!("deterministic test sentinel for a page projection panic");
        }
        assign_doc_runtime_ids(&mut parsed.document.roots, &fallback.rel_path);
        graph.restore_session_page_ids(&fallback, &content_rev(content), &mut parsed.document);
        let effective = effective_page_entry(&graph.journal_format, &fallback, &parsed.document);
        ParsedExternalDocument {
            format,
            effective,
            parsed,
            revision: content_rev(content),
        }
    })) {
        Ok(parsed) => Ok(parsed),
        Err(_) => Err(page_content_rejected(
            "external document parser rejected present graph text",
        )),
    }
}

/// A page file Tine read but cannot accept as a page: the parser rejected
/// it, or it is not UTF-8. Reading the same bytes again fails the same way,
/// so the failure is a state of the file, not an interruption: it lasts
/// until the file changes.
#[derive(Debug)]
struct PageContentRejected(&'static str);

impl std::fmt::Display for PageContentRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for PageContentRejected {}

pub(super) fn page_content_rejected(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, PageContentRejected(reason))
}

/// Whether `error` is a [`page_content_rejected`] failure.
pub(super) fn is_page_content_rejection(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<PageContentRejected>())
}

pub(crate) fn parse_exact_page(
    graph: &Graph,
    entry: &PageEntry,
    content: &str,
) -> io::Result<(PageEntry, Document, String)> {
    let parsed = parse_external_document(graph, entry.clone(), content)?;
    Ok((parsed.effective, parsed.parsed.document, parsed.revision))
}

/// Count a parsed document without recursive descent, so an externally edited
/// deeply nested source cannot consume the process stack during inactive source
/// capture.  Returning `limit + 1` is sufficient for the caller to reject
/// before retaining another parser-side row.
pub(super) fn graph_text_document_node_count(document: &Document) -> io::Result<u64> {
    let mut pending = Vec::<&DocBlock>::new();
    pending
        .try_reserve(document.roots.len())
        .map_err(|_| graph_text_capture_error("source parser-node stack allocation failed"))?;
    pending.extend(document.roots.iter().rev());
    let mut count = 0_u64;
    while let Some(block) = pending.pop() {
        count = count
            .checked_add(1)
            .ok_or_else(|| graph_text_capture_error("source parser-node counter overflow"))?;
        if count > MAX_GRAPH_TEXT_PARSER_NODES {
            return Ok(count);
        }
        pending
            .try_reserve(block.children.len())
            .map_err(|_| graph_text_capture_error("source parser-node stack allocation failed"))?;
        pending.extend(block.children.iter().rev());
    }
    Ok(count)
}

pub(super) fn isolate_page_parse(
    e: PageEntry,
    journal_format: &JournalFormat,
    parse: impl FnOnce(&PageEntry) -> Option<(Document, String)>,
) -> PageParseResult {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parse(&e))) {
        Ok(Some((doc, rev))) => {
            let effective = effective_page_entry(journal_format, &e, &doc);
            Ok(Some((effective, doc, rev)))
        }
        Ok(None) => Ok(None),
        Err(payload) => {
            let _ = payload;
            if crate::backend_error::runtime_debug_diagnostics_enabled() {
                eprintln!("Tine search index skipped one page after a parse/projection panic");
            }
            Err(e.rel_path)
        }
    }
}

pub(super) fn page_cache_worker_count() -> usize {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(8);
    #[cfg(test)]
    let workers = workers.max(2);
    workers
}

#[cfg(test)]
pub(super) const TEST_PAGE_PARSE_PANIC_SENTINEL: &str = "__TINE_TEST_PAGE_PARSE_PANIC__";
