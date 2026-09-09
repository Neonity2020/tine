//! Public Managed property metadata from one current main SQLite snapshot.
//!
//! The actor captures the same lifecycle, stamp, configuration, and immutable
//! registry-cache input as a property result query. Construction runs on the
//! calling thread after the actor operation lock is released and uses the
//! shared Managed snapshot open.

use std::path::PathBuf;
use std::time::Duration;

use crate::config::ParseConfig;
use crate::managed_query::{
    with_managed_read, ManagedQueryCensus, ManagedQueryOutcome, ManagedQueryStamp,
    ManagedReadInput, ManagedRegistryCapture,
};
use crate::query::ir::RegistrySnapshot;
use crate::query_jobs::QueryJobOwner;

#[cfg(test)]
thread_local! {
    static DRAIN_AFTER_CONSTRUCTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn drain_after_next_construction() {
    DRAIN_AFTER_CONSTRUCTION.with(|enabled| enabled.set(true));
}

pub(crate) struct ManagedMetadataCapture {
    pub(crate) job_epoch: crate::query_jobs::QueryJobEpoch,
    pub(crate) path: PathBuf,
    pub(crate) stamp: ManagedQueryStamp,
    pub(crate) config: ParseConfig,
    pub(crate) registry: ManagedRegistryCapture,
}

impl ManagedMetadataCapture {
    fn read_input(&self) -> ManagedReadInput<'_> {
        ManagedReadInput {
            path: &self.path,
            stamp: &self.stamp,
            config: &self.config,
            registry: Some(&self.registry),
        }
    }
}

pub(crate) enum ManagedMetadataOutcome {
    Answered(RegistrySnapshot),
    NotAnswered(ManagedQueryOutcome),
}

pub(crate) fn execute_managed_metadata(
    capture: &ManagedMetadataCapture,
    owner: &QueryJobOwner,
    census: &ManagedQueryCensus,
    wait: Duration,
) -> ManagedMetadataOutcome {
    let outcome = with_managed_read(
        capture.job_epoch,
        &capture.read_input(),
        owner,
        wait,
        |opened| {
            let snapshot = opened.registry.snapshot();
            #[cfg(test)]
            if DRAIN_AFTER_CONSTRUCTION.with(|enabled| enabled.replace(false)) {
                owner.begin_drain();
            }
            Ok(snapshot)
        },
    );
    match outcome {
        Ok(snapshot) => {
            census.note_metadata_read();
            ManagedMetadataOutcome::Answered(snapshot)
        }
        Err(outcome) => ManagedMetadataOutcome::NotAnswered(outcome),
    }
}
