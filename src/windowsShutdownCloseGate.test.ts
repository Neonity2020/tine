import { readFileSync } from "node:fs";
import { describe, expect, it, vi } from "vitest";
import { createSafeCloseCoordinator, type SafeCloseDeps } from "./safeClose";

// GH #455 research fixture (2026-09-16, base fc37445f). Read-only research:
// no production code is changed by this file. It pins, in executable form, the
// two state machines a Windows shutdown traverses, so a Windows runner (or an
// upstream fix) can be checked against the same expectations.
//
// Layer 2 — the session-end exit machine — is where the report places the
// defect. Windows shutdown sends WM_QUERYENDSESSION/WM_ENDSESSION, never a
// WM_CLOSE-shaped close request, so the frontend close gate below is not the
// direct cause of the blocking screen; it is the surface a fix that maps
// session-end onto a close would traverse, and it bounds how fast any close-
// shaped stimulus can be answered.

/// Windows' documented default patience before naming an app on the
/// "app is preventing shutdown" screen, and before force-terminating it.
const WINDOWS_HUNG_APP_TIMEOUT_MS = 5_000;
const WINDOWS_WAIT_TO_KILL_APP_TIMEOUT_MS = 20_000;

interface SessionEndLoop {
  wmQueryEndSession(): "agree" | "refuse";
  wmEndSession(): void;
  requestExit(code: number): void;
  pump(elapsedMs: number): void;
  ms(): number;
  exitCallbackRan(): boolean;
  processExitCode(): number | undefined;
  hungScreenShownAt(): number | null;
  forceKilledAt(): number | null;
}

/** A platform-neutral model of the tao 0.35.3 (rev 07f3742) + tauri-runtime-wry
 *  2.11.2 + tauri 2.11.2 exit machine, as built by Tine at base fc37445f.
 *
 *  Every transition cites its source; see the GH #455 research report for the
 *  full trace. The one behavior that makes this fixture fail-against-a-fix:
 *  the WM_ENDSESSION path delivers Tine's exit callback but never breaks the
 *  message loop, so nothing but the OS can end the process. */
function createSessionEndLoop(tineExitCallback: () => number | undefined): SessionEndLoop {
  let clockMs = 0;
  let controlFlow: "wait" | "exit" = "wait";
  let exitCallbackRan = false;
  let processExit: number | undefined;
  let hungScreenShownAt: number | null = null;
  let forceKilledAt: number | null = null;

  const running = () => processExit === undefined;

  return {
    // tao event_loop.rs: the WM_QUERYENDSESSION arm is deliberately absent
    // ("We don't process WM_QUERYENDSESSION yet"), so the message falls to
    // DefWindowProc, which answers TRUE: the app agrees to the shutdown.
    wmQueryEndSession: () => "agree",
    // tao event_loop.rs WM_ENDSESSION arm: delivers LoopDestroyed synchronously
    // and returns 0 ("after we return 0 here, Windows will shut us down").
    // tauri-runtime-wry lib.rs:4192 maps it to RunEvent::Exit, and the wry
    // event hook resets control flow to Wait (lib.rs:4175) — nothing posts
    // WM_QUIT, sets ControlFlow::Exit, or calls std::process::exit on this
    // path. Tine's own RunEvent::Exit callback (src-tauri/src/lib.rs
    // app.run) drains Concord ledgers within EXIT_DRAIN_BUDGET (200 ms) and
    // marks the clean shutdown, then returns.
    wmEndSession() {
      if (controlFlow !== "exit") controlFlow = "wait";
      processExit = tineExitCallback();
      exitCallbackRan = true;
    },
    // tauri-runtime-wry lib.rs:4361 Message::RequestExit — AppHandle::exit.
    // ExitRequested is not prevented, so control flow becomes Exit and the
    // tao run loop breaks at its next iteration boundary; tao's run() then
    // exits the process. This is Tine's normal quit and final-window close
    // (close_graph_window / tine_quit → app.exit(0)).
    requestExit(code: number) {
      if (processExit !== undefined) return;
      controlFlow = "exit";
      processExit = code;
    },
    // tao run_return: the 'main loop only breaks on WM_QUIT or
    // ControlFlow::ExitWithCode at a message-iteration boundary. The model
    // advances one boundary per pump; a loop still "running" after the OS
    // timeouts below is one only TerminateProcess can end.
    pump(elapsedMs: number) {
      clockMs += elapsedMs;
      if (running() && hungScreenShownAt === null && clockMs > WINDOWS_HUNG_APP_TIMEOUT_MS) {
        hungScreenShownAt = clockMs;
      }
      if (running() && forceKilledAt === null && clockMs > WINDOWS_WAIT_TO_KILL_APP_TIMEOUT_MS) {
        forceKilledAt = clockMs;
      }
    },
    ms: () => clockMs,
    exitCallbackRan: () => exitCallbackRan,
    processExitCode: () => processExit,
    hungScreenShownAt: () => hungScreenShownAt,
    forceKilledAt: () => forceKilledAt,
  };
}

