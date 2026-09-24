import { QueryUnavailableError } from "../backend";

/** The index's failure code when `error` says the index could not be built
 *  and has stopped trying this session (GH #594, index liveness L4); `null`
 *  for any other error. The code is a fixed backend vocabulary, never prose. */
export function indexFailureOf(error: unknown): string | null {
  return error instanceof QueryUnavailableError && error.reasonCode === "index_failed"
    ? error.indexFailure ?? "other"
    : null;
}
