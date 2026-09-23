import type { ViewSettings } from "./editor/queryIr";
// TS mirrors of the Rust DTOs (crates/logseq-core/src/model.rs).

export type PageKind = "journal" | "page";

export interface BlockDto {
  id: string;
  raw: string;
  collapsed: boolean;
  children: BlockDto[];
  /** Ancestor first-lines (search/reference results only). */
  breadcrumb?: string[];
  /** Synthetic read-only backlink row sourced from page-level properties. */
  page_property?: boolean;
  // M1 block-header facets, computed once off the Rust lsdoc projection and shipped
  // so the frontend reads them off the DTO (no parse on load) instead of re-deriving
  // with its own scanner. Omitted by the backend when empty (see model.rs BlockDto).
  marker?: string;
  priority?: string;
  heading_level?: number;
  scheduled?: string;
  deadline?: string;
  tags?: string[];
  properties?: [string, string][];
}

/** Node-and-byte-bounded subtree used only for block-reference previews/exports. */
export interface BlockPreview {
  group: RefGroup;
  /** Nodes omitted after the requested preview construction budget. */
  truncated: number;
}

/** One rendered query macro requested by a Copy / Export session. */
export interface QueryExportSpec {
  key: string;
  query: string;
  advanced: boolean;
  /** Surface syntax for a simple query. Missing preserves the legacy OG
   *  interpretation; advanced queries ignore this field. */
  simple_dialect?: "og" | "tql";
  /** The page this macro is written on — the §4.4 execution context, so an
   *  exported advanced query binds `?current-page` to the SAME page the rendered
   *  one did. Optional because an export with no owning page (a multi-page
   *  selection, a reference batch) genuinely has no binding to offer; absent is
   *  the honest "unbound", never a guess. */
  current_page?: string;
}

/** Native hierarchy projection for one query macro. */
export interface QueryExportResult {
  key: string;
  groups: RefGroup[];
  shown: number;
  total: number;
  omitted_nodes: number;
}

/** Every result in this batch shared one native root/node/byte budget. */
export interface QueryExportBatch {
  results: QueryExportResult[];
  omitted_queries: number;
}

/** On-disk page format: markdown (default) or org. */
export type Format = "md" | "org";

export interface PageDto {
  name: string;
  kind: PageKind;
  title: string;
  pre_block: string | null;
  blocks: BlockDto[];
  /** Hash of the on-disk file at load time — the save baseline. `null`/absent for
   *  a page with no file yet. Sent back on save to detect external changes. */
  rev?: string | null;
  /** Format this page is stored in (drives org vs markdown inline rendering). */
  format?: Format;
  /** True for a source page Tine can't structurally round-trip: shown but not
   *  editable, so Tine never rewrites (and risks corrupting) it. */
  read_only?: boolean;
  /** Graph-root-relative path of the file this page was loaded from
   *  (`journals/2026_06_26.org`). Echoed back on save so a page pinned to a
   *  SPECIFIC file (a duplicate-day stray, #21) saves to its own file rather than
   *  being re-resolved by name to the canonical one. Empty for a brand-new page. */
  path?: string;
  /** Which live editor instance is issuing this save.
   *
   *  Stamped from the activation registry when the DTO is built, NOT carried on
   *  `FeedPage` — a token on the page object is copied by every clone and history
   *  snapshot, and the copy would then claim an identity it does not have.
   *  Absent for an editor-less writer; legal on the ordinary save path, refused on
   *  the override path. (GH #254 increment 3.) */
  activation?: number;
  /** Bundled in-app Guide page: read-only, ephemeral, and excluded from normal
   *  graph persistence/search/reference surfaces. */
  guide?: boolean;
}

/** What an activation request means for a path that already has a live editor. */
export type ActivationIntent = "reuse" | "replace";

/** The exact revision of a DTO being installed, or null only for the mounted
 * save fallback whose ordinary base-revision guard remains the write authority. */