interface CloseGate {
  nativeCloseStimulus(): Promise<void>;
  windowClosed(): boolean;
  processExitCode(): number | undefined;
  events(): string[];
}

/** The App.tsx close handler (src/App.tsx onCloseRequested) around the REAL
 *  production safeClose coordinator, plus the tauri-2.11.2 manager hook that
 *  prevents every native close while a JS close-requested listener exists —
 *  Tine registers one unconditionally at startup. The App.tsx handler itself
 *  is inline and untestable-in-isolation, so its exact control flow
 *  (allowClose second pass → preventDefault → closeInProgress re-entry guard
 *  → prepare transaction → closeGraphWindow) is mirrored here line for line.
 */
function createTineCloseGate(
  safeClose: ReturnType<typeof createSafeCloseCoordinator>,
): CloseGate {
  let closeInProgress = false;
  let allowClose = false;
  let windowClosed = false;
  let processExit: number | undefined;
  const events: string[] = [];

  return {
    // A native close request (WM_CLOSE / the window manager's close button).
    nativeCloseStimulus() {
      // tauri-2.11.2 src/manager/window.rs: CloseRequested with a registered
      // JS listener is prevented natively FIRST, then re-emitted to the JS
      // listener. The window cannot close on this stimulus, whatever JS
      // does afterwards.
      events.push("manager-prevented-close");
      const e = { preventDefault() { events.push("js-prevent-default"); } };
      return (async () => {
        // --- App.tsx onCloseRequested body, mirrored ---
        if (allowClose) return; // second pass — let it through
        e.preventDefault();
        if (closeInProgress) return;
        closeInProgress = true;
        if ((await safeClose.prepare()) !== "accepted") {
          closeInProgress = false;
          return;
        }
        allowClose = true;
        // Final graph window: close_graph_window exits the process
        // (src-tauri/src/commands.rs: app.exit(0)).
        events.push("closeGraphWindow");
        processExit = 0;
        windowClosed = true;
      })();
    },
    windowClosed: () => windowClosed,
    processExitCode: () => processExit,
    events: () => events,
  };
}

function safeCloseHarness(overrides: Partial<SafeCloseDeps> = {}) {
  const deps: SafeCloseDeps = {
    blurActive: vi.fn(),
    endEdit: vi.fn(),
    flushPdfWork: vi.fn(async () => true),
    flushAll: vi.fn(async () => true),
    confirmDiscard: vi.fn(async () => false),
    flushSession: vi.fn(async () => {}),
    setTransition: vi.fn(),
    notifyPdfFailure: vi.fn(),
    notifyStillSaving: vi.fn(),
    notifyConfirmationFailure: vi.fn(),
    ...overrides,
  };
  return { deps, safeClose: createSafeCloseCoordinator(deps) };
}

describe("GH #455: Windows session-end exit machine", () => {
  it("agrees to WM_QUERYENDSESSION and exits after Tine's bounded cleanup callback", () => {
    const drained: string[] = [];
    const loop = createSessionEndLoop(() => {
      drained.push("drain+marker");
      return 0;
    });

    expect(loop.wmQueryEndSession()).toBe("agree");
    loop.wmEndSession();
    expect(loop.exitCallbackRan()).toBe(true);
    expect(drained).toEqual(["drain+marker"]);

    expect(loop.processExitCode()).toBe(0);
    loop.pump(WINDOWS_HUNG_APP_TIMEOUT_MS + 1_000);
    expect(loop.hungScreenShownAt()).toBeNull();
    loop.pump(WINDOWS_WAIT_TO_KILL_APP_TIMEOUT_MS + 1_000);
    expect(loop.forceKilledAt()).toBeNull();
  });

  it("normal control: AppHandle::exit breaks the loop at the first boundary and the process exits", () => {
    const loop = createSessionEndLoop(() => undefined);
    loop.requestExit(0);
    loop.pump(1);
    expect(loop.processExitCode()).toBe(0);
    expect(loop.hungScreenShownAt()).toBeNull();
    expect(loop.forceKilledAt()).toBeNull();
  });
});

