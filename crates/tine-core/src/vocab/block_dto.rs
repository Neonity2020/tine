//! Converting between parsed documents and DTOs: deterministic runtime ids,
//! block and page DTO conversion both ways, block facets, content_rev, and
//! encoding a page name into a file name.

use super::*;

/// Union of two markdown page-property pre-blocks: keep `mine` and append any
/// `key:: value` line `theirs` defines that `mine` doesn't (mine wins on a clash),
/// so a sync-conflict resolve doesn't silently drop the other device's
/// `alias::`/`tags::`/`icon::`. Free text in `theirs`' pre-block is dropped (rare;
/// the conflict copy is trashed-recoverable). Mirrors the property-carry in
/// [`Graph::merge_pages`].
/// Exposed so the Tauri conflict-capsule resolver composes this one
/// pre-block union instead of re-implementing it over `BlockDto` (D-14).
pub fn union_pre(mine: Option<&str>, theirs: Option<&str>) -> Option<String> {
    let mine = mine.unwrap_or("");
    let Some(theirs) = theirs else {
        return (!mine.is_empty()).then(|| mine.to_string());
    };
    let mine_keys: std::collections::HashSet<String> = mine
        .lines()
        .filter_map(|l| doc::parse_property_line(l).map(|(k, _)| k.to_ascii_lowercase()))
        .collect();
    let extra: Vec<&str> = theirs
        .lines()
        .filter(|l| {
            doc::parse_property_line(l)
                .is_some_and(|(k, _)| !mine_keys.contains(&k.to_ascii_lowercase()))
        })
        .collect();
    if extra.is_empty() {
        return (!mine.is_empty()).then(|| mine.to_string());
    }
    let mut pre = mine.to_string();
    if !pre.is_empty() && !pre.ends_with('\n') {
        pre.push('\n');
    }
    pre.push_str(&extra.join("\n"));
    Some(pre)
}

/// True if any block in the subtree has a non-empty line that isn't a `key::`
/// property line — i.e. the page is more than an empty/placeholder bullet.
pub(crate) fn doc_has_content(blocks: &[DocBlock]) -> bool {
    blocks.iter().any(|b| {
        b.raw
            .lines()
            .any(|l| !l.trim().is_empty() && crate::doc::parse_property_line(l).is_none())
            || doc_has_content(&b.children)
    })
}

/// Versioned namespace for file-mode runtime block locators. These UUIDs are
/// store/UI keys only: persisted `id::` remains the external `((id))` identity.
const FILE_BLOCK_RUNTIME_NAMESPACE_V1: Uuid =
    Uuid::from_u128(0x1e0c_5a13_9b42_5da4_a73c_0be5_8f6a_2320);

/// Versioned namespace for the projection key of a live runtime id that is not
/// itself a UUID. A block created in the editor is saved with the frontend's
/// own id (`src/store.ts` `freshId()`: `b<base36 time>-<counter>`), and the
/// in-memory save path deliberately keeps it so the editor can go on addressing
/// the block. Such an id is a store/UI key exactly like a structural one; the
/// projection needs a 16-byte key for it, never a refusal.
const LIVE_RUNTIME_ID_KEY_NAMESPACE_V1: Uuid =
    Uuid::from_u128(0x7c2d_4b9e_31a6_4f08_9d15_6e3a_b0c4_5d71);

/// The deterministic 16-byte projection key of a live, non-UUID runtime id.
pub(crate) fn live_runtime_id_key(runtime_id: &str) -> Uuid {
    deterministic_runtime_uuid(LIVE_RUNTIME_ID_KEY_NAMESPACE_V1, runtime_id.as_bytes())
}

fn normalized_runtime_owner(owner: &str) -> io::Result<String> {
    let owner = owner.replace('\\', "/");
    let mut parts = Vec::new();
    for part in owner.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "runtime identity owner must be graph-relative",
                ))
            }
            _ => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime identity owner must not be empty",
        ));
    }
    Ok(parts.join("/"))
}