export type ActivationExpectedRevision = string | null;

/** The outcome of activating an editor. */
export interface EditorActivationHandle {
  activation: number;
  /** The exact path this activation is live for. For an absent editor this is the
   *  prospective target resolved at activation time. */
  target: string;
  /** True when no file existed at activation time. */
  prospective: boolean;
}

/** Result of saving an editor page.
 *
 * Direct Files may return the activation that now owns a successful first
 * creation (including its resolved target). */
export type SavePageResult = {
  revision: string;
  activation?: EditorActivationHandle;
};

/** One authoritative Journals-feed transaction.  Cursor fields are ordinal
 * journal days, never counts of returned DTOs (a selected file may vanish). */
export interface JournalFeedPage {
  pages: PageDto[];
  next_before_day: number | null;
  done: boolean;
  as_of_day: number;
}

export interface GuidePage {
  title: string;
  markdown: string;
  page: PageDto;
}

export interface GuideCopyResult {
  name: string;
  created: boolean;
  created_pages?: string[];
  skipped_pages?: string[];
  copied_assets?: string[];
}

export interface TemplateDto {
  name: string;
  blocks: BlockDto[];
  /** Page the template's defining block lives on (to jump to it for editing). */
  page: string;
  kind: PageKind;
}

export interface PageEntry {
  name: string;
  kind: PageKind;
  date_key: number | null;
  /** Graph-root-relative path of this specific file. Use it to open basename
   *  collisions without re-resolving by display name. */
  path: string;
}

/** An orphaned asset file (no block references it) — for the cleanup UI. */
export interface AssetInfo {
  name: string;
  size: number;
  /** Last-modified time as Unix seconds (≈ when the file entered the graph). */
  modified: number | null;
}

/** Asset trash totals plus protected non-asset recovery entries in logseq/.tine-trash. */
export interface TrashStats {
  count: number;
  bytes: number;
  pages: number;
  journals: number;
  conflicts: number;
  other: number;
}

/** One file in a journal-day conflict (duplicate files for the same date). */
export interface JournalFile {
  name: string;
  /** Graph-root-relative path — lets the UI navigate straight to THIS file even
   *  when it shares a date with the canonical one (#21). */
  path: string;
  preview: string;
  canonical: boolean; // name is the date stem (yyyy_MM_dd) — the one to keep
}

/** A journal day that resolves to >1 file (e.g. a date-stem file + a title-named
 *  one), surfaced so the user can reconcile them. */
export interface JournalConflict {
  title: string;
  files: JournalFile[];
}

/** A journal file whose name doesn't round-trip to its date, and the name it
 *  would get. Concord invariant 4: Tine proposes these renames; opening a graph
 *  no longer performs them. */
export interface JournalFilenameMigration {
  from: string;
  to: string;
}

/** A sync-tool conflict copy (Syncthing/Dropbox) shadowing a real page — a
 *  `*.sync-conflict-*.md` (or Dropbox `(conflicted copy)`) file. Excluded from
 *  the page list; surfaced here so the user can review + merge it. */
export interface SyncConflict {
  /** Graph-root-relative path of the conflict copy. */
  path: string;
  /** Display name of the page it shadows (decoded page name / journal title). */
  base_name: string;
  /** Graph-root-relative path of the winning file, if it still exists. */
  base_path: string | null;
  kind: PageKind;
  /** Device/timestamp suffix from the conflict filename (best-effort label). */
  tag: string;
  /** One-line content preview of the conflict copy. */
  preview: string;
}

/** What a rename deliberately left undone. A rename cascades reference
 *  rewrites through every referring page, but files under VCS-marker
 *  quarantine are skipped rather than rewritten, so they still point at the
 *  old name and the user has to be told. */
