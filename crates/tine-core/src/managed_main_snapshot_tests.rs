//! A current query reads SQLite even when the ordinary save pipeline is ahead.

use super::*;

#[test]
fn live_query_and_registry_read_actual_main_when_required_frontier_is_ahead() {
    let fixture = ActivationFixture::empty("query-main-behind-required", 0x5c5a09);
    fs::create_dir_all(fixture.graph_root.join("notes")).unwrap();
    for (path, body) in [
        ("notes/Plain.md", "- TODO old committed text\n"),
        (
            "notes/Data.md",
            "- TODO row\n  score:: 01\n  lonely:: yes\n",
        ),
        ("notes/score.md", "tine.type:: number\n\n- declaration\n"),
    ] {
        fs::write(fixture.graph_root.join(path), body).unwrap();
    }
    let graph = Graph::open_checked(&fixture.graph_root).unwrap();
    let mut resources =
        activate_clean_runtime_resources(&fixture.request, graph, &mut |_| {}).unwrap();
    let page = oracle_page(&resources, "notes/Plain.md");
    let block = &page.blocks[0];
    let transaction = OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: block.block_id,
            home_document_id: block.home_document_id,
        },
        content: "DONE newer hot editor text".into(),
    }])
    .unwrap();
    let before = resources.runtime.database().frontier_root().unwrap();

    // Existing physical-apply failure cut advances the hot/required root but
    // rolls back SQL. No background actor tick may heal this cut before read.
    crate::oplog::sqlite::fail_next_apply_during_materialization_for_harness();
    let retained = {
        let mut session = resources
            .runtime
            .admit_clean_mutation(&resources.graph)
            .unwrap();
        OperationalCoordinator::execute_clean_local(
            &mut session,
            &resources.graph,
            &resources.receipts,
            &transaction,
            &mut resources.projection_turns,
        )
        .unwrap()
    };
    assert!(matches!(
        retained,
        crate::oplog::operational_coordinator::CleanLocalMutationState::DurablePending(_)
    ));
    drop(retained);
    let hot = resources.runtime.engine().accepted_frontier_root().unwrap();
    let required = resources
        .runtime
        .database()
        .required_frontier_root()
        .clone();
    let actual = resources.runtime.database().frontier_root().unwrap();
    assert_eq!(hot, required);
    assert_eq!(actual, before);
    assert_ne!(actual, required);

    let request = reopen_request(&fixture.request);
    let identities = request.clean_identities.clone().unwrap();
    let shared = Arc::new(crate::managed_query::ManagedQueryShared::default());
    let mut actor = RuntimeActor::from_clean_resources(
        request,
        identities,
        resources,
        SyncRuntimeRecovery::CleanManifestReplay,
        Arc::clone(&shared),
    )
    .unwrap();
    let SimpleQueryTurn::Captured(query) = actor
        .application_simple_query_turn("(task TODO)", 128, 1 << 20)
        .expect("current main queries must not wait for the required editor frontier")
    else {
        panic!("a complete current main image must yield a query capture")
    };
    assert_eq!(
        query.stamp.acceptance_sequence,
        actual.acceptance_sequence()
    );
    assert_ne!(
        query.stamp.acceptance_sequence,
        required.acceptance_sequence()
    );
    let crate::managed_query::ManagedQueryOutcome::Answered(
        crate::managed_query::ManagedQueryAnswer::Blocks(answer),
    ) = shared.execute(&query)
    else {
        panic!("the captured current main query must answer")
    };
    assert!(answer.groups.iter().any(|group| group
        .blocks
        .iter()
        .any(|block| block.raw.contains("TODO old committed text"))));
    assert!(!answer.groups.iter().any(|group| group
        .blocks
        .iter()
        .any(|block| block.raw.contains("newer hot editor text"))));

    let RegistryTurn::Captured(capture) = actor
        .application_captured_registry_turn()
        .expect("current metadata must not wait for the required editor frontier")
    else {
        panic!("a complete current main image must yield a registry capture")
    };
    assert_eq!(
        capture.stamp.acceptance_sequence,
        actual.acceptance_sequence()
    );
    let crate::managed_metadata::ManagedMetadataOutcome::Answered(registry) =
        shared.execute_metadata(&capture)
    else {
        panic!("the captured current main registry must answer")
    };
    let score = registry
        .rows
        .iter()
        .find(|row| row.normalized_name == "score")
        .unwrap();
    assert_eq!(score.observed_type, crate::query::ir::ObservedType::Number);
    assert_eq!(
        score.declared,
        Some((
            crate::query::ir::ObservedType::Number,
            crate::query::ir::Cardinality::One
        ))
    );
    assert!(registry
        .rows
        .iter()
        .any(|row| row.normalized_name == "lonely"));
    assert_eq!(
        actor.active_database().unwrap().frontier_root().unwrap(),
        actual,
        "query execution must not drain or write the pending accepted operation"
    );
}
