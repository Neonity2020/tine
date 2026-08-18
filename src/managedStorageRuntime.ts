import { createSignal, type Accessor } from "solid-js";
import { backend, type Backend } from "./backend";
import type {
  ApplicationPageAdmission,
  ManagedApplicationMoveSubtreesRecoveryResult,
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
  /**
   * The one report the user still owes an acknowledgement for. A persistently
   * failing actor emits the SAME error again after every retry, and each retry
   * passes through an in-flight `recovering` tick, so keying user-facing
   * feedback off `error` alone produced an unbounded stream of identical red
   * toasts for one condition (GH: Android, 2026-08-18). The sequence only ever
   * advances for a message the user has not been shown yet, so a repeat of a
   * condition that is still live is silent while the panel keeps showing it.
   */
  notice: { message: string; sequence: number } | null;
}

const initialSnapshot = (): ManagedStorageRuntimeSnapshot => ({
  bindingGeneration: null,
  applicationPageAdmission: null,
  status: null,
  runtime: null,
  tick: null,
  error: null,
  notice: null,
});

/**
 * Did the actor actually get somewhere? `recovering` and `retry_full` are the
 * in-flight steps of the very retry that is about to fail again, so they are
 * neither the failure nor its resolution: treating them as recovery is what let
 * one permanently blocked condition re-arm the toast on every cycle.
 */
function isRecoveredTick(tick: SparseV2Tick): boolean {
  return ["idle", "admitted_noop", "admitted_complete", "local_mutation", "provider_mutation"]
    .includes(tick.state);
}

function admissionsAgree(left: ApplicationPageAdmission, right: ApplicationPageAdmission): boolean {
  if (left.binding_generation !== right.binding_generation || left.authority !== right.authority) return false;
  if (left.authority !== "managed_writable" || right.authority !== "managed_writable") return true;
  return left.application_save_page_blocks === right.application_save_page_blocks
    && left.application_page_request_text_bytes === right.application_page_request_text_bytes
    && left.application_page_max_depth === right.application_page_max_depth;
}

/**
 * One window owns these three native subscriptions. Components consume its
 * reactive snapshot, so opening Settings cannot add another listener and a
 * rebound window cannot apply an event from its former graph.
 */
export function createManagedStorageRuntimeBridge(api: RuntimeEventBackend = backend()) {
  const [snapshot, setSnapshot] = createSignal<ManagedStorageRuntimeSnapshot>(initialSnapshot());
  let registration: Promise<() => void> | null = null;
  // Monotonic across the whole window: a report the user has already been shown
  // must never be re-raised, and a genuine recurrence after recovery must never
  // reuse a sequence the effect has already seen.
  let noticeSequence = 0;
  let noticedMessage: string | null = null;
  const clearNotice = () => {
    noticedMessage = null;
  };

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
    clearNotice();
    setSnapshot({ ...initialSnapshot(), bindingGeneration, applicationPageAdmission });
    return true;
  };

  const clear = () => {
    clearNotice();
    setSnapshot(initialSnapshot());
  };

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

  const acceptNativeTransition = (status: SparseV2Status): boolean => {
    if (status.application_page_admission.binding_generation !== status.binding_generation) return false;
    clearNotice();
    setSnapshot({
      bindingGeneration: status.binding_generation,
      applicationPageAdmission: status.application_page_admission,
      status,
      runtime: status.runtime,
      tick: status.runtime?.last_tick ?? null,
      error: null,
      notice: null,
    });
    return true;
  };

  /** X2 supplies the synchronous busy-token/graph/page-instance ownership
   * check. This bridge adds no episode state machine; it only validates the
   * native handoff envelope immediately before the existing atomic transition. */
  const transitionMoveRecovery = (
    result: ManagedApplicationMoveSubtreesRecoveryResult,
    expectedEpisodeId: string,
    expectedPreviousBinding: number,
    ownsRecovery: () => boolean,
  ): boolean => {
    if (!ownsRecovery()) return false;
    if (snapshot().bindingGeneration !== expectedPreviousBinding) return false;
    if (result.previous_binding_generation !== expectedPreviousBinding) return false;
    if (result.episode_id !== expectedEpisodeId || result.outcome.episode_id !== expectedEpisodeId) return false;
    if (result.binding_generation !== result.status.binding_generation) return false;
    if (!admissionsAgree(result.application_page_admission, result.status.application_page_admission)) return false;
    return acceptNativeTransition(result.status);
  };

  const receiveRuntimeStatus = (event: SparseV2RuntimeStatusEvent): boolean => {
    if (!accepts(event.binding_generation)) return false;
    if (event.application_page_admission.binding_generation !== event.binding_generation) return false;
    setSnapshot((current) => ({
      ...current,
      applicationPageAdmission: event.application_page_admission,
      runtime: event.runtime,
      status: current.status
        ? {
            ...current.status,
            runtime: event.runtime,
            application_page_admission: event.application_page_admission,
          }
        : null,
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
      // The native watcher emits an error before its failure tick; a later
      // HEALTHY tick is the matching recovery signal. An in-flight retry step
      // is not one, so it neither clears the condition nor re-arms its report.
      const recovered = isRecoveredTick(event.tick);
      if (recovered) clearNotice();
      return {
        ...current,
        runtime,
        status: current.status && runtime ? { ...current.status, runtime } : current.status,
        tick: event.tick,
        error: recovered ? null : current.error,
        notice: recovered ? null : current.notice,
      };
    });
    return true;
  };

  const receiveError = (event: SparseV2ErrorEvent): boolean => {
    if (!accepts(event.binding_generation)) return false;
    if (noticedMessage !== event.message) {
      noticedMessage = event.message;
      noticeSequence += 1;
    }
    const notice = { message: event.message, sequence: noticeSequence };
    setSnapshot((current) => {
      // A `managed_writable` admission is evidence that a live actor will save
      // what we accept. A failing tick withdraws that evidence, so writability
      // is revoked here rather than left standing until some later status
      // happens to arrive — the native watcher suppresses a repeated error, so
      // a persistently failing actor may send nothing else at all (GH #324).
      // Only an authoritative status can restore it.
      const admission = current.applicationPageAdmission;
      const revoked: ApplicationPageAdmission | null =
        admission?.authority === "managed_writable"
          ? { binding_generation: admission.binding_generation, authority: "managed_unavailable" }
          : admission;
      if (current.error === event.message && notice === current.notice && revoked === admission) {
        return current;
      }
      return {
        ...current,
        error: event.message,
        notice,
        applicationPageAdmission: revoked,
        status: current.status && revoked
          ? { ...current.status, application_page_admission: revoked }
          : current.status,
      };
    });
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
    acceptNativeTransition,
    transitionMoveRecovery,
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