export interface RenameOutcome {
  /** Paths of quarantined referrers left byte-identical, old refs intact. */
  skippedConflictedReferrers: string[];
  /** Every page whose file the rename moved or rewrote; the frontend reloads
   *  exactly these and keeps every other open page (GH #535). */
  touched: RenameTouchedPage[];
}

/** One page a rename moved or rewrote. */
export interface RenameTouchedPage {
  /** The page's name before the rename. */
  name: string;
  kind: PageKind;
  /** Graph-root-relative path of the file before the rename. */
  path: string;
  /** Set when the page itself moved: its name after the rename. */
  renamedTo: string | null;
}

/** A page whose on-disk bytes carry unresolved VCS merge-conflict markers
 *  (git/Fossil). It stays readable, but saves to it are refused so Tine never
 *  rewrites (and thereby mangles) the markers — the user resolves the merge in
 *  their VCS or an external editor. */
export interface VcsMarkerConflict {
  /** Graph-root-relative path of the marker-bearing file. */
  path: string;
  /** Display name of the page (decoded page name / journal title). */
  name: string;
  kind: PageKind;
  /** Distinct marker kinds found, e.g. ["<<<<<<<", "=======", ">>>>>>>"]. */
  markers: string[];
}

/** Native, binding-scoped admission envelope stamped into plans and fences
 * so an async continuation can prove it still targets the binding it was
 * planned against (I-20). Direct Files is the only authority. */
export type ApplicationPageAdmission = { binding_generation: number };

export type StorageTransitionKind = "lookup" | "open_direct";

export type StorageTransitionPhase =
  | "requested"
  | "looking_up_selection"
  | "validating_target"
  | "opening_direct";

/** Sole native storage-transition receipt. The frontend renders this identity;
 * it never infers ownership or failure from text prefixes or elapsed time. */
export interface StorageTransitionEvent {
  operationId: number;
  window: string;
  canonicalRoot?: string;
  kind: StorageTransitionKind;
  phase: StorageTransitionPhase;
  elapsedMs: number;
  terminal: boolean;
  outcome?: "succeeded" | "failed" | "superseded";
  outcomeCode?: string;
}

/** How one aligned block differs between the winner and the conflict copy. */
export type RowKind = "unchanged" | "modified" | "added" | "removed";

/** One side of a diff row. */
export interface BlockView {
  /** Persisted `id::`, or empty. */
  uuid: string;
  /** The block's full dedented body (may be multi-line); UI shows the first line. */
  text: string;
  child_count: number;
}

/** How a row relates to the 3-way BASE (the Concord ledger's last-agreed text).
 *  Only present on 3-way diffs. */
export type Diff3Verdict = "mine-only" | "theirs-only" | "both-changed";

/** Where a proposed merged body came from: "computed" = composed here from two
 *  disjoint edits of the base; "artifact" = lifted from the merge tool's own
 *  suggested-resolution region. Computed always wins when both exist. */
export type MergedSource = "computed" | "artifact";

/** A merged body offered for a `both-changed` row. Display only — the resolve
 *  re-derives the text from the same inputs and never trusts this echo. */
export interface MergedProposal {
  text: string;
  source: MergedSource;
}

/** One aligned position in the two block trees. `id` is a stable path ("2.1")
 *  that the resolve step reproduces, so a decision maps back to the same block. */
export interface DiffRow {
  id: string;
  kind: RowKind;
  mine: BlockView | null;
  theirs: BlockView | null;
  children: DiffRow[];
  /** 3-way classification against the base (absent on 2-way diffs). */
  verdict?: Diff3Verdict | null;
  /** Pre-selected decision the base justifies ("mine"/"theirs"/"merged"); the
   *  modal only pre-selects it — nothing applies without the user's confirm. */
  suggestion?: MergeDecision | null;
  /** Merged body proposed for a `both-changed` row — two disjoint edits
   *  composed, or the merge tool's own suggestion. Absent on 2-way diffs and
   *  wherever no proposal may be offered. */
  merged?: MergedProposal | null;
}