describe("GH #455 research: close-request gate", () => {
  it("holds the window open past the OS timeouts while a flush stalls and the discard dialog is unanswered", async () => {
    vi.useFakeTimers();
    try {
      const never = new Promise<boolean>(() => {});
      const neverDiscard = new Promise<boolean>(() => {});
      const { deps, safeClose } = safeCloseHarness({
        flushAll: vi.fn(() => never),
        confirmDiscard: vi.fn(() => neverDiscard),
      });
      const gate = createTineCloseGate(safeClose);

      const closing = gate.nativeCloseStimulus();
      await vi.advanceTimersByTimeAsync(1);

      // First stimulus: prevented natively, prevented in JS, transaction open.
      expect(gate.events()).toEqual(["manager-prevented-close", "js-prevent-default"]);
      expect(gate.windowClosed()).toBe(false);

      // 4 s soft bound: still-saving notification, still no close.
      await vi.advanceTimersByTimeAsync(4_000);
      expect(deps.notifyStillSaving).toHaveBeenCalledOnce();
      expect(gate.windowClosed()).toBe(false);

      // 26 s grace bound: the discard dialog is up and nobody answers it
      // during an OS shutdown.
      await vi.advanceTimersByTimeAsync(26_000);
      expect(deps.confirmDiscard).toHaveBeenCalledOnce();
      expect(gate.windowClosed()).toBe(false);
      expect(gate.processExitCode()).toBeUndefined();

      // Far past both Windows timeouts: the window is still pending on a
      // human answer. Only the OS can end it.
      await vi.advanceTimersByTimeAsync(60_000);
      expect(gate.windowClosed()).toBe(false);
      expect(gate.processExitCode()).toBeUndefined();

      // A second close stimulus is absorbed by the re-entry guard: the
      // transaction is not restarted, the window still does not close.
      void gate.nativeCloseStimulus();
      await vi.advanceTimersByTimeAsync(1_000);
      expect(deps.flushAll).toHaveBeenCalledOnce();
      expect(gate.events()).toEqual([
        "manager-prevented-close",
        "js-prevent-default",
        "manager-prevented-close",
        "js-prevent-default",
      ]);
      expect(gate.windowClosed()).toBe(false);

      void closing;
    } finally {
      vi.useRealTimers();
    }
  });

  it("normal control: an accepted flush closes through closeGraphWindow and exits the process", async () => {
    const { deps, safeClose } = safeCloseHarness();
    const gate = createTineCloseGate(safeClose);

    await gate.nativeCloseStimulus();

    expect(deps.flushAll).toHaveBeenCalledOnce();
    expect(deps.flushSession).toHaveBeenCalledOnce();
    expect(deps.confirmDiscard).not.toHaveBeenCalled();
    expect(gate.events()).toEqual(["manager-prevented-close", "js-prevent-default", "closeGraphWindow"]);
    expect(gate.windowClosed()).toBe(true);
    expect(gate.processExitCode()).toBe(0);
  });
});

describe("GH #455: production exit shapes", () => {
  it("App.tsx still turns every close request into a prevented, async transaction", () => {
    const app = readFileSync("src/App.tsx", "utf8");
    expect(app).toContain("unlisten = await w.onCloseRequested(async (e) => {");
    // The always-prevent first pass: preventDefault runs before any await.
    expect(app).toMatch(
      /onCloseRequested\(async \(e\) => \{\s*if \(allowClose\) return;[^\n]*\n\s*e\.preventDefault\(\);/,
    );
    expect(app).toContain('if ((await safeClose.prepare()) !== "accepted")');
    expect(app).toContain("await backend().closeGraphWindow();");
    expect(app).toContain("await w.destroy();");
    expect(app).toContain("await w.close();");
  });

  it("the RunEvent::Exit callback cleans up and terminates Windows itself", () => {
    const lib = readFileSync("src-tauri/src/lib.rs", "utf8");
    const start = lib.indexOf("app.run(|");
    expect(start).toBeGreaterThanOrEqual(0);
    const run = lib.slice(start, lib.indexOf("});", start));
    expect(run).toContain("RunEvent::Exit");
    expect(run).toContain("drain_concord_ledgers_for_exit(");
    expect(run).toContain("mark_clean_shutdown()");
    expect(run).toContain("std::process::exit(0)");
  });
});
