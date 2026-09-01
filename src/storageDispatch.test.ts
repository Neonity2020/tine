// Route-level capability guard for the storage-authority front door (I-6).
//
// The claim this file enforces is a REACHABILITY claim, not a naming one: for
// every published admission, `src/storageDispatch.ts` invokes exactly one arm,
// and under a managed binding the Direct arm is unreachable. A name-grep cannot
// prove that (aliasing evades it); exercising the dispatcher with each
// admission and letting the wrong arm throw can.
//
// If you are adding a semantic storage operation, add its front door to
// `src/storageDispatch.ts` and its rows here — do not branch on
// `applicationPageAdmission` at your call site.

import { describe, it, expect, beforeEach } from "vitest";
import { managedStorageRuntime } from "./managedStorageRuntime";
import {
  CROSS_PAGE_MOVE_UNAVAILABLE_TOAST,
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

const MANAGED_WRITABLE: ApplicationPageAdmission = {
  binding_generation: 11,
  authority: "managed_writable",
  application_save_page_blocks: 511,
  application_page_request_text_bytes: 1_048_576,
  application_page_max_depth: 128,
};
const MANAGED_UNAVAILABLE: ApplicationPageAdmission = {
  binding_generation: 11,
  authority: "managed_unavailable",
};
const DIRECT: ApplicationPageAdmission = { binding_generation: 11, authority: "direct" };

function bind(admission: ApplicationPageAdmission | null): void {
  managedStorageRuntime.clear();
  if (admission) managedStorageRuntime.bind(admission.binding_generation, admission);
}

/** Every admission the native slot can publish, and the route it must produce. */
const ADMISSIONS: readonly { name: string; admission: ApplicationPageAdmission | null; route: StorageRoute }[] = [
  { name: "no admission published yet", admission: null, route: "unavailable" },
  { name: "managed_unavailable", admission: MANAGED_UNAVAILABLE, route: "unavailable" },
  { name: "managed_writable", admission: MANAGED_WRITABLE, route: "managed" },
  { name: "direct", admission: DIRECT, route: "direct" },
];

function unreachable(arm: string): () => never {
  return () => {
    throw new Error(
      `I-6 violation: the ${arm} arm was reached under an admission that does not select it. `
      + "Storage authority is selected once and flows as a value; a managed-bound slot must "
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
    bind(MANAGED_WRITABLE);
    selectStorageRoute("cross-page-move");
    bind(DIRECT);
    selectStorageRoute("cross-page-move");
    bind(null);
    selectStorageRoute("cross-page-move");
    expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 1, direct: 1, unavailable: 1 });
    // Sibling operations keep their own counters.
    expect(storageDispatchCounters("dropped-file-insertion")).toEqual({ managed: 0, direct: 0, unavailable: 0 });
  });
});

describe("dispatchCrossPageMove", () => {
  const request = {
    sourcePages: ["Source"],
    destinationPage: "Destination",
    roots: ["root-a", "root-b"],
  };

  it("reaches the managed arm only, under a managed_writable admission", async () => {
    bind(MANAGED_WRITABLE);
    const seen: ApplicationPageAdmission[] = [];
    const result = await dispatchCrossPageMove(request, {
      managed: (admission) => {
        seen.push(admission);
        return "managed" as const;
      },
      direct: unreachable("Direct"),
      unavailable: unreachable("unavailable"),
    });
    expect(result).toBe("managed");
    expect(seen).toEqual([MANAGED_WRITABLE]);
    expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 1, direct: 0, unavailable: 0 });
    expect(toasts()).toEqual([]);
  });

  it("reaches the direct arm only, under a direct admission", async () => {
    bind(DIRECT);
    const result = await dispatchCrossPageMove(request, {
      managed: unreachable("managed"),
      direct: () => "direct" as const,
      unavailable: unreachable("unavailable"),
    });
    expect(result).toBe("direct");
    expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 0, direct: 1, unavailable: 0 });
    expect(toasts()).toEqual([]);
  });

  for (const { name, admission } of ADMISSIONS.filter((row) => row.route === "unavailable")) {
    it(`refuses with the one shared toast, and reaches neither writer arm, under ${name}`, async () => {
      bind(admission);
      const result = await dispatchCrossPageMove(request, {
        managed: unreachable("managed"),
        direct: unreachable("Direct"),
        unavailable: () => "refused" as const,
      });
      expect(result).toBe("refused");
      expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 0, direct: 0, unavailable: 1 });
      expect(toasts().map((toast) => toast.message)).toEqual([CROSS_PAGE_MOVE_UNAVAILABLE_TOAST]);
    });
  }

  it("records the stated intent alongside the route", async () => {
    bind(MANAGED_WRITABLE);
    await dispatchCrossPageMove(request, {
      managed: () => "managed" as const,
      direct: unreachable("Direct"),
      unavailable: unreachable("unavailable"),
    });
    expect(lastStorageDispatch("cross-page-move")).toEqual({
      operation: "cross-page-move",
      route: "managed",
      request,
    });
  });

  it("decides the route once, before any arm runs, so an admission that changes mid-arm cannot re-route", async () => {
    bind(DIRECT);
    let armAdmissionAtEntry: string | null = null;
    await dispatchCrossPageMove(request, {
      managed: unreachable("managed"),
      direct: async () => {
        armAdmissionAtEntry = managedStorageRuntime.snapshot().applicationPageAdmission?.authority ?? null;
        // A rebind while the Direct arm is mid-flight must not hand the rest of
        // the operation to the managed arm: the branch is already taken. The
        // arm's OWN staleness re-check (I-20) is what refuses the landing —
        // see src/storageDispatchRoutes.test.ts.
        bind(MANAGED_WRITABLE);
        return "direct" as const;
      },
      unavailable: unreachable("unavailable"),
    });
    expect(armAdmissionAtEntry).toBe("direct");
    expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 0, direct: 1, unavailable: 0 });
  });
});

