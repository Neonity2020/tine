import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { FailureBoundary } from "./FailureBoundary";
import { resetSlowBackendStateForTests } from "../backend";
import { setToasts, toasts } from "../ui";

beforeEach(() => {
  setToasts([]);
  resetSlowBackendStateForTests();
  vi.spyOn(console, "error").mockImplementation(() => {});
});

afterEach(() => {
  document.body.innerHTML = "";
  vi.restoreAllMocks();
});

function Boom(props: { when: () => boolean }) {
  if (props.when()) throw new Error("list_pages failed: backend is busy");
  return <div class="survivor">content</div>;
}

describe("FailureBoundary (GH #490/#332: a throw must not blank the app silently)", () => {
  it("shows the failing region, its message, and a Retry instead of rendering nothing", () => {
    const host = document.createElement("div");
    document.body.appendChild(host);
    render(
      () => (
        <FailureBoundary region="This page">
          <Boom when={() => true} />
        </FailureBoundary>
      ),
      host,
    );
    const failure = host.querySelector<HTMLElement>(".region-failure")!;
    expect(failure).not.toBeNull();
    expect(failure.getAttribute("role")).toBe("alert");
    expect(failure.getAttribute("data-failed-region")).toBe("This page");
    expect(failure.textContent).toContain("This page could not be displayed.");
    expect(failure.textContent).toContain("list_pages failed: backend is busy");
    expect(host.querySelector(".region-failure-retry")).not.toBeNull();
  });

  it("keeps a sibling region alive when one region throws", () => {
    // The whole point: Solid drops every effect batched with the throw, so
    // before this boundary existed an unrelated panel went blank too.
    const host = document.createElement("div");
    document.body.appendChild(host);
    render(
      () => (
        <>
          <FailureBoundary region="This page">
            <Boom when={() => true} />
          </FailureBoundary>
          <FailureBoundary region="The sidebar">
            <div class="sidebar-survivor">still here</div>
          </FailureBoundary>
        </>
      ),
      host,
    );
    expect(host.querySelectorAll(".region-failure").length).toBe(1);
    expect(host.querySelector(".sidebar-survivor")?.textContent).toBe("still here");
  });

  it("Retry re-renders the region, so a transient failure recovers without a restart", () => {
    const [failing, setFailing] = createSignal(true);
    const host = document.createElement("div");
    document.body.appendChild(host);
    render(
      () => (
        <FailureBoundary region="This page">
          <Boom when={failing} />
        </FailureBoundary>
      ),
      host,
    );
    expect(host.querySelector(".region-failure")).not.toBeNull();

    setFailing(false);
    host.querySelector<HTMLButtonElement>(".region-failure-retry")!.click();

    expect(host.querySelector(".region-failure")).toBeNull();
    expect(host.querySelector(".survivor")?.textContent).toBe("content");
  });

  it("says so out loud, once, without putting the message in the always-on log", () => {
    const host = document.createElement("div");
    document.body.appendChild(host);
    render(
      () => (
        <FailureBoundary region="Linked References">
          <Boom when={() => true} />
        </FailureBoundary>
      ),
      host,
    );
    expect(toasts().map((toast) => toast.kind)).toEqual(["error"]);
    expect(toasts()[0].message).toContain("Linked References");

    const logged = vi.mocked(console.error).mock.calls[0];
    expect(logged[0]).toBe("tine.region-failed");
    expect(JSON.stringify(logged[1])).not.toContain("list_pages");
    expect(logged[1]).toMatchObject({ region: "Linked References", kind: "Error" });
  });

  // Control arm, kept deliberately: this is what Tine shipped before the
  // boundary existed, and it is why the three tests above are not decoration.
  // Runs last because the escaping throw leaves Solid's globals mid-update.
  it("control: with no boundary the throw escapes render and the healthy sibling never mounts", () => {
    const host = document.createElement("div");
    document.body.appendChild(host);
    expect(() =>
      render(
        () => (
          <>
            <Boom when={() => true} />
            <div class="sidebar-survivor">still here</div>
          </>
        ),
        host,
      ),
    ).toThrow("list_pages failed");
    expect(host.querySelector(".sidebar-survivor")).toBeNull();
    expect(host.querySelector(".region-failure")).toBeNull();
  });
});
