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
    let rollback_monotonic = 10 + 60 * day as u64;
    age.observe_acceptance(4, 900, rollback_monotonic).unwrap();
    assert!(age.clock_frozen());
    assert_eq!(age.eligible_through(), before_rollback);
    age.observe_acceptance(5, 1_000, rollback_monotonic + 100)
        .unwrap();
    assert!(!age.clock_frozen(), "a stable pair starts a fresh epoch");
    assert_eq!(age.last_clock_reset_utc_ms(), Some(1_000));
    age.observe_acceptance(
        6,
        1_000 + RETAINED_HISTORY_MS,
        rollback_monotonic + 100 + RETAINED_HISTORY_MS as u64,
    )
    .unwrap();
    assert_eq!(
        age.eligible_through(),
        5,
        "uncertain history waits a fresh full window"
    );

    let encoded = age.encode_current().unwrap();
    let reopened =
        AcceptanceAgePolicy::decode_current(&encoded, 1_000 + RETAINED_HISTORY_MS).unwrap();
    assert_eq!(reopened.eligible_through(), age.eligible_through());
    assert_eq!(
        reopened.latest_acceptance_utc_ms(),
        age.latest_acceptance_utc_ms()
    );
    let recovered = AcceptanceAgePolicy::recover_missing(9, 99_000, 44).unwrap();
    assert_eq!(recovered.eligible_through(), 0);
    assert_eq!(recovered.observed_through(), 9);

    let mut replayed = reopened;
    let replay_t = replayed.latest_acceptance_utc_ms();
    replayed
        .observe_recovered_prefix(8, replay_t + day, rollback_monotonic + 200)
        .unwrap();
    assert_eq!(
        replayed.latest_acceptance_utc_ms(),
        replay_t,
        "replay supplies a conservative age bound without advancing T"
    );
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
    age.observe_acceptance(3, 180 * day, 180 * day as u64)
        .unwrap();
    assert_eq!(age.eligible_through(), 2);

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

#[test]
#[ignore = "release-only retained-tail growth benchmark; run explicitly for P3 qualification"]
fn p3_retained_tail_growth_benchmark() {
    use loro::{ExportMode, LoroDoc, LoroValue};

    fn import_median(checkpoint: &[u8]) -> std::time::Duration {
        let mut samples = Vec::with_capacity(21);
        for _ in 0..21 {
            let started = std::time::Instant::now();
            let reopened = LoroDoc::new();
            assert!(reopened.import(checkpoint).unwrap().pending.is_none());
            samples.push(started.elapsed());
        }
        samples.sort_unstable();
        samples[samples.len() / 2]
    }

    let production = FloorPolicyConfig::default();
    assert_eq!(production.minimum_tail_bytes, 256 * 1024);
    assert_eq!(production.live_size_multiplier, 4);
    let mut document = LoroDoc::new();
    document.set_peer_id(0x5030_0007).unwrap();
    let mut values = document.get_map("values");
    let mut candidates = Vec::new();
    let mut measurements = Vec::new();
    let mut random = 0x9e37_79b9_u32;

    for sequence in 1..=50_000_u64 {
        let value = if matches!(sequence, 10_000 | 50_000) {
            "fixed live text state".to_owned()
        } else {
            (0..256)
                .map(|_| {
                    random ^= random << 13;
                    random ^= random >> 17;
                    random ^= random << 5;
                    char::from(b'!' + (random % 90) as u8)
                })
                .collect()
        };
        values.insert("current", LoroValue::from(value)).unwrap();
        document.commit();
        if sequence % 5_000 == 0 || matches!(sequence, 9_900 | 49_900) {
            candidates.push((sequence, document.oplog_frontiers()));
        }
        if !matches!(sequence, 10_000 | 50_000) {
            continue;
        }

        let eligible_through = sequence - 100;
        let full_checkpoint = document.export(ExportMode::Snapshot).unwrap();
        let decision = choose_loro_floor(
            production,
            eligible_through,
            &document,
            std::mem::take(&mut candidates)
                .into_iter()
                .filter(|(requested_k, _)| *requested_k <= eligible_through),
        )
        .unwrap();
        let LoroFloorDecision::Advance {
            chosen, metrics, ..
        } = decision
        else {
            panic!("production policy did not cut the over-budget {sequence}-edit document")
        };
        assert!(metrics.removable_bytes > metrics.budget_bytes);
        assert!(metrics.post_cut_removable_bytes < metrics.budget_bytes / 2);
        assert!(chosen.checkpoint.len() < full_checkpoint.len());
        let median = import_median(&chosen.checkpoint);
        eprintln!(
            "p3_retained_tail_growth cycles={sequence} pre_r={} post_r={} requested_k={} actual_f={} image_bytes={} import_median_us={}",
            metrics.removable_bytes,
            metrics.post_cut_removable_bytes,
            chosen.requested_k,
            chosen.actual_removed_through,
            chosen.checkpoint.len(),
            median.as_micros(),
        );
        measurements.push((chosen.checkpoint.len(), median));

        let reopened = LoroDoc::new();
        assert!(reopened
            .import(&chosen.checkpoint)
            .unwrap()
            .pending
            .is_none());
        reopened.set_peer_id(0x5030_0007).unwrap();
        document = reopened;
        values = document.get_map("values");
    }

    let [(bytes_10k, import_10k), (bytes_50k, import_50k)] = measurements.as_slice() else {
        panic!("both retained-tail growth measurements must run")
    };
    assert!(
        *bytes_50k as u128 * 100 <= *bytes_10k as u128 * 125,
        "50k post-cut checkpoint bytes exceed 1.25x the 10k checkpoint"
    );
    assert!(
        import_50k.as_nanos() * 100 <= import_10k.as_nanos() * 125,
        "50k post-cut import median exceeds 1.25x the 10k median"
    );
}