/** The full block-level diff of a conflict copy against its winner. */
export interface SyncConflictDiff {
  base_rev: string;
  conflict_rev: string;
  rows: DiffRow[];
  mine_pre: string | null;
  theirs_pre: string | null;
  pre_differs: boolean;
  blocks_identical: boolean;
  /** True when rows carry 3-way verdicts computed against a real base. */
  three_way?: boolean;
  /** Revision of the pinned merge base this 3-way alignment (and its "merged"
   *  proposals) were computed from; echoed back on resolve so a repinned base
   *  refuses instead of substituting a body the user never saw. */
  merge_base_rev?: string | null;
}

/** A user's per-row merge decision. */
export type MergeDecision = "mine" | "theirs" | "both" | "merged";

/** Where a conflict object came from (Concord L3). */
export type ConflictSource = "sync-copy" | "vcs-markers" | "live-save" | "duplicate-journal";

export interface LiveSaveConflictSnapshot {
  page: PageDto;
  base_rev: string | null;
  conflict_epoch: number;
  draft_version: number;
  /** Exact editor base captured before watcher admission could advance caches. */
  base_text?: string | null;
  /** Exact disk revision the current review is aligned against. */
  disk_rev?: string;
  /** This retained draft was rehydrated after process restart. The ordinary
   * disk-loaded editor is not its replacement authority. */
  restored?: boolean;
}

export interface LiveSaveConflictCapture {
  diff: SyncConflictDiff;
  base_text: string | null;
  disk_rev: string;
}

/** Which version of the page a side is. Three roles, not two — a diff3/Fossil
 *  marker block and a ledger-backed conflict copy both supply a base. */
export type SideRole = "mine" | "theirs" | "base";

/** One version of a page participating in a conflict. */
export interface ConflictSide {
  role: SideRole;
  label: string;
  /** Graph-root-relative path, when the side is a file of its own. */
  path?: string | null;
}

/** One item in the Concord conflict queue: a page that needs the user's
 *  judgement. Entirely DERIVED from what is on disk (no metadata is stored in
 *  the graph), so the queue survives restarts by being recomputed. */
export interface ConflictObject {
  /** Stable derived id — `copy:<path>` / `markers:<path>` / `journal:<path>`. */
  id: string;
  source: ConflictSource;
  page_name: string;
  /** Path of the page to navigate to (the winner, or the marker file). */
  page_path: string;
  kind: PageKind;
  sides: ConflictSide[];
  /** Rows needing a decision, when it was cheap to compute (absent ≠ zero). */
  block_conflicts?: number | null;
  /** Marker tokens present, for a `vcs-markers` object. */
  markers?: string[];
  /** Present only for an in-memory editor draft whose guarded Direct Files save
   * was refused. It carries the exact unconsumed authority presentation. */
  live?: LiveSaveConflictSnapshot;
}

/** Everything the conflicts UI shows, from one pass over the graph
 *  (`conflict_inventory`); mirrors core's `ConflictInventory`. */
export interface ConflictInventory {
  sync_conflicts: SyncConflict[];
  vcs_markers: VcsMarkerConflict[];
  queue: ConflictObject[];
}

/** A marker-bearing page's own conflict, parsed out of its `<<<<<<<` sections
 *  and diffed with the same block machinery as a conflict copy (Concord L5). */
export interface MarkerConflictDiff {
  mine_label: string;
  theirs_label: string;
  regions: number;
  diff: SyncConflictDiff;
}

export interface RefGroup {
  page: string;
  kind: PageKind;
  /** Exact owner for path-bearing search presentations; absent for legacy DSL results. */
  path?: string;
  blocks: BlockDto[];
  evidence?: ReferenceBlockEvidence[];
}

export interface BacklinkFilterTarget {
  page: string;
  kind: PageKind;
  block_id: string;
}

