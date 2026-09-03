import { beforeEach, describe, expect, it, vi } from "vitest";

// GH #254 increment 2, adversarial implementation verification, finding 3.
//
// A force ("Keep mine") consumes its one-shot authority BEFORE every fallible
// validation, by design — a failed or replayed attempt must not be able to
// reuse it. If the backend then cannot coherently observe the winner it mints
// nothing and returns a `conflict_retry.*` code, which is deliberately NOT
// banner class.
//
// But the page already had a banner, and the force spent its token. Leaving
// that banner up strands the user completely: "Keep mine" can never work again,
// the transient retry refuses to run while a page is conflicted, and the only
// live button — "Use disk version" — discards their unsaved edits.

const calls: {
  name: string;
  force: boolean;
  conflictEpoch: number | null;
  managedConflictObservation: { path: string; revision: string } | null;
}[] = [];
let nextResult: (() => Promise<{ revision: string }>) | null = null;
let observedManagedPage: { rev: string; path: string } | null = null;
let draftPath = "pages/Notes.md";

vi.mock("./store", () => ({
  doc: { loaded: true, pages: [] },
  // The save path acquires the editor's activation before building the DTO
  // (GH #254 increment 3). These stubs say "this editor already holds one", so
  // these tests keep exercising the conflict/barrier behaviour they are about.
  editorActivationFor: () => 1,
  setEditorActivation: () => {},
  setProspectiveTarget: () => {},
  bumpEditGeneration: () => {},
  peekPageInstanceGeneration: () => 1,
  retryPendingBlockRefStamps: () => {},
  notifyPageBecameReplaceable: () => {},
  sweepReplaceable: () => {},
  pageByName: (name: string) => ({ name, kind: "page", path: draftPath }),
  pageInstanceGeneration: () => 1,
  pageToDto: (name: string) => ({
    name, kind: "page", title: name, pre_block: null, blocks: [],
    format: "markdown", path: draftPath, guide: false, read_only: false,
    activation: 1,
  }),
}));

vi.mock("./backend", () => ({
  ManagedActorRefusalError: class ManagedActorRefusalError extends Error {
    constructor(readonly reasonCode: string) {
      super("managed actor refusal");
    }
  },
  isSaveConflictError: (error: unknown) =>
    typeof error === "object" && error !== null && "kind" in error && error.kind === "save-conflict",
  backend: () => ({
    savePage: (
      page: { name: string },
      _baseRev: string | null,
      force: boolean,
      conflictEpoch: number | null,
      managedConflictObservation: { path: string; revision: string } | null,
    ) => {
      calls.push({ name: page.name, force, conflictEpoch, managedConflictObservation });
      const result = nextResult;
      nextResult = null;
      return result ? result() : Promise.resolve({ revision: "rev-after" });
    },
    getPageByPath: () => Promise.resolve(observedManagedPage),
    getPage: () => Promise.resolve(observedManagedPage),
  }),
}));

const conflicted = new Set<string>();
const toasts: string[] = [];
vi.mock("./ui", () => ({
  markConflict: (name: string) => conflicted.add(name),
  clearConflict: (name: string) => conflicted.delete(name),
  isConflicted: (name: string) => conflicted.has(name),
  conflicts: () => [...conflicted],
  bumpDataRev: () => {},
  bumpPageInventoryRev: () => {},
  pushToast: (message: string) => toasts.push(message),
}));

const {
  canForceSave,
  flushPage,
  forceSave,
  markDirty,
  resetSaveState,
  setBaseRev,
} = await import("./persistence");