fn deterministic_runtime_uuid(namespace: Uuid, name: &[u8]) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(namespace.as_bytes());
    hasher.update((name.len() as u64).to_be_bytes());
    hasher.update(name);
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // RFC 9562 variant + version 8 (application-defined deterministic UUID).
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn runtime_owner_namespace(domain: &str, owner: &str) -> io::Result<Uuid> {
    let owner = normalized_runtime_owner(owner)?;
    let mut name = Vec::with_capacity(domain.len() + owner.len() + 16);
    name.extend_from_slice(&(domain.len() as u64).to_be_bytes());
    name.extend_from_slice(domain.as_bytes());
    name.extend_from_slice(&(owner.len() as u64).to_be_bytes());
    name.extend_from_slice(owner.as_bytes());
    Ok(deterministic_runtime_uuid(
        FILE_BLOCK_RUNTIME_NAMESPACE_V1,
        &name,
    ))
}

fn structural_runtime_child(parent: Uuid, sibling: u64) -> Uuid {
    deterministic_runtime_uuid(parent, &sibling.to_be_bytes())
}

/// Reproduce a fresh Direct parse's runtime ID from its stored structural path.
/// Public/external `id::` is a separate identity. This is the R3 result
/// constructor seam: resolve admitted output without a startup-wide ID rewrite.
pub(crate) fn doc_runtime_id_for_order(owner_rel_path: &str, order_key: &str) -> io::Result<Uuid> {
    let mut structural = runtime_owner_namespace("file-block-runtime-v1", owner_rel_path)?;
    let mut depth = 0;
    for component in order_key.split('/') {
        depth += 1;
        if depth > MAX_BLOCK_DEPTH
            || component.len() != 8
            || !component
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid projected structural order",
            ));
        }
        let sibling = u32::from_str_radix(component, 16).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid projected structural order",
            )
        })?;
        structural = structural_runtime_child(structural, u64::from(sibling));
    }
    Ok(structural)
}

fn assign_runtime_ids_checked(blocks: &mut [DocBlock], parent: Uuid) -> io::Result<()> {
    struct Frame<'a> {
        blocks: std::slice::IterMut<'a, DocBlock>,
        parent: Uuid,
        sibling: usize,
    }
    let mut frames: [Option<Frame<'_>>; MAX_BLOCK_DEPTH] = std::array::from_fn(|_| None);
    let mut len = usize::from(!blocks.is_empty());
    if len != 0 {
        frames[0] = Some(Frame {
            blocks: blocks.iter_mut(),
            parent,
            sibling: 0,
        });
    }
    while len != 0 {
        let mut frame = frames[len - 1].take().expect("active runtime-id frame");
        let Some(block) = frame.blocks.next() else {
            len -= 1;
            continue;
        };
        let sibling = frame.sibling;
        frame.sibling = frame
            .sibling
            .checked_add(1)
            .ok_or_else(allocation_overflow)?;
        let structural = structural_runtime_child(frame.parent, usize_to_u64(sibling)?);
        frames[len - 1] = Some(frame);
        if block.uuid.is_empty() {
            block.uuid = structural.to_string();
        }
        if !block.children.is_empty() {
            if len == MAX_BLOCK_DEPTH {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph page block nesting exceeds 128 levels",
                ));
            }
            frames[len] = Some(Frame {
                blocks: block.children.iter_mut(),
                parent: structural,
                sibling: 0,
            });
            len += 1;
        }
    }
    Ok(())
}

/// Seed missing runtime keys for a graph-backed document from its normalized,
/// graph-relative physical owner. Existing live keys survive ordinary saves.
pub fn assign_doc_runtime_ids(roots: &mut [DocBlock], owner_rel_path: &str) {
    let owner = runtime_owner_namespace("file-block-runtime-v1", owner_rel_path)
        .expect("validated document runtime owner");
    let _ = assign_runtime_ids_checked(roots, owner);
}

fn assign_virtual_doc_runtime_ids(
    roots: &mut [DocBlock],
    domain: &str,
    owner: &str,
) -> io::Result<()> {
    let owner = runtime_owner_namespace(domain, owner)?;
    assign_runtime_ids_checked(roots, owner)
}

