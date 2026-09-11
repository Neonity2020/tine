use super::checkpoint_floor_policy::{
    choose_floor, choose_loro_floor, AcceptanceAgePolicy, CandidateMeasurement, FloorDecision,
    FloorPolicyConfig, ImageMeasurement, LimitingCause, LoroFloorDecision, RETAINED_HISTORY_MS,
};

fn config() -> FloorPolicyConfig {
    FloorPolicyConfig {
        revision: 1,
        minimum_tail_bytes: 256,
        live_size_multiplier: 4,
    }
}

#[test]
fn p3_floor_policy_core_clock_and_hysteresis() {
    let day = 24 * 60 * 60 * 1_000_i64;
    let mut age = AcceptanceAgePolicy::fresh(1_000, 10).unwrap();
    assert_eq!(age.eligible_through(), 0);

    age.observe_acceptance(1, 1_000 + 29 * day, 10 + 29 * day as u64)
        .unwrap();
    assert_eq!(
        age.eligible_through(),
        0,
        "just below 30 days keeps genesis protected"
    );
    age.observe_acceptance(2, 1_000 + 30 * day, 10 + 30 * day as u64)
        .unwrap();
    assert_eq!(
        age.eligible_through(),
        0,
        "genesis is not an accepted batch cut"
    );
    age.observe_acceptance(3, 1_000 + 59 * day, 10 + 59 * day as u64)
        .unwrap();
    assert_eq!(age.eligible_through(), 1);
    age.observe_acceptance(3, 1_000 + 100 * day, 10 + 100 * day as u64)
        .unwrap();
    assert_eq!(
        age.eligible_through(),
        1,
        "duplicate delivery does not advance time"
    );

    let within_budget = choose_floor(
        config(),
        age.eligible_through(),
        ImageMeasurement {
            image_bytes: 300,
            latest_state_bytes: 100,
        },
        [],
    )
    .unwrap();
    assert!(matches!(within_budget, FloorDecision::Keep { .. }));

    let cut = choose_floor(
        config(),
        3,
        ImageMeasurement {
            image_bytes: 1_000,
            latest_state_bytes: 100,
        },
        [
            CandidateMeasurement {
                requested_k: 1,
                actual_removed_through: 1,
                image_bytes: 500,
                advances_current: true,
            },
            CandidateMeasurement {
                requested_k: 2,
                actual_removed_through: 2,
                image_bytes: 250,
                advances_current: true,
            },
            CandidateMeasurement {
                requested_k: 3,
                actual_removed_through: 3,
                image_bytes: 180,
                advances_current: true,
            },
        ],
    )
    .unwrap();
    let FloorDecision::Advance { chosen, metrics } = cut else {
        panic!("size pressure must cut")
    };
    assert_eq!(
        chosen.requested_k, 2,
        "oldest candidate below half-budget wins"
    );
    assert_eq!(metrics.post_cut_removable_bytes, 150);

    let protected_overage = choose_floor(
        config(),
        1,
        ImageMeasurement {
            image_bytes: 1_000,
            latest_state_bytes: 100,
        },
        [CandidateMeasurement {
            requested_k: 1,
            actual_removed_through: 1,
            image_bytes: 900,
            advances_current: true,
        }],
    )
    .unwrap();
    let FloorDecision::Advance { metrics, .. } = protected_overage else {
        panic!("greatest safe cut is retained")
    };
    assert_eq!(metrics.limiting_cause, Some(LimitingCause::AgeLowerBound));
    assert!(metrics.budget_overage_bytes > 0);

    let before_rollback = age.eligible_through();
    age.observe_acceptance(4, 900, 10 + 60 * day as u64)
        .unwrap();
    assert!(age.clock_frozen());
    assert_eq!(age.eligible_through(), before_rollback);
    age.reestablish_clock(2_000, 20).unwrap();
    age.observe_acceptance(
        5,
        2_000 + RETAINED_HISTORY_MS,
        20 + RETAINED_HISTORY_MS as u64,
    )
    .unwrap();
    assert_eq!(
        age.eligible_through(),
        4,
        "uncertain history waits a fresh full window"
    );

    let encoded = age.encode_current().unwrap();
    let reopened =
        AcceptanceAgePolicy::decode_current(&encoded, 2_000 + RETAINED_HISTORY_MS).unwrap();
    assert_eq!(reopened.eligible_through(), age.eligible_through());
    assert_eq!(
        reopened.latest_acceptance_utc_ms(),
        age.latest_acceptance_utc_ms()
    );
    let recovered = AcceptanceAgePolicy::recover_missing(9, 99_000, 44).unwrap();
    assert_eq!(recovered.eligible_through(), 0);
    assert_eq!(recovered.observed_through(), 9);
}

