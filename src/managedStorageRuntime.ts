import { createSignal, type Accessor } from "solid-js";
import { backend, type Backend } from "./backend";
import type {
  ApplicationPageAdmission,
  SparseV2ErrorEvent,
  SparseV2RuntimeStatus,
  SparseV2RuntimeStatusEvent,
  SparseV2Status,
  SparseV2Tick,
  SparseV2TickEvent,
} from "./types";

type RuntimeEventBackend = Pick<
  Backend,
  "sparseV2Status" | "onSparseV2Status" | "onSparseV2Tick" | "onSparseV2Error"
>;

export interface ManagedStorageRuntimeSnapshot {
  /** The only graph binding whose watcher events this store may accept. */
  bindingGeneration: number | null;
  /** Native writer capability for this exact binding, never inferred from status. */
  applicationPageAdmission: ApplicationPageAdmission | null;
  /** Full status returned by a graph-scoped command or the Storage & sync panel. */
  status: SparseV2Status | null;
  /** Latest runtime-only watcher status. Kept separately before the panel opens. */
  runtime: SparseV2RuntimeStatus | null;
  tick: SparseV2Tick | null;
  error: string | null;
}

const initialSnapshot = (): ManagedStorageRuntimeSnapshot => ({
  bindingGeneration: null,
  applicationPageAdmission: null,
  status: null,
  runtime: null,
  tick: null,
  error: null,
});

function isFailureTick(tick: SparseV2Tick): boolean {
  return ["recovery_blocked", "blocked", "terminal", "failed"].includes(tick.state);
}

/**
 * One window owns these three native subscriptions. Components consume its
 * reactive snapshot, so opening Settings cannot add another listener and a
 * rebound window cannot apply an event from its former graph.
 */
export function createManagedStorageRuntimeBridge(api: RuntimeEventBackend = backend()) {
  const [snapshot, setSnapshot] = createSignal<ManagedStorageRuntimeSnapshot>(initialSnapshot());
  let registration: Promise<() => void> | null = null;

  const accepts = (bindingGeneration: number) => snapshot().bindingGeneration === bindingGeneration;

  const bind = (
    bindingGeneration: number,
    applicationPageAdmission: ApplicationPageAdmission = {
      binding_generation: bindingGeneration,
      authority: "managed_unavailable",
    },
  ) => {
    if (applicationPageAdmission.binding_generation !== bindingGeneration) return false;
    if (snapshot().bindingGeneration === bindingGeneration) {
      setSnapshot((current) => ({ ...current, applicationPageAdmission }));
      return true;
    }
    setSnapshot({ ...initialSnapshot(), bindingGeneration, applicationPageAdmission });
    return true;
  };

  const clear = () => setSnapshot(initialSnapshot());

  const receiveStatus = (status: SparseV2Status): boolean => {
    if (!accepts(status.binding_generation)) return false;
    if (status.application_page_admission.binding_generation !== status.binding_generation) return false;
    setSnapshot((current) => ({
      ...current,
      status,
      applicationPageAdmission: status.application_page_admission,
      runtime: status.runtime,
      tick: status.runtime?.last_tick ?? current.tick,
    }));
    return true;
  };

  const transitionTo = (status: SparseV2Status, expectedPreviousBinding = snapshot().bindingGeneration): boolean => {
    if (snapshot().bindingGeneration !== expectedPreviousBinding) return false;
    if (status.application_page_admission.binding_generation !== status.binding_generation) return false;
    setSnapshot({
      bindingGeneration: status.binding_generation,
      applicationPageAdmission: status.application_page_admission,
      status,
      runtime: status.runtime,
      tick: status.runtime?.last_tick ?? null,
      error: null,
    });
    return true;
  };

  const receiveRuntimeStatus = (event: SparseV2RuntimeStatusEvent): boolean => {
    if (!accepts(event.binding_generation)) return false;
    setSnapshot((current) => ({
      ...current,
      runtime: event.runtime,
      status: current.status ? { ...current.status, runtime: event.runtime } : null,
      tick: event.runtime.last_tick ?? current.tick,
    }));
    return true;
  };

  const receiveTick = (event: SparseV2TickEvent): boolean => {
    if (!accepts(event.binding_generation)) return false;
    setSnapshot((current) => {
      const runtime = current.runtime
        ? { ...current.runtime, last_tick: event.tick }
        : current.runtime;
      return {
        ...current,
        runtime,
        status: current.status && runtime ? { ...current.status, runtime } : current.status,
        tick: event.tick,
        // The native watcher emits an error before its failure tick; a later
        // healthy tick is the matching recovery signal.
        error: isFailureTick(event.tick) ? current.error : null,
      };
    });
    return true;
  };

  const receiveError = (event: SparseV2ErrorEvent): boolean => {
    if (!accepts(event.binding_generation)) return false;
    setSnapshot((current) => current.error === event.message ? current : { ...current, error: event.message });
    return true;
  };

  const refresh = async (): Promise<SparseV2Status | null> => {
    const requestedBinding = snapshot().bindingGeneration;
    const status = await api.sparseV2Status();
    const currentBinding = snapshot().bindingGeneration;
    if (currentBinding !== null && currentBinding !== requestedBinding) return null;
    if (requestedBinding !== null && status.binding_generation !== requestedBinding) return null;
    if (currentBinding === null) bind(status.binding_generation, status.application_page_admission);
    return receiveStatus(status) ? status : null;
  };

  const listen = (): Promise<() => void> => {
    if (registration) return registration;
    let promise: Promise<() => void>;
    promise = Promise.all([
      api.onSparseV2Status(receiveRuntimeStatus),
      api.onSparseV2Tick(receiveTick),
      api.onSparseV2Error(receiveError),
    ]).then((unlisteners) => () => {
      for (const unlisten of unlisteners) unlisten();
      if (registration === promise) registration = null;
    });
    registration = promise;
    return promise;
  };

  return {
    snapshot: snapshot as Accessor<ManagedStorageRuntimeSnapshot>,
    bind,
    clear,
    transitionTo,
    receiveStatus,
    receiveRuntimeStatus,
    receiveTick,
    receiveError,
    refresh,
    listen,
  };
}

export const managedStorageRuntime = createManagedStorageRuntimeBridge();

export function managedStorageRuntimeErrorMessage(reason: string): string {
  return `Tine-managed storage needs attention: ${reason}\n\nOpen Storage & sync to inspect recovery status.`;
}
