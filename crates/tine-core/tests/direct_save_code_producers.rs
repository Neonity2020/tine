//! I-12: every Direct save failure code has exactly one producer, and every
//! code the contract names has at least one.
//!
//! W4-E4 replaced a prose-matching classifier with a closed
//! `DirectSaveFailureCode` stamped where the failure is constructed. That moved
//! the misclassification risk: it used to be "a page title contains a matched
//! sentence", and it is now "a producer stamps the wrong variant, or a variant
//! nobody produces is named by the contract and the frontend's non-retryable
//! list". The first half is guarded behaviourally in `model_tests.rs`
//! (`direct_save_conflict_sites_produce_their_own_codes` and
//! `direct_save_precheck_helpers_produce_their_own_codes`, which drive the real
//! minting helpers). This file guards the second half, which needs to see the
//! whole shipped tree rather than one module.
//!
//! It uses the shared scanner, not a second walker: "what source does a shipped
//! binary compile" has one answer in this repository
//! (`tests/support/production_source.rs`), and a guard that re-derives it
//! disagrees with it eventually.

#[path = "support/production_source.rs"]
mod production_source;

use production_source::{compiled_source, production_source_files};
use tine_core::model::DirectSaveFailureCode;

#[test]
fn every_direct_save_failure_code_has_a_production_producer() {
    let mut shipped = String::new();
    for path in production_source_files() {
        shipped.push_str(&compiled_source(&path));
        shipped.push('\n');
    }
    assert!(
        shipped.contains("DirectSaveFailureCode::"),
        "the scanner found no Direct save code construction at all, which means \
         it is not reading the production tree — fix the scan before trusting \
         this guard"
    );

    let mut unproduced = Vec::new();
    for code in DirectSaveFailureCode::ALL {
        // `Unknown` is the fallback `direct_save_failure_code` returns for an
        // error that carries no typed code, so it needs no producer of its own.
        if matches!(code, DirectSaveFailureCode::Unknown) {
            continue;
        }
        if !shipped.contains(&format!("DirectSaveFailureCode::{code:?}")) {
            unproduced.push(code.as_str());
        }
    }

    assert!(
        unproduced.is_empty(),
        "I-12: these DirectSaveFailureCode variants are named by \
         docs/contracts/typed-errors.md and by the frontend's save policy, but \
         no shipped source constructs them, so no save can ever emit them: \
         {unproduced:?}.\n\
         Either wire the producer at the site that should raise it, or delete \
         the variant together with its contract row and its entry in \
         src/typedErrorRatchet.test.ts. A code with no producer is a second \
         answer to \"what can go wrong\" that drifts from the first."
    );
}

/// The wire strings are the diagnostic and retry contract, so two variants
/// sharing one string would make a user's retry disposition depend on which
/// producer happened to run.
#[test]
fn direct_save_failure_code_strings_are_distinct() {
    let mut seen = std::collections::BTreeMap::new();
    for code in DirectSaveFailureCode::ALL {
        if let Some(previous) = seen.insert(code.as_str(), code) {
            panic!(
                "I-12: {previous:?} and {code:?} both serialise as {:?}; the \
                 frontend cannot tell them apart and their retry dispositions \
                 would silently merge",
                code.as_str()
            );
        }
    }
    assert_eq!(
        seen.len(),
        DirectSaveFailureCode::ALL.len(),
        "every variant contributes one distinct wire string"
    );
}
