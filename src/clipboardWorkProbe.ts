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

let work: MutableClipboardWorkForTest = emptyClipboardWork(null);
let dirtyPageNames = new Set<string>();

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

/** Begin a labelled test receipt. Production callers never enable one. */
export function __resetClipboardWorkForTest(label: string | null = null): void {
  work = emptyClipboardWork(label);
  dirtyPageNames = new Set<string>();
}

/** Read the current labelled clipboard work receipt. */
export function __clipboardWorkForTest(): ClipboardWorkForTest {
  return {
    ...work,
    distinct_dirty_pages: [...dirtyPageNames].sort(),
    accepted_source_saves: [...work.accepted_source_saves],
    accepted_target_saves: [...work.accepted_target_saves],
    phase_order: [...work.phase_order],
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
  work[metric] += count;
}

export function recordClipboardDirtyPageForTest(name: string): void {
  dirtyPageNames.add(name);
}

export function recordClipboardUndoSnapshotForTest(nodes: number, rawBytes: number): void {
  work.undo_snapshots++;
  work.undo_snapshot_nodes += nodes;
  work.undo_snapshot_raw_bytes += rawBytes;
}

export function recordClipboardPhaseForTest(phase: ClipboardWorkPhase): void {
  work.phase_order.push(phase);
}

export function recordClipboardAcceptedSaveForTest(kind: "source" | "target", name: string): void {
  if (kind === "source") {
    work.accepted_source_saves.push(name);
    recordClipboardPhaseForTest("accepted-source-save");
  } else {
    work.accepted_target_saves.push(name);
    recordClipboardPhaseForTest("accepted-target-save");
  }
}