export interface BacklinkFilterEntry extends BacklinkFilterTarget {
  facets: string[];
  text_matches: boolean;
  truncated?: boolean;
}

export interface BacklinkFilterContext {
  entries: BacklinkFilterEntry[];
  search_error?: string;
  truncated?: boolean;
}

export type ReferenceKind = "explicit" | "plain";

export interface ReferenceOccurrence {
  matched_name: string;
  canonical: string;
  kind: ReferenceKind;
  /** UTF-16 offsets into the matching BlockDto.raw. */
  span: MatchSpan;
  rule: string;
}

export interface ReferenceBlockEvidence {
  block_id: string;
  occurrences: ReferenceOccurrence[];
  /** Total matches in the block before the bounded jump-target list is capped. */
  total?: number;
  truncated?: boolean;
}

export interface MatchSpan {
  /** UTF-16 code-unit offsets into QueryHit.display_text; end is exclusive. */
  start: number;
  end: number;
}

export interface MatchEvidence {
  clause_id: number;
  field: "page_name" | "visible_content";
  mode: "contains" | "phrase" | "regex" | "fuzzy";
  spans: MatchSpan[];
  score?: number;
}

export type ObjectiveMatchClass = "exact" | "prefix" | "substring" | "fuzzy" | "body_evidence";

export interface QueryDiagnostic {
  code: string;
  message: string;
  span?: MatchSpan;
}

export interface QueryExplainNode {
  clause_id?: number;
  description: string;
  children: QueryExplainNode[];
}

export type QueryHit =
  | {
      entity: "page";
      page: PageEntry;
      display_text: string;
      evidence: MatchEvidence[];
      score: number;
      match_class?: ObjectiveMatchClass;
      matched_alias?: string;
      /** The hydrated page row, present exactly when this hit came from the
       * Display-enabled search AND names a stored page (§7.6, Q3). A virtual
       * reference-name suggestion has no stored page and therefore no row: its
       * absence is the honest answer, never an empty property list. */
      row?: import("./editor/queryIr").PageRow;
    }
  | {
      entity: "block";
      page: string;
      kind: PageKind;
      /** Exact graph-root-relative file that physically owns this block hit. */
      path?: string;
      block: BlockDto;
      display_text: string;
      evidence: MatchEvidence[];
      score?: number;
      match_class?: ObjectiveMatchClass;
    };

export interface QueryExecution {
  hits: QueryHit[];
  diagnostics: QueryDiagnostic[];
  explanation: { branches: QueryExplainNode[] };
  /** Absent only when talking to an older backend or using an older test fixture. */
  has_more?: { pages: boolean; blocks: boolean };
  cancelled: boolean;
}

/** A single routed page used to scope block search. When present, `path` is the
 * authoritative file identity; otherwise kind plus canonical page name is used. */
export interface QueryPageScope {
  name: string;
  pageKind: PageKind;
  path?: string;
}

/** Result of an advanced (datalog) query: matched groups + which clause heads
 *  ran vs were ignored (`supported` is false only when nothing in the subset matched). */
export interface AdvancedQueryResult {
  groups: RefGroup[];
  ran: string[];
  ignored: string[];
  supported: boolean;
}

