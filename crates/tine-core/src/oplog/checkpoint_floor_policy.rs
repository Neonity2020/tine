//! Device-local age and byte policy for shallow checkpoint floors.
//!
//! This module decides whether a measured document needs a cut and which
//! verified native candidate is legal. Its Loro adapter performs disposable
//! worker-side exports and verification imports, but it cannot author an
//! operation or publish a checkpoint. Keeping publication outside the policy
//! makes it impossible for size or time alone to invent CRDT history
//! (D-14/I-14).

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

pub(crate) const RETAINED_HISTORY_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
pub(crate) const DEFAULT_MINIMUM_TAIL_BYTES: u64 = 256 * 1024;
pub(crate) const DEFAULT_LIVE_SIZE_MULTIPLIER: u64 = 4;
const AGE_POLICY_SCHEMA_VERSION: u32 = 1;
const MAX_CLOCK_DELTA_SKEW_MS: u64 = 5 * 60 * 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptanceObservation {
    /// Every accepted sequence after the preceding observation and through this
    /// sequence has this conservative UTC upper bound.
    through_sequence: u64,
    utc_upper_bound_ms: i64,
}

/// Disposable device-local policy facts. Accepted history remains authority;
/// losing this value calls [`Self::recover_missing`] and merely delays cutting.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptanceAgePolicy {
    schema_version: u32,
    eligible_through: u64,
    observed_through: u64,
    observations: VecDeque<AcceptanceObservation>,
    latest_acceptance_utc_ms: i64,
    last_observed_utc_ms: i64,
    /// Process monotonic time is deliberately cleared by `decode_current`.
    last_observed_monotonic_ms: Option<u64>,
    clock_frozen: bool,
}

impl AcceptanceAgePolicy {
    /// Genesis gets a fresh local observation, but is not itself an accepted
    /// batch sequence. With no later acceptance, wall-clock time cannot move a
    /// floor because no method reevaluates eligibility.
    pub(crate) fn fresh(utc_ms: i64, monotonic_ms: u64) -> Result<Self, FloorPolicyError> {
        if utc_ms < 0 {
            return Err(FloorPolicyError::InvalidClock);
        }
        Ok(Self {
            schema_version: AGE_POLICY_SCHEMA_VERSION,
            eligible_through: 0,
            observed_through: 0,
            observations: VecDeque::new(),
            latest_acceptance_utc_ms: utc_ms,
            last_observed_utc_ms: utc_ms,
            last_observed_monotonic_ms: Some(monotonic_ms),
            clock_frozen: false,
        })
    }

    /// Missing policy metadata is not a format fallback. The current accepted
    /// prefix receives a new conservative upper bound, so it must wait another
    /// complete retention window before becoming eligible.
    pub(crate) fn recover_missing(
        accepted_through: u64,
        utc_ms: i64,
        monotonic_ms: u64,
    ) -> Result<Self, FloorPolicyError> {
        let mut policy = Self::fresh(utc_ms, monotonic_ms)?;
        policy.observed_through = accepted_through;
        if accepted_through != 0 {
            policy.observations.push_back(AcceptanceObservation {
                through_sequence: accepted_through,
                utc_upper_bound_ms: utc_ms,
            });
        }
        Ok(policy)
    }

    pub(crate) fn decode_current(bytes: &[u8], utc_now_ms: i64) -> Result<Self, FloorPolicyError> {
        if utc_now_ms < 0 {
            return Err(FloorPolicyError::InvalidClock);
        }
        let (mut policy, trailing): (Self, &[u8]) =
            postcard::take_from_bytes(bytes).map_err(|_| FloorPolicyError::InvalidEncoding)?;
        if !trailing.is_empty()
            || policy.schema_version != AGE_POLICY_SCHEMA_VERSION
            || postcard::to_allocvec(&policy).map_err(|_| FloorPolicyError::InvalidEncoding)?
                != bytes
        {
            return Err(FloorPolicyError::InvalidEncoding);
        }
        policy.validate()?;
        // A process monotonic reading has no meaning after restart. UTC remains
        // the persisted policy clock; a detectable rollback freezes advancement
        // until the caller establishes a new conservative epoch.
        policy.last_observed_monotonic_ms = None;
        if utc_now_ms < policy.last_observed_utc_ms {
            policy.clock_frozen = true;
        }
        Ok(policy)
    }