describe("dispatchDroppedFileInsertion", () => {
  const request = { afterId: "block-1", paths: ["/tmp/a.png"] };

  for (const { name, admission, route } of ADMISSIONS) {
    it(`reaches only the ${route} arm under ${name}`, async () => {
      bind(admission);
      const arms = {
        managed: route === "managed" ? () => "managed" as const : unreachable("managed"),
        direct: route === "direct" ? () => "direct" as const : unreachable("Direct"),
        unavailable: route === "unavailable" ? () => "unavailable" as const : unreachable("unavailable"),
      };
      expect(await dispatchDroppedFileInsertion(request, arms)).toBe(route);
      expect(storageDispatchCounters("dropped-file-insertion")[route]).toBe(1);
    });
  }

  it("raises no toast of its own — the arm owns its refusal message", async () => {
    bind(MANAGED_UNAVAILABLE);
    await dispatchDroppedFileInsertion(request, {
      managed: unreachable("managed"),
      direct: unreachable("Direct"),
      unavailable: () => null,
    });
    expect(toasts()).toEqual([]);
  });
});

describe("dispatchCarry — B2 gave carry a route", () => {
  const request = { destinationPage: "Sep 1st, 2026", sourcePages: ["Aug 31st, 2026"] };

  // B1 asserted the opposite of this on purpose: carry ran the Direct
  // choreography under EVERY admission, and that assertion existed so B2 had to
  // delete it deliberately rather than rediscover the gap. It is deleted here.
  it("runs the Direct choreography only under a Direct binding", async () => {
    bind(DIRECT);
    expect(
      await dispatchCarry(request, {
        direct: () => "direct" as const,
        managed: unreachable("managed"),
        unavailable: unreachable("unavailable"),
      }),
    ).toBe("direct");
    expect(lastStorageDispatch("carry")).toEqual({ operation: "carry", route: "direct", request });
  });

  it("refuses under a managed binding instead of writing Direct", async () => {
    bind(MANAGED_WRITABLE);
    expect(
      await dispatchCarry(request, {
        direct: unreachable("Direct"),
        managed: () => "managed" as const,
        unavailable: unreachable("unavailable"),
      }),
    ).toBe("managed");
    expect(lastStorageDispatch("carry")).toEqual({ operation: "carry", route: "managed", request });
  });

  it("refuses with the shared toast when no writer is bound", async () => {
    bind(null);
    expect(
      await dispatchCarry(request, {
        direct: unreachable("Direct"),
        managed: unreachable("managed"),
        unavailable: () => "unavailable" as const,
      }),
    ).toBe("unavailable");
    expect(toasts().map((toast) => [toast.message, toast.kind])).toEqual([
      [CROSS_PAGE_MOVE_UNAVAILABLE_TOAST, "error"],
    ]);
  });
});
