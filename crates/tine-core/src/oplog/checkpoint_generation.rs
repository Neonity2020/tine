//! Adapter between Tine's one current accepted-evidence format and the shared
//! sealed accepted-history index.
//!
//! R1a is deliberately reader-only at the production boundary. The generation
//! builder and durable marker authoring arrive in later independently
//! releasable cuts. Managed Storage is pre-0.7, so this module contains no
//! legacy decoder, version dispatch, or migration bridge.

use super::hot_engine::{AcceptedBatchEvidence, ACCEPTED_EVIDENCE_SCHEMA_VERSION};

pub(crate) struct TineAcceptedEvidenceDecoder;

impl tine_storage::sealed_accepted_index::SealedAcceptedEvidenceDecoder
    for TineAcceptedEvidenceDecoder
{
    fn decode_accepted_evidence(
        &self,
        evidence_schema: u32,
        exact_evidence_bytes: &[u8],
    ) -> Result<
        tine_storage::sealed_accepted_index::AcceptedEvidenceBindingV2,
        tine_storage::sealed_accepted_index::SealedAcceptedIndexError,
    > {
        use tine_storage::sealed_accepted_index::SealedAcceptedIndexError;

        if evidence_schema != ACCEPTED_EVIDENCE_SCHEMA_VERSION {
            return Err(SealedAcceptedIndexError::Corrupt(format!(
                "accepted-status evidence schema {evidence_schema} != current schema {ACCEPTED_EVIDENCE_SCHEMA_VERSION}"
            )));
        }
        let evidence = AcceptedBatchEvidence::decode_canonical(exact_evidence_bytes)
            .map_err(|error| SealedAcceptedIndexError::Corrupt(error.to_string()))?;
        Ok(
            tine_storage::sealed_accepted_index::AcceptedEvidenceBindingV2 {
                batch_id: evidence.batch_id().as_uuid().into_bytes(),
                manifest_fingerprint: evidence.manifest_fingerprint(),
                event_binding_digest: evidence.event_binding_digest(),
                acceptance_sequence: evidence.acceptance_sequence(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::hot_engine::{
        accepted_causal_record_digest, authenticated_causal_clock_root, AcceptedFrontierRoot,
    };
    use crate::oplog::{BatchCausalDot, BatchId, CausalPeerId, ContentDigest, DeviceId};
    use tine_storage::sealed_accepted_index::{
        AcceptedSequenceEntryV2, AcceptedSequenceRootV2, AcceptedStatusRecordV2,
        AuthenticatedMapRootV1, SealedAcceptedCausalClockEntryV2, SealedAcceptedCausalRecordV2,
        SealedAcceptedEvidenceDecoder, SealedAcceptedIndexObjectStore, SealedAcceptedIndexReader,
        SealedAcceptedIndexRootsV2, SealedAcceptedIndexWriter, SealedAcceptedObjectKind,
    };

    #[derive(Default)]
    struct SealedMemoryStore {
        objects: Vec<(SealedAcceptedObjectKind, ContentDigest, Vec<u8>)>,
    }

    impl SealedAcceptedIndexObjectStore for SealedMemoryStore {
        fn read_sealed_accepted_object(
            &self,
            kind: SealedAcceptedObjectKind,
            address: ContentDigest,
        ) -> Result<Option<Vec<u8>>, tine_storage::sealed_accepted_index::SealedAcceptedIndexError>
        {
            Ok(self
                .objects
                .iter()
                .find(|(stored_kind, stored_address, _)| {
                    *stored_kind == kind && *stored_address == address
                })
                .map(|(_, _, bytes)| bytes.clone()))
        }

        fn publish_sealed_accepted_object(
            &mut self,
            kind: SealedAcceptedObjectKind,
            address: ContentDigest,
            bytes: &[u8],
        ) -> Result<(), tine_storage::sealed_accepted_index::SealedAcceptedIndexError> {
            if let Some((_, _, existing)) =
                self.objects
                    .iter()
                    .find(|(stored_kind, stored_address, _)| {
                        *stored_kind == kind && *stored_address == address
                    })
            {
                if existing != bytes {
                    return Err(
                        tine_storage::sealed_accepted_index::SealedAcceptedIndexError::Corrupt(
                            "same address has different test bytes".into(),
                        ),
                    );
                }
                return Ok(());
            }
            self.objects.push((kind, address, bytes.to_vec()));
            Ok(())
        }
    }

    fn digest(byte: u8) -> ContentDigest {
        ContentDigest::from_bytes([byte; 32])
    }

    fn evidence() -> AcceptedBatchEvidence {
        let batch_id = BatchId::from_uuid(uuid::Uuid::from_bytes([0x51; 16]));
        AcceptedBatchEvidence::for_test(
            batch_id,
            digest(0x61),
            digest(0x71),
            AcceptedFrontierRoot::empty(),
            Vec::new(),
            Vec::new(),
            vec![(batch_id, digest(0x81))],
            0,
        )
    }

    #[test]
    fn decoder_accepts_only_the_one_current_evidence_schema() {
        let evidence = evidence();
        let bytes = evidence.encode_canonical().unwrap();
        let binding = TineAcceptedEvidenceDecoder
            .decode_accepted_evidence(ACCEPTED_EVIDENCE_SCHEMA_VERSION, &bytes)
            .unwrap();
        assert_eq!(binding.batch_id, [0x51; 16]);
        assert_eq!(binding.manifest_fingerprint, digest(0x61));
        assert_eq!(binding.event_binding_digest, digest(0x71));
        assert_eq!(binding.acceptance_sequence, 1);
        assert!(TineAcceptedEvidenceDecoder
            .decode_accepted_evidence(ACCEPTED_EVIDENCE_SCHEMA_VERSION - 1, &bytes)
            .is_err());

        let mut trailing = bytes;
        trailing.push(0);
        assert!(TineAcceptedEvidenceDecoder
            .decode_accepted_evidence(ACCEPTED_EVIDENCE_SCHEMA_VERSION, &trailing)
            .is_err());
    }

    #[test]
    fn tine_and_storage_share_the_exact_causal_record_address() {
        let low =
            CausalPeerId::from_device_id(DeviceId::from_uuid(uuid::Uuid::from_bytes([0x11; 16])));
        let author =
            CausalPeerId::from_device_id(DeviceId::from_uuid(uuid::Uuid::from_bytes([0x44; 16])));
        let (root_key, root_digest) =
            authenticated_causal_clock_root(&[(low, 3), (author, 7)]).unwrap();
        let engine_address = accepted_causal_record_digest(
            BatchId::from_uuid(uuid::Uuid::from_bytes([0x51; 16])),
            digest(0x22),
            digest(0x33),
            BatchCausalDot::new(author, 7).unwrap(),
            root_key,
            root_digest,
        );
        let storage_record = SealedAcceptedCausalRecordV2 {
            batch_id: [0x51; 16],
            manifest_fingerprint: digest(0x22),
            event_binding_digest: digest(0x33),
            causal_peer_id: [0x44; 16],
            causal_counter: 7,
            canonical_causal_clock: vec![
                SealedAcceptedCausalClockEntryV2 {
                    peer_id: [0x11; 16],
                    counter: 3,
                },
                SealedAcceptedCausalClockEntryV2 {
                    peer_id: [0x44; 16],
                    counter: 7,
                },
            ],
        };
        assert_eq!(storage_record.address().unwrap(), engine_address);
    }

    #[test]
    fn tine_decoder_completes_the_shared_membership_proof() {
        let evidence = evidence();
        let causal = SealedAcceptedCausalRecordV2 {
            batch_id: [0x51; 16],
            manifest_fingerprint: digest(0x61),
            event_binding_digest: digest(0x71),
            causal_peer_id: [0x44; 16],
            causal_counter: 7,
            canonical_causal_clock: vec![SealedAcceptedCausalClockEntryV2 {
                peer_id: [0x44; 16],
                counter: 7,
            }],
        };
        let mut store = SealedMemoryStore::default();
        let (batch_map, status_map, sequence);
        {
            let mut writer = SealedAcceptedIndexWriter::new(&mut store);
            let causal_address = writer.publish_causal(&causal).unwrap();
            let status = AcceptedStatusRecordV2 {
                batch_id: [0x51; 16],
                no_op: false,
                evidence_schema: ACCEPTED_EVIDENCE_SCHEMA_VERSION,
                exact_evidence_bytes: evidence.encode_canonical().unwrap(),
                accepted_causal_record_digest: causal_address,
            };
            let status_address = writer.publish_status(&status).unwrap();
            batch_map = writer
                .upsert_map(AuthenticatedMapRootV1::empty(), [0x51; 16], causal_address)
                .unwrap();
            status_map = writer
                .upsert_map(AuthenticatedMapRootV1::empty(), [0x51; 16], status_address)
                .unwrap();
            sequence = writer
                .append_sequence(
                    AcceptedSequenceRootV2::empty(),
                    AcceptedSequenceEntryV2 {
                        sequence: 1,
                        batch_id: [0x51; 16],
                        accepted_status_value_digest: status_address,
                    },
                )
                .unwrap();
        }
        let proof = SealedAcceptedIndexReader::new(&store)
            .prove_membership(
                SealedAcceptedIndexRootsV2 {
                    batch_map,
                    status_map,
                    sequence,
                },
                1,
                [0x51; 16],
                &TineAcceptedEvidenceDecoder,
            )
            .unwrap()
            .unwrap();
        assert_eq!(proof.sequence.sequence, 1);
        assert_eq!(
            proof.status.exact_evidence_bytes,
            evidence.encode_canonical().unwrap()
        );
        assert_eq!(proof.causal, causal);
    }

    #[test]
    fn r1a_adapter_cannot_author_checkpoint_markers() {
        let production = include_str!("checkpoint_generation.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        for forbidden in [
            "std::fs",
            "cap_std",
            "DurableDirectoryPublication",
            "publish_immutable",
            "write_all",
        ] {
            assert!(
                !production.contains(forbidden),
                "R1a adapter gained a production authoring primitive: {forbidden}"
            );
        }

        fn visit(directory: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(&path, files);
                } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                    files.push(path);
                }
            }
        }

        let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        visit(&source_root, &mut files);
        for forbidden in [
            "SealedAcceptedIndexWriter",
            "initialize_checkpoint_candidate_schema",
        ] {
            let production_callers = files
                .iter()
                .filter(|path| {
                    std::fs::read_to_string(path)
                        .unwrap()
                        .split("#[cfg(test)]\nmod tests")
                        .next()
                        .unwrap()
                        .contains(forbidden)
                })
                .collect::<Vec<_>>();
            assert!(
                production_callers.is_empty(),
                "R1a gained a production checkpoint authoring caller for {forbidden}: {production_callers:?}"
            );
        }
    }
}