fn block_runtime_id(b: &DocBlock) -> String {
    assert!(
        !b.uuid.is_empty(),
        "DocBlock must have an explicit runtime owner before DTO projection"
    );
    b.uuid.clone()
}

/// Convert a parsed (cached) block to a DTO, carrying its stable uuid as the id.
pub fn block_to_dto(b: &DocBlock) -> io::Result<BlockDto> {
    doc_blocks_to_dto_checked(std::slice::from_ref(b))?
        .pop()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "block projection produced no result",
            )
        })
}

/// Convert one block to the result-row wire shape. Result membership is about
/// block identity, raw text, and facets; descendants belong to the source page
/// and are hydrated once per page by live consumers. Keeping this constructor
/// separate makes it difficult to accidentally reintroduce overlapping subtree
/// amplification in queries, references, search, or batched resolution.
/// The ONE parser-backed `DocBlock` → shallow `BlockDto` facet projection
/// (DUP-6/B9, 2026-08-25 duplication audit): both DTO constructors delegate
/// here, so a new `BlockDto` facet is a one-site decision on this path. `id`
/// validation stays with the callers — their polite-error vs assert difference
/// is deliberate.
fn doc_block_facets_dto(block: &DocBlock, id: String) -> BlockDto {
    shallow_block_facets_dto(ShallowBlockFacets {
        id,
        raw: block.raw.clone(),
        collapsed: block.collapsed(),
        heading_level: block.heading_level(),
        marker: block.marker().map(str::to_string),
        priority: block.priority().map(str::to_string),
        scheduled: block.scheduled().map(str::to_string),
        deadline: block.deadline().map(str::to_string),
        tags: block.tags(),
        properties: block.properties(),
    })
}

/// The facets a shallow result row carries, independent of where they were
/// read from. [`doc_block_facets_dto`] fills them from a parsed `DocBlock`;
/// R3's database result read fills the same ten from `block_text`, `blocks`,
/// `tasks`, `block_planning`, `tags` and `properties`.
pub(crate) struct ShallowBlockFacets {
    pub(crate) id: String,
    pub(crate) raw: String,
    pub(crate) collapsed: bool,
    pub(crate) heading_level: Option<u8>,
    pub(crate) marker: Option<String>,
    pub(crate) priority: Option<String>,
    pub(crate) scheduled: Option<String>,
    pub(crate) deadline: Option<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) properties: Vec<(String, String)>,
}

/// The ONE shallow `BlockDto` FIELD LIST (DUP-6/B9, extended for R3).
///
/// The three fixed fields a result row never carries — `children`,
/// `breadcrumb`, `page_property` — are decided exactly here, so the
/// parser-backed and the database-backed constructors cannot drift into two
/// different answers to the same question (I-12, I-19). A new `BlockDto` facet
/// is still a one-site decision; it is now a one-site decision for BOTH
/// backends.
pub(crate) fn shallow_block_facets_dto(facets: ShallowBlockFacets) -> BlockDto {
    let ShallowBlockFacets {
        id,
        raw,
        collapsed,
        heading_level,
        marker,
        priority,
        scheduled,
        deadline,
        tags,
        properties,
    } = facets;
    BlockDto {
        id,
        raw,
        collapsed,
        children: Vec::new(),
        breadcrumb: Vec::new(),
        page_property: false,
        marker,
        priority,
        heading_level,
        scheduled,
        deadline,
        tags,
        properties,
    }
}

pub fn block_to_shallow_dto(b: &DocBlock) -> BlockDto {
    doc_block_facets_dto(b, block_runtime_id(b))
}

/// The ONE `BlockDto` → `DocBlock` field mapping (2026-08-25 duplication
/// audit, DUP-application-query-twin). Every path that rehydrates a
/// parseable block from its wire DTO goes through this constructor, so a new
/// `DocBlock` field is initialized in exactly one place. The tree walkers
/// around it deliberately differ — [`dto_blocks_to_doc_checked`] is iterative,
/// depth-bounded, and allocation-guarded because it validates untrusted wire
/// page loads, while the query/projection walkers recurse over block trees that
/// are already inside the trusted process — but the per-block field mapping
/// must not diverge. A source guard in this module's tests pins the invariant.
pub(crate) fn dto_block_to_doc_block(block: &BlockDto, is_org: bool) -> DocBlock {
    DocBlock {
        raw: block.raw.clone(),
        children: Vec::new(),
        uuid: block.id.clone(),
        is_org,
        proj: std::sync::OnceLock::new(),
    }
}