    pub(crate) fn encode_current(&self) -> Result<Vec<u8>, FloorPolicyError> {
        self.validate()?;
        postcard::to_allocvec(self).map_err(|_| FloorPolicyError::InvalidEncoding)
    }

    pub(crate) fn observe_acceptance(
        &mut self,
        sequence: u64,
        utc_ms: i64,
        monotonic_ms: u64,
    ) -> Result<(), FloorPolicyError> {
        if sequence <= self.observed_through {
            return Ok(());
        }
        if sequence != self.observed_through.saturating_add(1) || utc_ms < 0 {
            return Err(FloorPolicyError::NoncontiguousAcceptance);
        }
        let discontinuity = utc_ms < self.last_observed_utc_ms
            || self
                .last_observed_monotonic_ms
                .is_some_and(|last| monotonic_ms < last)
            || self.last_observed_monotonic_ms.is_some_and(|last| {
                let utc_delta = utc_ms.saturating_sub(self.last_observed_utc_ms) as u64;
                let monotonic_delta = monotonic_ms.saturating_sub(last);
                utc_delta.abs_diff(monotonic_delta) > MAX_CLOCK_DELTA_SKEW_MS
            });
        self.observed_through = sequence;
        self.last_observed_utc_ms = utc_ms;
        self.last_observed_monotonic_ms = Some(monotonic_ms);
        if self.clock_frozen || discontinuity {
            self.clock_frozen = true;
            return Ok(());
        }
        self.latest_acceptance_utc_ms = utc_ms;
        self.observations.push_back(AcceptanceObservation {
            through_sequence: sequence,
            utc_upper_bound_ms: utc_ms,
        });
        self.advance_eligible();
        Ok(())
    }

    /// Start a new trustworthy clock epoch after rollback/discontinuity. Every
    /// not-yet-eligible acceptance is conservatively no older than this moment.
    pub(crate) fn reestablish_clock(
        &mut self,
        utc_ms: i64,
        monotonic_ms: u64,
    ) -> Result<(), FloorPolicyError> {
        if utc_ms < 0 {
            return Err(FloorPolicyError::InvalidClock);
        }
        self.observations.clear();
        if self.observed_through > self.eligible_through {
            self.observations.push_back(AcceptanceObservation {
                through_sequence: self.observed_through,
                utc_upper_bound_ms: utc_ms,
            });
        }
        self.latest_acceptance_utc_ms = utc_ms;
        self.last_observed_utc_ms = utc_ms;
        self.last_observed_monotonic_ms = Some(monotonic_ms);
        self.clock_frozen = false;
        Ok(())
    }

    pub(crate) const fn eligible_through(&self) -> u64 {
        self.eligible_through
    }

    pub(crate) const fn observed_through(&self) -> u64 {
        self.observed_through
    }

    pub(crate) const fn clock_frozen(&self) -> bool {
        self.clock_frozen
    }

    pub(crate) const fn latest_acceptance_utc_ms(&self) -> i64 {
        self.latest_acceptance_utc_ms
    }

    fn advance_eligible(&mut self) {
        let cutoff = self
            .latest_acceptance_utc_ms
            .saturating_sub(RETAINED_HISTORY_MS);
        for observation in &self.observations {
            if observation.through_sequence <= self.eligible_through {
                continue;
            }
            if observation.utc_upper_bound_ms > cutoff {
                break;
            }
            self.eligible_through = observation.through_sequence;
        }
        while self
            .observations
            .front()
            .is_some_and(|observation| observation.through_sequence <= self.eligible_through)
        {
            self.observations.pop_front();
        }
    }

