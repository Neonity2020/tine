// @vitest-environment jsdom

import { afterEach, describe, expect, it } from "vitest";
import {
  beginGraphOpenTrace,
  markGraphOpen,
  recordGraphOpenCommand,
  resetGraphOpenTraceForTest,
} from "./graphOpenTrace";

afterEach(() => {
  resetGraphOpenTraceForTest();
  document.body.replaceChildren();
});

describe("graph-open trace", () => {
  it("records first content only after the native graph binding is ready", () => {
    const block = document.createElement("div");
    block.className = "ls-block";
    document.body.append(block);

    beginGraphOpenTrace();
    expect(window.__TINE_GRAPH_OPEN_TRACE__?.milestones.first_content).toBeUndefined();

    markGraphOpen("native_binding_ready");
    const trace = window.__TINE_GRAPH_OPEN_TRACE__;
    expect(trace?.milestones.native_binding_ready).toBeTypeOf("number");
    expect(trace?.milestones.first_content).toBeTypeOf("number");
  });

  it("keeps the first value for every milestone", () => {
    beginGraphOpenTrace();
    markGraphOpen("session_restored");
    const first = window.__TINE_GRAPH_OPEN_TRACE__?.milestones.session_restored;
    markGraphOpen("session_restored");
    expect(window.__TINE_GRAPH_OPEN_TRACE__?.milestones.session_restored).toBe(first);
  });

  it("bounds command observations and counts dropped entries", () => {
    beginGraphOpenTrace();
    const started = performance.now();
    for (let index = 0; index < 131; index += 1) {
      recordGraphOpenCommand(`command-${index}`, started, index === 0 ? "failed" : "completed");
    }

    const trace = window.__TINE_GRAPH_OPEN_TRACE__;
    expect(trace?.commands).toHaveLength(128);
    expect(trace?.droppedCommands).toBe(3);
    expect(trace?.commands[0]).toMatchObject({ command: "command-0", outcome: "failed" });
    expect(trace?.commands.every((entry) => entry.startedMs >= 0 && entry.elapsedMs >= 0)).toBe(true);
  });
});
