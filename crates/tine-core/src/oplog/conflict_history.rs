//! Disposable conflict-history index for bounded resolution evaluation.
//!
//! Immutable accepted batches remain the sole authority. This run-local index
//! is advanced in acceptance order and rebuilt from that authority whenever
//! its acceptance-sequence stamp does not match the engine frontier.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    BatchCausalDot, BatchId, BatchOrigin, BlockDelta, BlockId, BlockState, CausalPeerId,
    ContentDigest, ManagedPath, PageId,
};

/// The one causal-clock membership predicate used by accepted admission and
/// the disposable conflict index. Clocks are canonical peer-sorted vectors.
pub(crate) fn causal_clock_contains_dot(
    clock: &[(CausalPeerId, u64)],
    dot: BatchCausalDot,
) -> bool {
    clock
        .binary_search_by_key(&dot.peer_id(), |(peer, _)| *peer)
        .ok()
        .is_some_and(|index| clock[index].1 >= dot.counter())
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ProjectionCreateKey {
    page_id: PageId,
    path: ManagedPath,
    target_digest: ContentDigest,
}

impl ProjectionCreateKey {
    pub(crate) fn new(page_id: PageId, path: ManagedPath, target_digest: ContentDigest) -> Self {
        Self {
            page_id,
            path,
            target_digest,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ConflictHistoryBatch {
    batch_id: BatchId,
    causal_dot: BatchCausalDot,
    causal_clock: Vec<(CausalPeerId, u64)>,
    origin: BatchOrigin,
    block_post_states: BTreeMap<BlockId, Option<BlockState>>,
    projection_create_keys: BTreeSet<ProjectionCreateKey>,
}

impl ConflictHistoryBatch {
    pub(crate) fn new(
        batch_id: BatchId,
        causal_dot: BatchCausalDot,
        causal_clock: Vec<(CausalPeerId, u64)>,
        origin: BatchOrigin,
        deltas: &[BlockDelta],
        projection_create_keys: Vec<ProjectionCreateKey>,
    ) -> Self {
        Self {
            batch_id,
            causal_dot,
            causal_clock,
            origin,
            block_post_states: deltas
                .iter()
                .map(|delta| (delta.block_id, delta.after.clone()))
                .collect(),
            projection_create_keys: projection_create_keys.into_iter().collect(),
        }
    }

    fn contains(&self, ancestor: &Self) -> bool {
        self.batch_id == ancestor.batch_id
            || causal_clock_contains_dot(&self.causal_clock, ancestor.causal_dot)
    }

    pub(crate) fn post_state(&self, block_id: BlockId) -> Option<&BlockState> {
        self.block_post_states.get(&block_id)?.as_ref()
    }

    fn projection_origin_matches(&self, other: &Self) -> bool {
        matches!(
            (self.origin, other.origin),
            (
                BatchOrigin::ExternalReconciliation { .. },
                BatchOrigin::LocalMutation
            ) | (
                BatchOrigin::LocalMutation,
                BatchOrigin::ExternalReconciliation { .. }
            )
        )
    }
}

#[derive(Default)]
struct ConflictBlockHistory {
    touches: BTreeSet<BatchId>,
    causal_tips: BTreeSet<BatchId>,
    unresolved_pairs: BTreeSet<(BatchId, BatchId)>,
    /// Transitive conflict-pair members settled by one descendant touch.
    /// Later concurrent branch work needs these sparse ancestors to retire a
    /// provisional keep-both sibling without scanning ordinary history.
    settlement_ancestors: BTreeMap<BatchId, BTreeSet<BatchId>>,
}

#[derive(Default)]
struct ProjectionCreateHistory {
    touches: BTreeSet<BatchId>,
    causal_tips: BTreeSet<BatchId>,
}

#[derive(Default)]
pub(crate) struct ConflictHistoryIndex {
    acceptance_sequence: u64,
    batches: BTreeMap<BatchId, ConflictHistoryBatch>,
    batch_sequences: BTreeMap<BatchId, u64>,
    blocks: BTreeMap<BlockId, ConflictBlockHistory>,
    projection_creates: BTreeMap<ProjectionCreateKey, ProjectionCreateHistory>,
}

impl ConflictHistoryIndex {
    pub(crate) fn is_current(&self, acceptance_sequence: u64) -> bool {
        self.acceptance_sequence == acceptance_sequence
    }

    pub(crate) fn advance(&mut self, acceptance_sequence: u64, batch: ConflictHistoryBatch) {
        let batch_id = batch.batch_id;
        if self.batches.contains_key(&batch_id) {
            self.acceptance_sequence = acceptance_sequence;
            return;
        }
        let touched_blocks = batch.block_post_states.keys().copied().collect::<Vec<_>>();
        let projection_create_keys = batch
            .projection_create_keys
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        self.batches.insert(batch_id, batch);
        self.batch_sequences.insert(batch_id, acceptance_sequence);

        for block_id in touched_blocks {
            let prior_tips = self
                .blocks
                .get(&block_id)
                .map(|history| history.causal_tips.iter().copied().collect::<Vec<_>>())
                .unwrap_or_default();
            let has_concurrent_tip = prior_tips
                .iter()
                .any(|tip| !self.contains(batch_id, *tip) && !self.contains(*tip, batch_id));
            let settled_pairs = self
                .blocks
                .get(&block_id)
                .map(|history| {
                    history
                        .unresolved_pairs
                        .iter()
                        .copied()
                        .filter(|(left, right)| {
                            self.contains(batch_id, *left) && self.contains(batch_id, *right)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let settlement_ancestors = self
                .blocks
                .get(&block_id)
                .map(|history| {
                    settled_pairs
                        .iter()
                        .flat_map(|(left, right)| [*left, *right])
                        .flat_map(|member| {
                            std::iter::once(member).chain(
                                history
                                    .settlement_ancestors
                                    .get(&member)
                                    .into_iter()
                                    .flat_map(|ancestors| ancestors.iter().copied()),
                            )
                        })
                        .collect::<BTreeSet<_>>()
                })
                .unwrap_or_default();
            // The common linear case compares only causal tips. If any tip is
            // concurrent, every concurrent touch is a distinct unresolved
            // pair under the existing semantics, so enumerating touches is
            // proportional to the new C rather than hidden lifetime work.
            let concurrent = if has_concurrent_tip {
                self.blocks
                    .get(&block_id)
                    .into_iter()
                    .flat_map(|history| history.touches.iter().copied())
                    .filter(|other| {
                        !self.contains(batch_id, *other) && !self.contains(*other, batch_id)
                    })
                    .map(|other| ordered_pair(batch_id, other))
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let tips_containing_new = prior_tips.iter().any(|tip| self.contains(*tip, batch_id));
            let descended_tips = prior_tips
                .into_iter()
                .filter(|tip| self.contains(batch_id, *tip))
                .collect::<Vec<_>>();

            let history = self.blocks.entry(block_id).or_default();
            for pair in settled_pairs {
                history.unresolved_pairs.remove(&pair);
            }
            if !settlement_ancestors.is_empty() {
                history
                    .settlement_ancestors
                    .insert(batch_id, settlement_ancestors);
            }
            history.unresolved_pairs.extend(concurrent);
            history.touches.insert(batch_id);
            for tip in descended_tips {
                history.causal_tips.remove(&tip);
            }
            if !tips_containing_new {
                history.causal_tips.insert(batch_id);
            }
        }

        for key in projection_create_keys {
            let prior_tips = self
                .projection_creates
                .get(&key)
                .map(|history| history.causal_tips.iter().copied().collect::<Vec<_>>())
                .unwrap_or_default();
            let has_concurrent_opposite_tip = prior_tips.iter().any(|tip| {
                self.batches[&batch_id].projection_origin_matches(&self.batches[tip])
                    && !self.contains(batch_id, *tip)
                    && !self.contains(*tip, batch_id)
            });
            let concurrent = if has_concurrent_opposite_tip {
                self.projection_creates
                    .get(&key)
                    .into_iter()
                    .flat_map(|history| history.touches.iter().copied())
                    .filter(|other| {
                        self.batches[&batch_id].projection_origin_matches(&self.batches[other])
                            && !self.contains(batch_id, *other)
                            && !self.contains(*other, batch_id)
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let tips_containing_new = prior_tips.iter().any(|tip| self.contains(*tip, batch_id));
            let descended_tips = prior_tips
                .into_iter()
                .filter(|tip| self.contains(batch_id, *tip))
                .collect::<Vec<_>>();

            for other in concurrent {
                let pair = ordered_pair(batch_id, other);
                let pair_blocks = self.batches[&batch_id]
                    .block_post_states
                    .keys()
                    .chain(self.batches[&other].block_post_states.keys())
                    .copied()
                    .collect::<BTreeSet<_>>();
                for block_id in pair_blocks {
                    self.blocks
                        .entry(block_id)
                        .or_default()
                        .unresolved_pairs
                        .insert(pair);
                }
            }

            let history = self.projection_creates.entry(key).or_default();
            history.touches.insert(batch_id);
            for tip in descended_tips {
                history.causal_tips.remove(&tip);
            }
            if !tips_containing_new {
                history.causal_tips.insert(batch_id);
            }
        }
        self.acceptance_sequence = acceptance_sequence;
    }

    pub(crate) fn candidates(&self, batch_id: BatchId) -> Vec<BatchId> {
        let Some(batch) = self.batches.get(&batch_id) else {
            return Vec::new();
        };
        let mut candidates = BTreeSet::new();
        for block_id in batch.block_post_states.keys() {
            let Some(history) = self.blocks.get(block_id) else {
                continue;
            };
            for (left, right) in &history.unresolved_pairs {
                if *left == batch_id {
                    candidates.insert(*right);
                } else if *right == batch_id {
                    candidates.insert(*left);
                }
            }
        }
        let mut candidates = candidates.into_iter().collect::<Vec<_>>();
        candidates.sort_unstable_by_key(|candidate| self.batch_sequences[candidate]);
        candidates
    }

    pub(crate) fn pair_is_unresolved(
        &self,
        block_id: BlockId,
        left: BatchId,
        right: BatchId,
    ) -> bool {
        self.blocks.get(&block_id).is_some_and(|history| {
            history
                .unresolved_pairs
                .contains(&ordered_pair(left, right))
        })
    }

    pub(crate) fn unresolved_members(&self, block_id: BlockId) -> Vec<BatchId> {
        let Some(history) = self.blocks.get(&block_id) else {
            return Vec::new();
        };
        let mut members = history
            .unresolved_pairs
            .iter()
            .flat_map(|(left, right)| [*left, *right])
            .collect::<BTreeSet<_>>();
        let endpoints = members.iter().copied().collect::<Vec<_>>();
        for endpoint in endpoints {
            if let Some(ancestors) = history.settlement_ancestors.get(&endpoint) {
                members.extend(ancestors.iter().copied());
            }
        }
        members.into_iter().collect()
    }

    pub(crate) fn contains(&self, descendant: BatchId, ancestor: BatchId) -> bool {
        let (Some(descendant), Some(ancestor)) =
            (self.batches.get(&descendant), self.batches.get(&ancestor))
        else {
            return false;
        };
        descendant.contains(ancestor)
    }

    pub(crate) fn post_state(&self, batch_id: BatchId, block_id: BlockId) -> Option<&BlockState> {
        self.batches.get(&batch_id)?.post_state(block_id)
    }

    #[cfg(test)]
    pub(crate) fn unresolved_pair_count(&self) -> usize {
        self.blocks
            .values()
            .map(|history| history.unresolved_pairs.len())
            .sum()
    }
}

fn ordered_pair(left: BatchId, right: BatchId) -> (BatchId, BatchId) {
    if left < right {
        (left, right)
    } else {
        (right, left)
    }
}
