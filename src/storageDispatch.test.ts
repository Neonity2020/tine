// Route-level capability guard for the storage-authority front door (I-6).
//
// The claim this file enforces is a REACHABILITY claim, not a naming one: for
// every published admission, `src/storageDispatch.ts` invokes exactly one arm,
// and with no admission the Direct arm is unreachable. A name-grep cannot
// prove that (aliasing evades it); exercising the dispatcher with each
// admission and letting the wrong arm throw can.
//
// If you are adding a semantic storage operation, add its front door to
// `src/storageDispatch.ts` and its rows here — do not branch on
// `applicationPageAdmission` at your call site.

import { describe, it, expect, beforeEach } from "vitest";
import { graphBindingRuntime } from "./graphBindingRuntime";
import {
  CROSS_PAGE_MOVE_UNAVAILABLE_TOAST,
  dispatchBulkInsertion,
  dispatchCarry,
  dispatchCrossPageMove,
  dispatchDroppedFileInsertion,
  lastStorageDispatch,
  resetStorageDispatchCounters,
  selectStorageRoute,
  storageDispatchCounters,
  type StorageRoute,
} from "./storageDispatch";
import { setToasts, toasts } from "./ui";
import type { ApplicationPageAdmission } from "./types";

const DIRECT: ApplicationPageAdmission = { binding_generation: 11 };

function bind(admission: ApplicationPageAdmission | null): void {
  graphBindingRuntime.clear();
  if (admission) graphBindingRuntime.bind(admission.binding_generation, admission);
}

/** Every admission the native slot can publish, and the route it must produce. */
const ADMISSIONS: readonly { name: string; admission: ApplicationPageAdmission | null; route: StorageRoute }[] = [
  { name: "no admission published yet", admission: null, route: "unavailable" },
  { name: "direct", admission: DIRECT, route: "direct" },
];

function unreachable(arm: string): () => never {
  return () => {
    throw new Error(
      `I-6 violation: the ${arm} arm was reached under an admission that does not select it. `
      + "Storage authority is selected once and flows as a value; a slot with no admission must "
      + "never reach Direct persistence. Front door: src/storageDispatch.ts.",
    );
  };
}

beforeEach(() => {
  resetStorageDispatchCounters();
  setToasts([]);
});

describe("selectStorageRoute", () => {
  for (const { name, admission, route } of ADMISSIONS) {
    it(`maps ${name} to the ${route} route`, () => {
      bind(admission);
      expect(selectStorageRoute("cross-page-move").route).toBe(route);
    });
  }

  it("counts every decision so a packet gate can prove the route was exercised", () => {
    bind(DIRECT);
    selectStorageRoute("cross-page-move");
    bind(null);
    selectStorageRoute("cross-page-move");
    expect(storageDispatchCounters("cross-page-move")).toEqual({ direct: 1, unavailable: 1 });
    // Sibling operations keep their own counters.
    expect(storageDispatchCounters("dropped-file-insertion")).toEqual({ direct: 0, unavailable: 0 });
    expect(storageDispatchCounters("bulk-insertion")).toEqual({ direct: 0, unavailable: 0 });
  });
});

describe("dispatchCrossPageMove", () => {
  const request = {
    sourcePages: ["Source"],
    destinationPage: "Destination",
    roots: ["root-a", "root-b"],
  };

  it("runs only the Direct arm under a direct admission and records the intent", async () => {
    bind(DIRECT);
    const result = await dispatchCrossPageMove(request, {
      direct: () => "direct",
      unavailable: unreachable("unavailable"),
    });
    expect(result).toBe("direct");
    expect(toasts()).toEqual([]);
    expect(lastStorageDispatch("cross-page-move")).toEqual({ operation: "cross-page-move", route: "direct", request });
  });

  it("raises the shared refusal toast and runs only the unavailable arm with no admission", async () => {
    bind(null);
    const result = await dispatchCrossPageMove(request, {
      direct: unreachable("direct"),
      unavailable: () => "refused",
    });
    expect(result).toBe("refused");
    expect(toasts().map(({ message }) => message)).toEqual([CROSS_PAGE_MOVE_UNAVAILABLE_TOAST]);
    expect(lastStorageDispatch("cross-page-move")?.route).toBe("unavailable");
  });
});

describe("dispatchDroppedFileInsertion", () => {
  const request = { afterId: "block-1", paths: ["/tmp/a.png"] };

  for (const { name, admission, route } of ADMISSIONS) {
    it(`runs exactly the ${route} arm under ${name}`, async () => {
      bind(admission);
      const result = await dispatchDroppedFileInsertion(request, {
        direct: route === "direct" ? () => "direct" : unreachable("direct"),
        unavailable: route === "unavailable" ? () => "unavailable" : unreachable("unavailable"),
      });
      expect(result).toBe(route);
      // This operation owns its own refusal wording; the front door raises none.
      expect(toasts()).toEqual([]);
      expect(lastStorageDispatch("dropped-file-insertion")).toEqual({ operation: "dropped-file-insertion", route, request });
    });
  }
});

describe("dispatchBulkInsertion", () => {
  const request = { targetId: "block-1", targetPageName: "Page" };

  for (const { name, admission, route } of ADMISSIONS) {
    it(`runs exactly the ${route} arm under ${name}, synchronously`, () => {
      bind(admission);
      const result = dispatchBulkInsertion(request, {
        direct: route === "direct" ? () => "direct" : unreachable("direct"),
        unavailable: route === "unavailable" ? () => "unavailable" : unreachable("unavailable"),
      });
      expect(result).toBe(route);
      expect(storageDispatchCounters("bulk-insertion")[route]).toBe(1);
    });
  }
});

describe("dispatchCarry", () => {
  const request = { destinationPage: "Sep 1st, 2026", sourcePages: ["Aug 31st, 2026"] };

  it("runs only the Direct arm under a direct admission", async () => {
    bind(DIRECT);
    const result = await dispatchCarry(request, {
      direct: () => "direct",
      unavailable: unreachable("unavailable"),
    });
    expect(result).toBe("direct");
    expect(toasts()).toEqual([]);
  });

  it("refuses before the in-memory carry runs when no admission is published", async () => {
    bind(null);
    const result = await dispatchCarry(request, {
      direct: unreachable("direct"),
      unavailable: () => "refused",
    });
    expect(result).toBe("refused");
    expect(toasts().map(({ message }) => message)).toEqual([CROSS_PAGE_MOVE_UNAVAILABLE_TOAST]);
    expect(lastStorageDispatch("carry")).toEqual({ operation: "carry", route: "unavailable", request });
  });
});