/// Bounded tree walker over [`dto_block_to_doc_block`] for untrusted wire
/// page loads: iterative (no recursion), depth-limited, allocation-guarded.
pub(crate) fn dto_blocks_to_doc_checked(
    blocks: &[BlockDto],
    is_org: bool,
) -> io::Result<Vec<DocBlock>> {
    struct Frame<'a> {
        source: &'a [BlockDto],
        next: usize,
        output: Vec<DocBlock>,
    }
    let mut frames: [Option<Frame<'_>>; MAX_BLOCK_DEPTH] = std::array::from_fn(|_| None);
    frames[0] = Some(Frame {
        source: blocks,
        next: 0,
        output: Vec::with_capacity(blocks.len()),
    });
    let mut len = 1_usize;
    loop {
        let frame = frames[len - 1]
            .as_mut()
            .expect("active DTO conversion frame");
        if frame.next == frame.source.len() {
            let completed = frames[len - 1]
                .take()
                .expect("completed DTO conversion frame")
                .output;
            len -= 1;
            if len == 0 {
                return Ok(completed);
            }
            frames[len - 1]
                .as_mut()
                .expect("parent DTO conversion frame")
                .output
                .last_mut()
                .expect("child frame has parent")
                .children = completed;
            continue;
        }
        let block = &frame.source[frame.next];
        frame.next = frame.next.checked_add(1).ok_or_else(allocation_overflow)?;
        frame.output.push(dto_block_to_doc_block(block, is_org));
        if !block.children.is_empty() {
            if len == MAX_BLOCK_DEPTH {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph page block nesting exceeds 128 levels",
                ));
            }
            frames[len] = Some(Frame {
                source: &block.children,
                next: 0,
                output: Vec::with_capacity(block.children.len()),
            });
            len += 1;
        }
    }
}

fn doc_blocks_to_dto_checked(blocks: &[DocBlock]) -> io::Result<Vec<BlockDto>> {
    fn output_with_source_capacity(source_len: usize) -> io::Result<Vec<BlockDto>> {
        let mut output = Vec::new();
        output
            .try_reserve_exact(source_len)
            .map_err(|_| allocation_overflow())?;
        Ok(output)
    }

    struct Frame<'a> {
        source: &'a [DocBlock],
        next: usize,
        output: Vec<BlockDto>,
    }
    let mut frames: [Option<Frame<'_>>; MAX_BLOCK_DEPTH] = std::array::from_fn(|_| None);
    frames[0] = Some(Frame {
        source: blocks,
        next: 0,
        output: output_with_source_capacity(blocks.len())?,
    });
    let mut len = 1_usize;
    loop {
        let frame = frames[len - 1]
            .as_mut()
            .expect("active document conversion frame");
        if frame.next == frame.source.len() {
            let completed = frames[len - 1]
                .take()
                .expect("completed document conversion frame")
                .output;
            len -= 1;
            if len == 0 {
                return Ok(completed);
            }
            frames[len - 1]
                .as_mut()
                .expect("parent document conversion frame")
                .output
                .last_mut()
                .expect("child frame has parent")
                .children = completed;
            continue;
        }
        let block = &frame.source[frame.next];
        frame.next = frame.next.checked_add(1).ok_or_else(allocation_overflow)?;
        if block.uuid.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "block has no assigned runtime identity",
            ));
        }
        frame
            .output
            .push(doc_block_facets_dto(block, block.uuid.clone()));
        if !block.children.is_empty() {
            if len == MAX_BLOCK_DEPTH {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph document nesting exceeds 128 levels",
                ));
            }
            frames[len] = Some(Frame {
                source: &block.children,
                next: 0,
                output: output_with_source_capacity(block.children.len())?,
            });
            len += 1;
        }
    }
}

