// F3's clipboard work receipt. The store calls this only from Vite test-mode
// branches, so it cannot alter clipboard authority, persistence scheduling, or
// production hot-path work.

export interface ClipboardWorkForTest {
  label: string | null;
  public_markdown_visits: number;
  public_markdown_raw_bytes: number;
  private_payload_visits: number;
  private_payload_raw_bytes: number;
  prepared_destination_nodes: number;
  allocated_destination_nodes: number;
  distinct_dirty_pages: string[];
  undo_snapshots: number;
  undo_snapshot_nodes: number;
  undo_snapshot_raw_bytes: number;
  accepted_source_saves: string[];
  accepted_target_saves: string[];
  source_retirement_phases: number;
  resolve_blocks_phases: number;
  final_identity_guard_phases: number;
  target_insertion_phases: number;
  phase_order: ClipboardWorkPhase[];
}

export type ClipboardWorkPhase =
  | "source-retirement"
  | "resolve-blocks"
  | "final-identity-guard"
  | "target-insertion"
  | "accepted-source-save"
  | "accepted-target-save";

type MutableClipboardWorkForTest = Omit<ClipboardWorkForTest, "distinct_dirty_pages">;

// No eager state: production call sites are compile-time-eliminated, allowing
// Rollup to tree-shake this entire module (including every metric/phase string).
let work: MutableClipboardWorkForTest | null = null;
let dirtyPageNames: Set<string> | null = null;

function emptyClipboardWork(label: string | null): MutableClipboardWorkForTest {
  return {
    label,
    public_markdown_visits: 0,
    public_markdown_raw_bytes: 0,
    private_payload_visits: 0,
    private_payload_raw_bytes: 0,
    prepared_destination_nodes: 0,
    allocated_destination_nodes: 0,
    undo_snapshots: 0,
    undo_snapshot_nodes: 0,
    undo_snapshot_raw_bytes: 0,
    accepted_source_saves: [],
    accepted_target_saves: [],
    source_retirement_phases: 0,
    resolve_blocks_phases: 0,
    final_identity_guard_phases: 0,
    target_insertion_phases: 0,
    phase_order: [],
  };
}

function currentWork(): MutableClipboardWorkForTest {
  work ??= emptyClipboardWork(null);
  return work;
}

function currentDirtyPageNames(): Set<string> {
  dirtyPageNames ??= new Set<string>();
  return dirtyPageNames;
}

/** Begin a labelled test receipt. Production callers never enable one. */
export function __resetClipboardWorkForTest(label: string | null = null): void {
  work = emptyClipboardWork(label);
  dirtyPageNames = new Set<string>();
}

/** Read the current labelled clipboard work receipt. */
export function __clipboardWorkForTest(): ClipboardWorkForTest {
  const current = currentWork();
  return {
    ...current,
    distinct_dirty_pages: [...currentDirtyPageNames()].sort(),
    accepted_source_saves: [...current.accepted_source_saves],
    accepted_target_saves: [...current.accepted_target_saves],
    phase_order: [...current.phase_order],
  };
}

export function recordClipboardWorkForTest(
  metric: "public_markdown_visits"
    | "public_markdown_raw_bytes"
    | "private_payload_visits"
    | "private_payload_raw_bytes"
    | "prepared_destination_nodes"
    | "allocated_destination_nodes"
    | "source_retirement_phases"
    | "resolve_blocks_phases"
    | "final_identity_guard_phases"
    | "target_insertion_phases",
  count = 1,
): void {
  currentWork()[metric] += count;
}

export function recordClipboardDirtyPageForTest(name: string): void {
  currentDirtyPageNames().add(name);
}

export function recordClipboardUndoSnapshotForTest(nodes: number, rawBytes: number): void {
  const current = currentWork();
  current.undo_snapshots++;
  current.undo_snapshot_nodes += nodes;
  current.undo_snapshot_raw_bytes += rawBytes;
}

export function recordClipboardPhaseForTest(phase: ClipboardWorkPhase): void {
  currentWork().phase_order.push(phase);
}

export function recordClipboardAcceptedSaveForTest(kind: "source" | "target", name: string): void {
  const current = currentWork();
  if (kind === "source") {
    current.accepted_source_saves.push(name);
    recordClipboardPhaseForTest("accepted-source-save");
  } else {
    current.accepted_target_saves.push(name);
    recordClipboardPhaseForTest("accepted-target-save");
  }
}