    fn validate(&self) -> Result<(), FloorPolicyError> {
        if self.eligible_through > self.observed_through
            || self.latest_acceptance_utc_ms < 0
            || self.last_observed_utc_ms < 0
        {
            return Err(FloorPolicyError::InvalidEncoding);
        }
        let mut previous = self.eligible_through;
        for observation in &self.observations {
            if observation.through_sequence <= previous
                || observation.through_sequence > self.observed_through
                || observation.utc_upper_bound_ms < 0
            {
                return Err(FloorPolicyError::InvalidEncoding);
            }
            previous = observation.through_sequence;
        }
        if self.observed_through > self.eligible_through
            && !self.clock_frozen
            && self
                .observations
                .back()
                .is_none_or(|observation| observation.through_sequence != self.observed_through)
        {
            return Err(FloorPolicyError::InvalidEncoding);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FloorPolicyConfig {
    pub(crate) revision: u32,
    pub(crate) minimum_tail_bytes: u64,
    pub(crate) live_size_multiplier: u64,
}

impl Default for FloorPolicyConfig {
    fn default() -> Self {
        Self {
            revision: 1,
            minimum_tail_bytes: DEFAULT_MINIMUM_TAIL_BYTES,
            live_size_multiplier: DEFAULT_LIVE_SIZE_MULTIPLIER,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImageMeasurement {
    pub(crate) image_bytes: u64,
    pub(crate) latest_state_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CandidateMeasurement {
    pub(crate) requested_k: u64,
    /// The greatest accepted prefix the verified native shallow root removes.
    pub(crate) actual_removed_through: u64,
    pub(crate) image_bytes: u64,
    /// Established by native-frontier comparison, because an actual shallow
    /// root need not coincide with any whole accepted transaction boundary.
    pub(crate) advances_current: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum LimitingCause {
    AgeLowerBound,
    NativeNormalization,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FloorMetrics {
    pub(crate) image_bytes: u64,
    pub(crate) latest_state_bytes: u64,
    pub(crate) removable_bytes: u64,
    pub(crate) budget_bytes: u64,
    pub(crate) post_cut_removable_bytes: u64,
    pub(crate) hysteresis_shortfall_bytes: u64,
    pub(crate) budget_overage_bytes: u64,
    pub(crate) limiting_cause: Option<LimitingCause>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FloorDecision {
    Keep {
        metrics: FloorMetrics,
    },
    Advance {
        chosen: CandidateMeasurement,
        metrics: FloorMetrics,
    },
}

/// Select only among candidates already exported and re-imported by the worker.
/// Candidate sizes are deliberately scanned in K order: compression is not
/// assumed monotone, so binary search would be unsound.
pub(crate) fn choose_floor(
    config: FloorPolicyConfig,
    eligible_through: u64,
    current: ImageMeasurement,
    candidates: impl IntoIterator<Item = CandidateMeasurement>,
) -> Result<FloorDecision, FloorPolicyError> {
    if config.minimum_tail_bytes == 0 || config.live_size_multiplier == 0 {
        return Err(FloorPolicyError::InvalidConfiguration);
    }
    let budget = config.minimum_tail_bytes.max(
        config
            .live_size_multiplier
            .saturating_mul(current.latest_state_bytes),
    );
    let removable = current
        .image_bytes
        .saturating_sub(current.latest_state_bytes);
    let base_metrics = FloorMetrics {
        image_bytes: current.image_bytes,
        latest_state_bytes: current.latest_state_bytes,
        removable_bytes: removable,
        budget_bytes: budget,
        post_cut_removable_bytes: removable,
        hysteresis_shortfall_bytes: 0,
        budget_overage_bytes: removable.saturating_sub(budget),
        limiting_cause: None,
    };
    if removable <= budget {
        return Ok(FloorDecision::Keep {
            metrics: base_metrics,
        });
    }

    let mut candidates = candidates.into_iter().collect::<Vec<_>>();
    candidates.sort_unstable_by_key(|candidate| candidate.requested_k);
    let mut prior_requested = None;
    let mut safe = Vec::new();
    for candidate in candidates {
        if prior_requested == Some(candidate.requested_k) {
            return Err(FloorPolicyError::DuplicateCandidate);
        }
        prior_requested = Some(candidate.requested_k);
        if candidate.requested_k > eligible_through
            || candidate.actual_removed_through > candidate.requested_k
        {
            return Err(FloorPolicyError::CandidateCrossesAgeBoundary);
        }
        if candidate.advances_current {
            safe.push(candidate);
        }
    }
    let half_budget = budget / 2;
    let sufficient = safe.iter().copied().find(|candidate| {
        candidate
            .image_bytes
            .saturating_sub(current.latest_state_bytes)
            < half_budget
    });
    let chosen = sufficient.or_else(|| {
        safe.iter().copied().max_by(|left, right| {
            left.actual_removed_through
                .cmp(&right.actual_removed_through)
                .then_with(|| right.requested_k.cmp(&left.requested_k))
        })
    });
    let Some(chosen) = chosen else {
        return Ok(FloorDecision::Keep {
            metrics: FloorMetrics {
                hysteresis_shortfall_bytes: removable.saturating_sub(half_budget),
                limiting_cause: Some(LimitingCause::AgeLowerBound),
                ..base_metrics
            },
        });
    };
    let post_cut = chosen
        .image_bytes
        .saturating_sub(current.latest_state_bytes);
    let cause = if post_cut < half_budget {
        None
    } else if chosen.actual_removed_through < chosen.requested_k {
        Some(LimitingCause::NativeNormalization)
    } else {
        Some(LimitingCause::AgeLowerBound)
    };
    Ok(FloorDecision::Advance {
        chosen,
        metrics: FloorMetrics {
            post_cut_removable_bytes: post_cut,
            hysteresis_shortfall_bytes: post_cut.saturating_sub(half_budget),
            budget_overage_bytes: post_cut.saturating_sub(budget),
            limiting_cause: cause,
            ..base_metrics
        },
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FloorPolicyError {
    InvalidClock,
    NoncontiguousAcceptance,
    InvalidEncoding,
    InvalidConfiguration,
    DuplicateCandidate,
    CandidateCrossesAgeBoundary,
    InvalidNativeFrontier,
    NativeExportFailed,
    NativeVerificationFailed,
    NativeImageTooLarge,
    MeasurementMutatedDocument,
}

impl std::fmt::Display for FloorPolicyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidClock => "checkpoint floor policy received an invalid clock reading",
            Self::NoncontiguousAcceptance => {
                "checkpoint floor policy acceptance observations are not contiguous"
            }
            Self::InvalidEncoding => {
                "checkpoint floor policy metadata is not current canonical data"
            }
            Self::InvalidConfiguration => "checkpoint floor policy byte budget is invalid",
            Self::DuplicateCandidate => "checkpoint floor policy repeats a candidate K",
            Self::CandidateCrossesAgeBoundary => {
                "checkpoint floor candidate would remove protected acceptance history"
            }
            Self::InvalidNativeFrontier => {
                "checkpoint floor candidate is not a frontier of the measured document"
            }
            Self::NativeExportFailed => "checkpoint shallow document export failed",
            Self::NativeVerificationFailed => {
                "checkpoint shallow document verification import failed"
            }
            Self::NativeImageTooLarge => "checkpoint shallow document size exceeds u64",
            Self::MeasurementMutatedDocument => {
                "checkpoint floor measurement changed the source document"
            }
        })
    }
}

impl std::error::Error for FloorPolicyError {}

#[derive(Clone, Debug)]
pub(crate) struct ChosenLoroFloor {
    pub(crate) requested_k: u64,
    pub(crate) actual_removed_through: u64,
    pub(crate) checkpoint: Vec<u8>,
    pub(crate) actual_floor: loro::Frontiers,
}

#[derive(Clone, Debug)]
pub(crate) struct RetainedLoroImage {
    pub(crate) checkpoint: Vec<u8>,
    pub(crate) actual_floor: loro::Frontiers,
}

#[derive(Clone, Debug)]
pub(crate) struct LoroFloorWork {
    pub(crate) measurement_exports: u64,
    pub(crate) candidate_exports: u64,
    pub(crate) verification_imports: u64,
    pub(crate) latest_actual_floor: loro::Frontiers,
}

#[derive(Clone, Debug)]
pub(crate) enum LoroFloorDecision {
    Keep {
        retained: RetainedLoroImage,
        metrics: FloorMetrics,
        work: LoroFloorWork,
    },
    Advance {
        chosen: ChosenLoroFloor,
        metrics: FloorMetrics,
        work: LoroFloorWork,
    },
}

struct VerifiedLoroCandidate {
    policy: CandidateMeasurement,
    checkpoint: Vec<u8>,
    actual_floor: loro::Frontiers,
}

/// Perform the worker-side exports and verification imports required by the
/// floor policy. The caller supplies causally closed accepted-prefix frontiers;
/// this function proves the native normalized root removes no operation beyond
/// the corresponding eligible K and never moves behind the installed root.
pub(crate) fn choose_loro_floor(
    config: FloorPolicyConfig,
    eligible_through: u64,
    document: &loro::LoroDoc,
    candidates: impl IntoIterator<Item = (u64, loro::Frontiers)>,
) -> Result<LoroFloorDecision, FloorPolicyError> {
    use loro::ExportMode;

    let before_value = document.get_deep_value();
    let before_vv = document.oplog_vv();
    let current_floor = document.shallow_since_frontiers();
    let current_floor_vv = document
        .frontiers_to_vv(&current_floor)
        .ok_or(FloorPolicyError::InvalidNativeFrontier)?;
    let current_checkpoint = document
        .export(ExportMode::shallow_snapshot(&current_floor))
        .map_err(|_| FloorPolicyError::NativeExportFailed)?;
    let _ = verify_loro_image(document, &current_checkpoint)?;

    let latest_checkpoint = document
        .export(ExportMode::shallow_snapshot(&document.oplog_frontiers()))
        .map_err(|_| FloorPolicyError::NativeExportFailed)?;
    let latest = verify_loro_image(document, &latest_checkpoint)?;
    let mut work = LoroFloorWork {
        measurement_exports: 2,
        candidate_exports: 0,
        verification_imports: 2,
        latest_actual_floor: latest.shallow_since_frontiers(),
    };
    let current_measurement = ImageMeasurement {
        image_bytes: u64::try_from(current_checkpoint.len())
            .map_err(|_| FloorPolicyError::NativeImageTooLarge)?,
        latest_state_bytes: u64::try_from(latest_checkpoint.len())
            .map_err(|_| FloorPolicyError::NativeImageTooLarge)?,
    };
    let budget = config.minimum_tail_bytes.max(
        config
            .live_size_multiplier
            .saturating_mul(current_measurement.latest_state_bytes),
    );
    if current_measurement
        .image_bytes
        .saturating_sub(current_measurement.latest_state_bytes)
        <= budget
    {
        let FloorDecision::Keep { metrics } =
            choose_floor(config, eligible_through, current_measurement, [])?
        else {
            unreachable!("within-budget policy cannot advance")
        };
        return Ok(LoroFloorDecision::Keep {
            retained: RetainedLoroImage {
                checkpoint: current_checkpoint,
                actual_floor: current_floor,
            },
            metrics,
            work,
        });
    }

    let mut requested = candidates.into_iter().collect::<Vec<_>>();
    requested.sort_unstable_by_key(|(sequence, _)| *sequence);
    if requested.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(FloorPolicyError::DuplicateCandidate);
    }
    let mut requested_vv = Vec::with_capacity(requested.len());
    for (sequence, frontiers) in &requested {
        if *sequence > eligible_through {
            return Err(FloorPolicyError::CandidateCrossesAgeBoundary);
        }
        requested_vv.push((
            *sequence,
            document
                .frontiers_to_vv(frontiers)
                .ok_or(FloorPolicyError::InvalidNativeFrontier)?,
        ));
    }
    let current_removed_through = requested_vv
        .iter()
        .filter(|(_, candidate)| current_floor_vv.includes_vv(candidate))
        .map(|(sequence, _)| *sequence)
        .max()
        .unwrap_or(0);

    let mut verified = Vec::new();
    for ((sequence, frontiers), (_, eligible_vv)) in requested.into_iter().zip(requested_vv.iter())
    {
        let checkpoint = document
            .export(ExportMode::shallow_snapshot(&frontiers))
            .map_err(|_| FloorPolicyError::NativeExportFailed)?;
        work.candidate_exports = work.candidate_exports.saturating_add(1);
        let reopened = verify_loro_image(document, &checkpoint)?;
        work.verification_imports = work.verification_imports.saturating_add(1);
        let actual_floor = reopened.shallow_since_frontiers();
        let actual_vv = document
            .frontiers_to_vv(&actual_floor)
            .ok_or(FloorPolicyError::InvalidNativeFrontier)?;
        if !eligible_vv.includes_vv(&actual_vv) {
            return Err(FloorPolicyError::CandidateCrossesAgeBoundary);
        }
        // Loro cannot fork backward from an installed shallow root. Such a
        // normalized candidate is safe to ignore; recovery, not this policy,
        // is the route for obtaining older history.
        if !actual_vv.includes_vv(&current_floor_vv) || actual_vv == current_floor_vv {
            continue;
        }
        let actual_removed_through = requested_vv
            .iter()
            .filter(|(_, candidate)| actual_vv.includes_vv(candidate))
            .map(|(sequence, _)| *sequence)
            .max()
            .unwrap_or(current_removed_through);
        verified.push(VerifiedLoroCandidate {
            policy: CandidateMeasurement {
                requested_k: sequence,
                actual_removed_through,
                image_bytes: u64::try_from(checkpoint.len())
                    .map_err(|_| FloorPolicyError::NativeImageTooLarge)?,
                advances_current: true,
            },
            checkpoint,
            actual_floor,
        });
    }
    if document.get_deep_value() != before_value || document.oplog_vv() != before_vv {
        return Err(FloorPolicyError::MeasurementMutatedDocument);
    }
    match choose_floor(
        config,
        eligible_through,
        current_measurement,
        verified.iter().map(|candidate| candidate.policy),
    )? {
        FloorDecision::Keep { metrics } => Ok(LoroFloorDecision::Keep {
            retained: RetainedLoroImage {
                checkpoint: current_checkpoint,
                actual_floor: current_floor,
            },
            metrics,
            work,
        }),
        FloorDecision::Advance { chosen, metrics } => {
            let candidate = verified
                .into_iter()
                .find(|candidate| candidate.policy.requested_k == chosen.requested_k)
                .ok_or(FloorPolicyError::InvalidNativeFrontier)?;
            Ok(LoroFloorDecision::Advance {
                chosen: ChosenLoroFloor {
                    requested_k: chosen.requested_k,
                    actual_removed_through: chosen.actual_removed_through,
                    checkpoint: candidate.checkpoint,
                    actual_floor: candidate.actual_floor,
                },
                metrics,
                work,
            })
        }
    }
}

fn verify_loro_image(
    source: &loro::LoroDoc,
    checkpoint: &[u8],
) -> Result<loro::LoroDoc, FloorPolicyError> {
    let reopened = loro::LoroDoc::new();
    let status = reopened
        .import(checkpoint)
        .map_err(|_| FloorPolicyError::NativeVerificationFailed)?;
    if status.pending.is_some()
        || reopened.get_deep_value() != source.get_deep_value()
        || reopened.oplog_vv() != source.oplog_vv()
    {
        return Err(FloorPolicyError::NativeVerificationFailed);
    }
    Ok(reopened)
}