// GH #254 increment 2, fourth correction-delta re-verification, HIGH. A Direct
// save error's display detail carries graph PATHS — so a page the user is
// entitled to name `conflict_authority.notes` puts that family marker inside an
// unrelated failure's text. When a substring test read it, a permanent precheck
// failure was routed into the authority handler, which deletes the epoch,
// re-dirties the page and fire-and-forgets another save: one user request became
// four backend calls and kept feeding the queue instead of reaching the bounded
// retry/toast path.
//
// W4-E4 made that unreachable by SHAPE rather than by care: `saveFailureCode`
// reads a typed `reasonCode` and never inspects prose. These cases still send
// the page name in the payload, so if a prose fallback is ever reintroduced
// they fail rather than quietly passing.
describe("a failure is classified by its code, not by the page's name", () => {
  beforeEach(() => {
    calls.length = 0;
    toasts.length = 0;
    conflicted.clear();
    nextResult = null;
    observedManagedPage = null;
    draftPath = "pages/Notes.md";
    resetSaveState();
  });

  // Only the `conflict_authority` case fails against the pre-fix substring test:
  // its handler re-enqueues, so one request multiplied. The `conflict_retry` case
  // passed either way, because that handler schedules a BOUNDED retry — it is
  // here as a specification of the same rule for a family with the same shape,
  // not as fail-before evidence.
  for (const family of ["conflict_authority", "conflict_retry"]) {
    it(`does not read "${family}." out of a page path`, async () => {
      const name = `${family}.notes`;
      markDirty(name);
      // The page name still travels in the display detail — that is the whole
      // point of the test. Typing the code moved this from "the classifier
      // must ignore the name" to "the name cannot reach the classifier", but
      // the fixture keeps carrying it so the guard still fails if some future
      // fallback starts reading prose again.
      nextResult = () => Promise.reject({
        kind: "direct-save-failure",
        reasonCode: "precheck.symlink",
        ioErrorKind: "InvalidInput",
        message: `managed text entry is a symlink or reparse point: pages/${name}.md`,
      });

      expect(await forceSave(name)).toBe(false);

      // Exactly the one request, then the bounded transient path — not a
      // self-feeding chain of re-observations.
      await new Promise((resolve) => setTimeout(resolve, 50));
      expect(calls.length).toBe(1);
    });
  }

  // Fifth re-verification, LOW. An error with no bounded prefix used to be
  // returned whole, so prose that merely OPENS with a code-shaped token was
  // accepted as that family.
  it("does not read a family out of unprefixed prose", async () => {
    markDirty("Notes");
    nextResult = () => Promise.reject(new Error(
      "conflict_authority.spent while reporting an unrelated raw failure"
    ));

    expect(await forceSave("Notes")).toBe(false);

    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(calls.length).toBe(1);
  });
});

describe("a tokenless force does not strand the page behind a spent banner", () => {
  beforeEach(() => {
    calls.length = 0;
    toasts.length = 0;
    conflicted.clear();
    nextResult = null;
    observedManagedPage = null;
    draftPath = "pages/Notes.md";
    resetSaveState();
  });

  it("retracts the spent banner and lets the retry reach the backend", async () => {
    markDirty("Notes");
    conflicted.add("Notes"); // the banner the user is looking at
    nextResult = () => Promise.reject({
      kind: "direct-save-failure",
      reasonCode: "conflict_retry.replace_pre_retirement",
    });

    expect(await forceSave("Notes")).toBe(false);

    // The banner is gone: it stood on authority this attempt already spent.
    expect(conflicted.has("Notes")).toBe(false);

    // …and the retry actually calls the backend, which the conflicted gate
    // would otherwise forbid.
    await vi.waitFor(() => expect(calls.length).toBe(2));
    expect(calls[0]).toMatchObject({ name: "Notes", force: true });
    expect(calls[1].force).toBe(false);
  });

  it("re-raises a real conflict from the retry, with a fresh banner", async () => {
    markDirty("Notes");
    conflicted.add("Notes");
    nextResult = () => Promise.reject({
      kind: "direct-save-failure",
      reasonCode: "conflict_retry.commit_recheck",
    });

    await forceSave("Notes");
    expect(conflicted.has("Notes")).toBe(false);

    nextResult = () => Promise.reject({ kind: "save-conflict", epoch: null });
    await vi.waitFor(() => expect(calls.length).toBe(2));
    await vi.waitFor(() => expect(conflicted.has("Notes")).toBe(true));
  });

  // A banner-class conflict is unchanged: it mints authority, so its banner is
  // live and must stay up.
  it("leaves a banner-class conflict exactly as it was", async () => {
    markDirty("Notes");
    nextResult = () => Promise.reject({ kind: "save-conflict", epoch: null });

    expect(await forceSave("Notes")).toBe(false);

    expect(conflicted.has("Notes")).toBe(true);
    expect(canForceSave("Notes")).toBe(false);
    expect(calls.length).toBe(1);
  });
});

