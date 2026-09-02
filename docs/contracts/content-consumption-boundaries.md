# Content consumption boundaries

Graph text, clipboard HTML, imported trees, plugin output, and future shared
content are untrusted at the point where Tine consumes them. Validation belongs
at that consumption point; callers must compose the existing boundary instead
of duplicating a sanitizer, URL allowlist, parser, or tree walker.

## Outbound links

`src/components/ExternalLink.tsx` is the frontend door for graph-authored
outbound anchors. It preserves `href` for presentation, always prevents WebView
navigation, and routes the destination through `backend().openExternal`. The
native `external_open_plan` remains the sole file/http(s)/mailto scheme
allowlist. Relative graph navigation uses its existing internal-link components.

On mobile, the document interceptor routes native-allowlisted schemes and
blocks every other explicit scheme. Relative and hash links remain internal.

## Recursion and size ceilings

| Consumer | Bound | Result at the bound |
| --- | ---: | --- |
| Formula evaluation | 128 AST levels (`MAX_FORMULA_EVAL_DEPTH`) | formula error |
| Visual query parsing/rendering | 64 levels (`MAX_QUERY_BUILDER_DEPTH`) | raw/truncated fallback |
| PeekPopup block copying | 64 levels (`MAX_PEEK_BLOCK_DEPTH`) | truncated count |
| Managed backlink-filter trees | 128 levels (`MAX_MANAGED_BLOCK_DEPTH`) | truncated filter entry |
| Hiccup | 64 KiB, depth 64, 2,048 nodes | bounded fallback |
| Query source | 64 KiB, nesting 64 | rejected before parsing |

Fixtures deeper than a bound are built iteratively. Tests never require a real
stack overflow, because a process abort is not a useful safety oracle.

## Clipboard HTML provenance

`structuredHtmlOutline` is the general external `text/html` boundary. It runs
Turndown with an **identity text escaper**: literal `[ ] * _ \` # ~` in pasted
text stay literal (Martin's catalogued decision, `UI-PASTE-BRACKET-LITERAL-001`;
the 2026-09-02 fix-up restored it after wave-2 packet F had re-enabled
escaping). Clipboard text equality is not provenance, and the authenticated
block-payload route exits before this function.

The safety argument does not rest on the escaper. Pasted text that happens to
form Markdown structure produces ordinary graph content, and graph content is
already untrusted at every consumer: outbound hrefs open only through
`ExternalLink` → `backend().openExternal`, whose native `external_open_plan`
allowlist rejects `javascript:` and every other non-file/http(s)/mailto scheme
(`src-tauri/src/platform.rs` tests pin this). Re-enabling paste-time escaping
would be a presentation regression, not a boundary.