pub(crate) fn page_dto_checked(entry: &PageEntry, doc: &Document) -> io::Result<PageDto> {
    Ok(PageDto {
        activation: None,
        name: entry.name.clone(),
        kind: entry.kind,
        title: entry.name.clone(),
        pre_block: doc.pre_block.clone(),
        blocks: doc_blocks_to_dto_checked(&doc.roots)?,
        rev: None,
        format: Format::from_path(&entry.path),
        read_only: false,
        path: String::new(),
        guide: false,
    })
}

/// Build a Markdown page DTO from raw Logseq Markdown without touching disk.
/// Used by the bundled in-app Guide so it reuses the same document parser and
/// DTO projection as normal graph pages.
pub fn markdown_page_dto(name: &str, title: &str, markdown: &str) -> io::Result<PageDto> {
    let mut doc = doc::parse(markdown);
    assign_virtual_doc_runtime_ids(&mut doc.roots, "bundled-markdown-v1", name)?;
    let blocks = doc_blocks_to_dto_checked(&doc.roots)?;
    Ok(PageDto {
        activation: None,
        name: name.to_string(),
        kind: PageKind::Page,
        title: title.to_string(),
        pre_block: doc.pre_block.clone(),
        blocks,
        rev: None,
        format: Format::Md,
        read_only: false,
        path: String::new(),
        guide: false,
    })
}

/// The one bounded `PageDto` -> `Document` conversion. Exposed so native
/// callers (the conflict-capsule commands) never re-grow a recursive twin.
pub fn page_dto_document(page: &PageDto) -> io::Result<Document> {
    Ok(Document {
        pre_block: page.pre_block.clone(),
        roots: dto_blocks_to_doc_checked(&page.blocks, page.format == Format::Org)?,
    })
}

pub(crate) fn existing_document_page_dto(
    base: &PageDto,
    mut document: Document,
) -> io::Result<PageDto> {
    assign_virtual_doc_runtime_ids(
        &mut document.roots,
        "graph-document-update-v1",
        if base.path.is_empty() {
            &base.name
        } else {
            &base.path
        },
    )?;
    let mut page = base.clone();
    page.pre_block = document.pre_block;
    page.blocks = doc_blocks_to_dto_checked(&document.roots)?;
    Ok(page)
}

/// Whether a page should load read-only: an org file whose on-disk bytes don't
/// round-trip through Tine's org parser/serializer, so Tine must never rewrite
/// it (lest it corrupt the user's graph). Markdown pages are always editable.
pub(crate) fn read_only_org(path: &Path, content: &str) -> bool {
    Format::from_path(path) == Format::Org && !crate::org::org_editable(content)
}