describe("managed save conflict resolution", () => {
  beforeEach(() => {
    calls.length = 0;
    toasts.length = 0;
    conflicted.clear();
    nextResult = null;
    observedManagedPage = null;
    draftPath = "pages/Notes.md";
    resetSaveState();
  });

  it("retains the draft and binds Keep mine to the exact managed revision it observed", async () => {
    observedManagedPage = { rev: "managed-winner-a", path: "pages/Notes.md" };
    nextResult = () => Promise.reject("managed.conflict: stale_base");
    setBaseRev("Notes", "managed-editor-base");
    markDirty("Notes");

    expect(await flushPage("Notes")).toBe(false);
    expect(conflicted.has("Notes")).toBe(true);
    expect(canForceSave("Notes")).toBe(true);

    nextResult = () => Promise.resolve({ revision: "managed-mine" });
    expect(await forceSave("Notes")).toBe(true);
    expect(calls[1]).toMatchObject({
      force: true,
      conflictEpoch: null,
      managedConflictObservation: {
        path: "pages/Notes.md",
        revision: "managed-winner-a",
      },
    });
  });

  it("re-observes after a second managed winner and never upgrades an earlier click", async () => {
    observedManagedPage = { rev: "managed-winner-a", path: "pages/Notes.md" };
    nextResult = () => Promise.reject("managed.conflict: stale_base");
    setBaseRev("Notes", "managed-editor-base");
    markDirty("Notes");
    await flushPage("Notes");

    observedManagedPage = { rev: "managed-winner-b", path: "pages/Notes.md" };
    nextResult = () => Promise.reject("managed.conflict: stale_base");
    expect(await forceSave("Notes")).toBe(false);
    expect(calls[1]).toMatchObject({
      force: true,
      managedConflictObservation: {
        path: "pages/Notes.md",
        revision: "managed-winner-a",
      },
    });

    nextResult = () => Promise.resolve({ revision: "managed-mine" });
    expect(await forceSave("Notes")).toBe(true);
    expect(calls[2]).toMatchObject({
      force: true,
      managedConflictObservation: {
        path: "pages/Notes.md",
        revision: "managed-winner-b",
      },
    });
  });

  it("binds a losing new-page draft to the identifiable winner's exact path and revision", async () => {
    draftPath = "";
    observedManagedPage = { rev: "managed-created-winner", path: "pages/Notes.md" };
    nextResult = () => Promise.reject("managed.conflict: page_already_exists");
    markDirty("Notes");

    expect(await flushPage("Notes")).toBe(false);
    expect(conflicted.has("Notes")).toBe(true);
    expect(canForceSave("Notes")).toBe(true);

    nextResult = () => Promise.resolve({ revision: "managed-new-draft-won" });
    expect(await forceSave("Notes")).toBe(true);
    expect(calls[1]).toMatchObject({
      force: true,
      conflictEpoch: null,
      managedConflictObservation: {
        path: "pages/Notes.md",
        revision: "managed-created-winner",
      },
    });
  });

  it("fails closed when the exact managed owner was deleted or renamed", async () => {
    observedManagedPage = null;
    nextResult = () => Promise.reject("managed.conflict: missing_page");
    setBaseRev("Notes", "managed-editor-base");
    markDirty("Notes");

    await flushPage("Notes");
    expect(conflicted.has("Notes")).toBe(true);
    expect(canForceSave("Notes")).toBe(false);
    const before = calls.length;
    expect(await forceSave("Notes")).toBe(false);
    expect(calls).toHaveLength(before);
  });
});
