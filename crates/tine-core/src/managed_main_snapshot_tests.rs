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
    assert!(matches!(
        actor.clean.as_mut().unwrap().retain_outcome(retained),
        CleanActorMutationOutcome::DurablePending { .. }
    ));
    let pending_before = actor.clean.as_ref().unwrap().pending_failure();
    assert!(pending_before.is_some());
    let assert_read_only = |actor: &RuntimeActor, stage: &str| {
        assert_eq!(
            actor.active_database().unwrap().frontier_root().unwrap(),
            actual,
            "{stage} must not advance the main projection"
        );
        assert_eq!(
            actor.clean.as_ref().unwrap().pending_failure(),
            pending_before,
            "{stage} must leave the ordinary save continuation untouched"
        );
    };
    let SimpleQueryTurn::Captured(query) = actor
        .application_simple_query_turn("(task TODO)", 128, 1 << 20)
        .expect("current main queries must not wait for the required editor frontier")
    else {
        panic!("a complete current main image must yield a query capture")
    };
    assert_read_only(&actor, "simple capture");
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
    assert_read_only(&actor, "simple execution");

    let IrQueryTurn::Captured(ir) = actor
        .application_captured_query_turn(
            &ManagedQueryTurnInput::Ir {
                query: query.query.clone(),
                view: query.view.clone(),
                context: crate::query::ir::ExecutionContext::none(),
                explain: false,
            },
            128,
            1 << 20,
        )
        .expect("IR capture must read current main without settling a save")
    else {
        panic!("a complete current main image must yield an IR capture")
    };
    assert_read_only(&actor, "IR capture");
    let crate::managed_query::ManagedQueryOutcome::Answered(
        crate::managed_query::ManagedQueryAnswer::Blocks(ir_answer),
    ) = shared.execute(&ir)
    else {
        panic!("the current main IR query must answer")
    };
    assert_eq!(
        serde_json::to_value(&ir_answer.groups).unwrap(),
        serde_json::to_value(&answer.groups).unwrap()
    );
    assert_read_only(&actor, "IR execution");

    let RegistryTurn::Captured(capture) = actor
        .application_captured_registry_turn()
        .expect("current metadata must not wait for the required editor frontier")
    else {
        panic!("a complete current main image must yield a registry capture")
    };
    assert_read_only(&actor, "registry capture");
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
    assert_read_only(&actor, "registry execution");

    let specs = [crate::query::QueryExportSpec {
        key: "committed".into(),
        query: "(task TODO)".into(),
        advanced: false,
        simple_dialect: None,
        current_page: None,
    }];
    let prepared =
        crate::query::export_execute::PreparedExportBatch::prepare(&specs, 8, query.today);
    let export_capture = actor
        .capture_current_query_read(prepared.requires_registry())
        .expect("export capture must not settle retained publication");
    assert_read_only(&actor, "export capture");
    let exported = shared
        .execute_export(&export_capture, &prepared, 128, 1024, 1 << 20)
        .expect("export must read the current main image without settling retained publication");
    assert_eq!(
        serde_json::to_value(&exported.results[0].groups).unwrap(),
        serde_json::to_value(&answer.groups).unwrap()
    );
    assert_read_only(&actor, "export execution");
    assert_eq!(
        actor.active_database().unwrap().frontier_root().unwrap(),
        actual,
        "query execution must not drain or write the pending accepted operation"
    );
    shared.jobs.begin_drain();
    assert!(
        matches!(
            shared.execute_export(&export_capture, &prepared, 128, 1024, 1 << 20),
            Err(crate::managed_query::ManagedQueryOutcome::Cancelled)
        ),
        "a retired projection cannot execute a captured export"
    );
}