#[test]
fn p3_floor_policy_core_native_normalization() {
    let decision = choose_floor(
        config(),
        4,
        ImageMeasurement {
            image_bytes: 1_200,
            latest_state_bytes: 100,
        },
        [
            CandidateMeasurement {
                requested_k: 1,
                actual_removed_through: 0,
                image_bytes: 1_150,
                advances_current: false,
            },
            CandidateMeasurement {
                requested_k: 2,
                actual_removed_through: 1,
                image_bytes: 700,
                advances_current: true,
            },
            CandidateMeasurement {
                requested_k: 3,
                actual_removed_through: 1,
                image_bytes: 760,
                advances_current: true,
            },
            CandidateMeasurement {
                requested_k: 4,
                actual_removed_through: 2,
                image_bytes: 650,
                advances_current: true,
            },
        ],
    )
    .unwrap();
    let FloorDecision::Advance { chosen, metrics } = decision else {
        panic!("a safe older floor exists")
    };
    assert_eq!(chosen.requested_k, 4);
    assert_eq!(
        chosen.actual_removed_through, 2,
        "native normalization may retain an older floor"
    );
    assert_eq!(
        metrics.limiting_cause,
        Some(LimitingCause::NativeNormalization)
    );
    assert!(metrics.hysteresis_shortfall_bytes > 0);
}

#[test]
fn p3_floor_policy_core_age_never_cuts_without_pressure() {
    let day = 24 * 60 * 60 * 1_000_i64;
    let mut age = AcceptanceAgePolicy::fresh(0, 0).unwrap();
    age.observe_acceptance(1, day, day as u64).unwrap();
    age.observe_acceptance(2, 29 * day, 29 * day as u64)
        .unwrap();
    assert_eq!(age.eligible_through(), 0);
    age.observe_acceptance(3, 31 * day, 31 * day as u64)
        .unwrap();
    assert_eq!(age.eligible_through(), 1);

    let slow = choose_floor(
        config(),
        age.eligible_through(),
        ImageMeasurement {
            image_bytes: 350,
            latest_state_bytes: 100,
        },
        [],
    )
    .unwrap();
    assert!(
        matches!(slow, FloorDecision::Keep { .. }),
        "age alone never cuts"
    );

    let busy = choose_floor(
        config(),
        age.eligible_through(),
        ImageMeasurement {
            image_bytes: 1_500,
            latest_state_bytes: 100,
        },
        [CandidateMeasurement {
            requested_k: 1,
            actual_removed_through: 1,
            image_bytes: 200,
            advances_current: true,
        }],
    )
    .unwrap();
    assert!(
        matches!(busy, FloorDecision::Advance { chosen, .. } if chosen.actual_removed_through == 1)
    );
}

#[test]
fn p3_floor_policy_core_real_loro_candidate() {
    use loro::LoroValue;

    let document = loro::LoroDoc::new();
    let values = document.get_map("values");
    let mut candidates = Vec::new();
    for sequence in 1..=160_u64 {
        values
            .insert(
                "current",
                LoroValue::from(format!("{sequence:04}-{}", "x".repeat(256))),
            )
            .unwrap();
        document.commit();
        if sequence % 20 == 0 {
            candidates.push((sequence, document.oplog_frontiers()));
        }
    }
    let before_value = document.get_deep_value();
    let before_vv = document.oplog_vv();
    let outcome = choose_loro_floor(
        FloorPolicyConfig {
            revision: 7,
            minimum_tail_bytes: 1,
            live_size_multiplier: 1,
        },
        160,
        &document,
        candidates,
    )
    .unwrap();
    let LoroFloorDecision::Advance {
        chosen,
        metrics,
        work,
    } = outcome
    else {
        panic!("real accumulated Loro history must cross the tiny test budget")
    };
    assert!(chosen.requested_k <= 160);
    assert!(chosen.actual_removed_through <= chosen.requested_k);
    assert_eq!(
        document.get_deep_value(),
        before_value,
        "measurement authors no state"
    );
    assert_eq!(
        document.oplog_vv(),
        before_vv,
        "measurement authors no operations"
    );
    let reopened = loro::LoroDoc::new();
    let status = reopened.import(&chosen.checkpoint).unwrap();
    assert!(status.pending.is_none());
    assert_eq!(reopened.get_deep_value(), before_value);
    assert_eq!(reopened.oplog_vv(), before_vv);
    assert_eq!(reopened.shallow_since_frontiers(), chosen.actual_floor);
    assert!(metrics.latest_state_bytes > 0);
    assert!(document
        .frontiers_to_vv(&work.latest_actual_floor)
        .is_some());
    assert_eq!(work.measurement_exports, 2);
    assert_eq!(work.candidate_exports, 8);
    assert_eq!(work.verification_imports, 10);
}
