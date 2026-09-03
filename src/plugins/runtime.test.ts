import { describe, expect, it } from "vitest";
import { PluginRuntime, PluginRuntimeError } from "./runtime";

class SilentWorker {
  onmessage: ((event: MessageEvent) => void) | null = null;
  onerror: ((event: ErrorEvent) => void) | null = null;
  terminated = false;
  postMessage(_message: unknown, _transfer?: Transferable[]) {}
  terminate() {
    this.terminated = true;
  }
}

describe("PluginRuntime", () => {
  it("terminates an unresponsive guest at the hard deadline", async () => {
    const worker = new SilentWorker();
    await expect(
      PluginRuntime.create(new ArrayBuffer(0), {
        workerFactory: () => worker,
        initializationTimeoutMs: 5,
      })
    ).rejects.toThrow(/time limit/);
    expect(worker.terminated).toBe(true);
  });

  it("keeps the pre-existing guest-visible string error shape", async () => {
    class RefusingWorker extends SilentWorker {
      override postMessage(message: unknown, _transfer?: Transferable[]) {
        const request = message as { id: number; kind: string };
        queueMicrotask(() => {
          this.onmessage?.({
            data: request.kind === "init"
              ? { id: request.id, ok: true }
              : { id: request.id, ok: false, error: "guest refusal" },
          } as MessageEvent);
        });
      }
    }

    const runtime = await PluginRuntime.create(new ArrayBuffer(0), {
      workerFactory: () => new RefusingWorker(),
    });
    const failure = await runtime.invoke({} as never).catch((error) => error);
    expect(failure).toBeInstanceOf(PluginRuntimeError);
    expect(failure).toMatchObject({ name: "PluginRuntimeError", message: "guest refusal" });
    runtime.dispose();
  });
});
