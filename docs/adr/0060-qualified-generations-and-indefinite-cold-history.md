# 0060. Qualified generations bound ordinary work while retaining history

- **Status:** Accepted — implementation and cutover remain gated
- **Date:** 2026-09-07
- **Partially supersedes:** [ADR 0049](0049-oplog-first-sparse-storage.md): the first-rollout prohibition on retiring redundant hot representations, not the preservation of logical history.

## Context

Retaining every accepted edit in ordinary replay, document history and resident
indexes makes startup and maintenance grow with elapsed use. Indefinite history
is a product requirement; replaying it on every join is not. Disposable A5
checkpoints cannot become authority merely by being renamed.

## Decision

We will introduce qualified generations containing compact document state and
exact authenticated acceptance, identity and retention facts. The first generation
requires independent full rederivation. Subsequent generations inherit qualified
immutable roots and independently rederive their exact accepted delta. Full replay
remains the repair and audit oracle. Cutover is marker-last under the workspace
lease, with crash-safe replacement, retained fallback authority and preservation
of every acknowledged post-cutoff edit.

Original logical history remains recoverable indefinitely in cold storage. Hot
retirement requires complete retained closure, including Restore, receipt and
absence obligations. Cold history is excluded from ordinary admission and open
scans; exact historical identity queries use the active generation's point index.
A returning offline branch is recovered using exact original CRDT ancestry in an
isolated full document, preserving identities and concurrent work before a new
compact document is installed.

Enrolled own devices may join from qualified portable baselines after inbound
validation. Shared retirement additionally requires protocol fencing and a
non-destructive recovery path for excluded returning devices. Age or a missing
head alone cannot authorize retirement. Generation-aware sync belongs in the
readiness design for 0.7.0; completion requires its actual qualification.

Tine-side first join on the representative anonymized graph must take under ten
seconds on declared hardware/cache conditions with required shared bytes local,
including discovery, validation, loading, tail replay, projection and usable
rendering. Report transfer-inclusive time separately. Fixed live state must not
require ordinary join work proportional to retained history. This is an acceptance
requirement, not a measured result.

## Consequences

Storage remains proportional to retained history, while ordinary work follows
live state, bounded tail and current obligations. Recovery and initial qualification
can be expensive. Maintenance must make bounded progress even without idle time.
Implement operational bounds before cold deduplication; deduplication remains
required. No expiry policy, destructive rejoin or automatic Tine release is approved.