export interface GraphMeta {
  root: string;
  journals_dir: string;
  pages_dir: string;
  preferred_workflow: string; // "now" | "todo"
  shortcuts: Record<string, string>;
  start_of_week: number; // Logseq :start-of-week, 0=Monday … 6=Sunday (default 6)
  block_hidden_properties: string[];
  /** Logseq `:ref/linked-references-collapsed-threshold` — a page opens its
   *  Linked References collapsed once the total backlink count reaches this.
   *  Absent or non-integer in config.edn means OG's default, 100. */
  linked_references_collapsed_threshold: number;
  default_journal_template: string | null;
  /** Graph-portable startup page from config.edn `:default-home {:page "..."}`. */
  default_home?: string | null;
  favorites: string[];
  /** `:tine/favorites-page` — the page that owns the Favorites arrangement
   *  (groups and order). `favorites` above stays the flat, Logseq-readable
   *  membership list. Absent until this graph has one. */
  favorites_page?: string | null;
  journal_page_title_format: string; // :journal/page-title-format (default "MMM do, yyyy")
  journal_file_name_format: string; // :journal/file-name-format (default "yyyy_MM_dd")
  preferred_format: Format; // :preferred-format — new pages/journals ("md" | "org")
  macros: Record<string, string>; // :macros — user text-substitution macros ($1..$N)
  enable_timetracking: boolean; // :feature/enable-timetracking?, default true
  show_brackets: boolean; // :ui/show-brackets?, default true
  /** :shortcut/doc-mode-enter-for-new-block?, false when absent / older backend. */
  doc_mode_enter_for_new_block?: boolean;
  /** :editor/logical-outdenting?, false when absent / older backend. */
  logical_outdenting?: boolean;
  logbook_with_second_support: boolean; // :logbook/settings :with-second-support?, default true
  logbook_enabled_in_timestamped_blocks: boolean;
  logbook_enabled_in_all_blocks: boolean;
  guide_announced: boolean; // :tine/guide-announced?, default false
}

export interface Rect {
  top: number;
  left: number;
  width: number;
  height: number;
  /** Coordinate-space dimensions from a current Logseq PDF sidecar. Absent on
   * rectangles written by older Tine versions, which already use page space. */
  source_width?: number;
  source_height?: number;
}

export interface Highlight {
  id: string;
  page: number;
  position: { page: number; bounding: Rect; rects: Rect[] };
  color: string;
  text: string | null;
  image: number | null;
}

export interface PdfState {
  highlights: Highlight[];
  page: number | null;
  scale: number | null;
}

/** "Export query results…": what the query surface executed plus the user's
 *  export choices. `query` is the EFFECTIVE source the surface ran. Mirrors
 *  Rust `publish::query_export::QueryPublicationRequest` (camelCase). */
export interface QueryPublicationRequest {
  query: string;
  advanced: boolean;
  simpleDialect?: "og" | "tql";
  currentPage?: string | null;
  view?: ViewSettings | null;
  hostBlockId?: string | null;
  /** The host block's `tine.*` properties, as handed to `parseQuery`; the
   *  export's home block carries them so it opens under the same display. */
  hostProperties?: [string, string][];
  name: string;
  /** The exact reviewed leaf folder; carried back unchanged on confirm. */
  folder?: string | null;
  replace?: boolean;
  /** Byte budget for copied assets (Settings → Graph); `null` = the backend default. */
  assetBudgetBytes?: number | null;
}

export interface QueryPublicationPage {
  path: string;
  name: string;
  journal: boolean;
}

/** The reviewed plan: shown by the dialog, its fingerprint echoed on confirm. */
export interface QueryPublicationPlan {
  anchor: "block" | "page";
  rowCount: number;
  sampled: boolean;
  boundPage: string | null;
  pages: QueryPublicationPage[];
  folder: string;
  path: string;
  exists: boolean;
  suggestedFolder: string | null;
  fingerprint: string;
}

/** What a static export produced (`publish_query`). */
export interface PublishOutcome {
  path: string;
  pages: number;
  /** Where the previous occupant of the folder was retired, when there was one. */
  retired: string | null;
  /** Non-fatal omissions (an asset that was missing or over the size limit). */
  warnings: string[];
}

/** Options for the print-to-PDF export (chosen in the pre-export dialog). Field
 *  names are snake_case to match the Rust `PrintOpts` serde deserialization. */
export interface PrintOpts {
  /** Expand `collapsed:: true` blocks (true = print the whole page). */
  expand_collapsed: boolean;
  /** Base body font size, px. */
  font_px: number;
  /** Page margin, mm (all four sides). */
  margin_mm: number;
}