/// A page's `icon::` property value from its pre-block, handling markdown
/// (`icon:: 🏁`), org property drawers (`:icon: 🏁`) and org `#+ICON:` directives.
/// None if absent or blank.
pub(crate) fn pre_block_icon(pre: &str) -> Option<String> {
    for line in pre.lines() {
        // Markdown `icon:: value` (single shared parser; needs the `::`).
        if let Some((k, v)) = crate::doc::parse_property_line(line) {
            let v = v.trim();
            if k.eq_ignore_ascii_case("icon") && !v.is_empty() {
                return Some(v.to_string());
            }
        }
        let t = line.trim();
        // Org property drawer `:icon: value` or directive `#+ICON: value`.
        for stripped in [t.strip_prefix(':'), t.strip_prefix("#+")]
            .into_iter()
            .flatten()
        {
            if let Some(idx) = stripped.find(':') {
                let (k, v) = (&stripped[..idx], stripped[idx + 1..].trim());
                if k.eq_ignore_ascii_case("icon") && !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Stable SHA-256 content digest used as the exact loaded-byte baseline for an
/// audited existing-file save.
pub fn content_rev(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}

/// Encode one logical page title as a portable on-disk filename stem.
///
/// This is the shared create/rename identity boundary. It retains OG's
/// configured namespace spellings (`%2F` for legacy, `___` for triple-lowbar)
/// and percent syntax while making the mapping injective: a literal percent is
/// escaped before generated escapes are introduced, and every character the
/// matching decoder would otherwise reinterpret is escaped. New paths are safe
/// on Windows as well as POSIX; already-loaded pages remain path-pinned and are
/// never renamed merely because their historical spelling is non-canonical.
pub(crate) fn encode_page_name(name: &str, fmt: FileNameFormat) -> String {
    let trailing_windows_unsafe = name
        .trim_end_matches(|character| character == ' ' || character == '.')
        .len();
    let mut escaped = String::with_capacity(name.len());
    for (offset, character) in name.char_indices() {
        let encode = character == '%'
            || character <= '\u{1f}'
            || character == '\u{7f}'
            || matches!(
                character,
                '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*' | '#'
            )
            || (character == '.'
                && (fmt == FileNameFormat::Legacy
                    || offset == 0
                    || offset >= trailing_windows_unsafe))
            || (character == ' ' && offset >= trailing_windows_unsafe);
        if encode {
            let mut bytes = [0_u8; 4];
            for byte in character.encode_utf8(&mut bytes).as_bytes() {
                push_percent_byte(&mut escaped, *byte);
            }
        } else {
            escaped.push(character);
        }
    }

    let mut encoded = match fmt {
        FileNameFormat::Legacy => escaped.replace('/', "%2F"),
        FileNameFormat::TripleLowbar => escaped
            // Disambiguate underscores that would otherwise be ambiguous after
            // `/`→`___` (OG `fs.cljs`), THEN map the namespace separator.
            .replace("___", "%5F%5F%5F")
            .replace("_/", "%5F/")
            .replace("/_", "/%5F")
            .replace('/', "___"),
    };
    escape_windows_device_stem(&mut encoded);
    encoded
}

fn push_percent_byte(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    output.push('%');
    output.push(char::from(HEX[usize::from(byte >> 4)]));
    output.push(char::from(HEX[usize::from(byte & 0x0f)]));
}

/// Win32 reserves these device bodies case-insensitively even when another
/// extension follows. Superscript 1/2/3 are documented aliases for COM/LPT.
fn escape_windows_device_stem(stem: &mut String) {
    let body = stem
        .split('.')
        .next()
        .unwrap_or(stem)
        .trim_end_matches(' ')
        .to_uppercase();
    let reserved = matches!(body.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            body.strip_prefix(prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
    if reserved {
        let first_len = stem.chars().next().map(char::len_utf8).unwrap_or(0);
        let mut safe = String::with_capacity(stem.len() + 2);
        for byte in stem.as_bytes().iter().take(first_len) {
            push_percent_byte(&mut safe, *byte);
        }
        safe.push_str(&stem[first_len..]);
        *stem = safe;
    }
}

/// Inverse of [`encode_page_name`]. Legacy: dot→slash, then percent-decode
/// (`%2F`→`/`), matching Logseq's backward-compatible title parser.
/// Triple-lowbar: `___`→`/` FIRST, then percent-decode — the OG order
/// (`util.cljs:153-160`), so an encoded literal `___` (stored `%5F%5F%5F`)
/// survives instead of being turned into a separator.
pub(crate) fn decode_page_name(stem: &str, fmt: FileNameFormat) -> String {
    match fmt {
        // OG's legacy title parser predates percent-encoded namespace
        // separators: it first maps every dot to `/`, then URI-decodes. Thus a
        // retained pre-2022 `Foo.Bar.md` and a later `Foo%2FBar.md` share the
        // same effective `Foo/Bar` page identity.
        FileNameFormat::Legacy => percent_decode(&stem.replace('.', "/")),
        FileNameFormat::TripleLowbar => percent_decode(&stem.replace("___", "/")),
    }
}

/// Decode `%XX` percent-escapes (UTF-8 aware, like JS `decodeURIComponent`). An
/// invalid or truncated escape is left literal rather than dropped.
pub(crate) fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_nibble(b[i + 1]), hex_nibble(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
