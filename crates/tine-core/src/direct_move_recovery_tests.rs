//! The B2 acceptance gate: every crash cut of a Direct cross-page move
//! converges, and neither terminal state changes a byte the move did not own.
//!
//! **What "the crash matrix" means here.** A Direct cross-page move's durable
//! steps are exactly the ones `direct_move_durable_steps` names — commit the
//! record, write the destination, write each source, retire the record — and the
//! only thing a crash can do is stop the sequence after some step k. So instead
//! of killing a process and hoping to hit an interesting moment, these tests
//! EXECUTE the production functions in the production order, stop after each k
//! in turn, and run the real `recover_all` on the resulting disk. Every reachable
//! disk state is covered by construction rather than by sampling. The frontend's
//! side of the same claim — that `src/store.ts` really emits that order — is
//! pinned by `src/directMoveOrder.test.ts`.

use super::*;
use crate::direct_move_recovery::{
    direct_move_durable_steps, recover_all, retire_if_terminal, DirectMoveRecord, DurableStep,
    ImageRef, MoveParticipant, ParticipantRole, RecordOutcome, RecoveryStore, RECORD_SCHEMA,
};
use crate::model::{content_rev, Graph, PageDto, PageKind};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const CONTRACT: &str = include_str!("../../../docs/contracts/direct-move-recovery.md");

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tine-b2-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    dir
}

struct Fixture {
    graph_root: PathBuf,
    store_root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Fixture {
        let graph_root = scratch(tag);
        let store_root = graph_root
            .parent()
            .unwrap()
            .join(format!("tine-b2-store-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&store_root);
        fs::create_dir_all(&store_root).unwrap();
        Fixture {
            graph_root,
            store_root,
        }
    }

    fn store(&self) -> RecoveryStore {
        RecoveryStore::new(&self.store_root)
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.graph_root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn read(&self, relative: &str) -> Option<String> {
        fs::read_to_string(self.graph_root.join(relative)).ok()
    }

    fn snapshot(&self) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        collect_files(&self.graph_root, &self.graph_root, &mut files);
        files
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.graph_root);
        let _ = fs::remove_dir_all(&self.store_root);
    }
}

fn collect_files(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out);
        } else if let Ok(bytes) = fs::read(&path) {
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(relative, bytes);
        }
    }
}

/// One block moved out of every source page and appended to the destination —
/// the shape every one of the five B1 move call sites and carry produce.
fn load_any(graph: &Graph, name: &str) -> PageDto {
    graph
        .load_named(name, PageKind::Journal)
        .ok()
        .flatten()
        .or_else(|| graph.load_named(name, PageKind::Page).ok().flatten())
        .unwrap_or_else(|| panic!("page loads: {name}"))
}

fn plan_move(graph: &Graph, destination: &str, sources: &[&str]) -> (PageDto, Vec<PageDto>) {
    let mut destination_dto = load_any(graph, destination);
    let mut source_dtos = Vec::new();
    for source in sources {
        let mut dto = load_any(graph, source);
        let moved = dto.blocks.remove(0);
        destination_dto.blocks.push(moved);
        source_dtos.push(dto);
    }
    (destination_dto, source_dtos)
}

/// Execute the production durable steps in production order, stopping after
/// `cut` of them. `cut == 0` is "crashed before anything was durable".
fn run_to_cut(
    fixture: &Fixture,
    graph: &Graph,
    destination: &PageDto,
    sources: &[PageDto],
    cut: usize,
) -> Option<DirectMoveRecord> {
    let prepared = graph
        .prepare_direct_cross_page_move(destination, sources)
        .expect("record composes")
        .expect("a cross-page move needs a record");
    let steps = direct_move_durable_steps(&prepared.record);
    let store = fixture.store();
    for step in steps.into_iter().take(cut) {
        match step {
            DurableStep::CommitRecord => store
                .commit(&prepared.record, &prepared.images)
                .expect("record commits"),
            DurableStep::WriteDestination => {
                graph
                    .save_page(destination, destination.rev.as_deref())
                    .expect("destination saves");
            }
            DurableStep::WriteSource(index) => {
                let source = &sources[index];
                graph
                    .save_page(source, source.rev.as_deref())
                    .expect("source saves");
            }
            DurableStep::RetireRecord => {
                assert!(retire_if_terminal(
                    &fixture.store_root,
                    &fixture.graph_root,
                    &prepared.record.move_id
                ));
            }
        }
    }
    Some(prepared.record)
}

/// The bytes each participant ends with when the move runs to completion with no
/// crash at all — the oracle every recovered state is compared against.
fn uncrashed_terminal_bytes(tag: &str, build: impl Fn(&Fixture)) -> BTreeMap<String, Vec<u8>> {
    let fixture = Fixture::new(&format!("{tag}-oracle"));
    build(&fixture);
    let graph = Graph::open(&fixture.graph_root);
    let (destination, sources) = plan_move(&graph, "Today", &["Older", "Oldest"]);
    graph
        .save_page(&destination, destination.rev.as_deref())
        .unwrap();
    for source in &sources {
        graph.save_page(source, source.rev.as_deref()).unwrap();
    }
    fixture.snapshot()
}

fn two_source_graph(fixture: &Fixture) {
    fixture.write("pages/Today.md", "- today keeps this\n");
    fixture.write("pages/Older.md", "- moves out of older\n- older keeps this\n");
    fixture.write("pages/Oldest.md", "- moves out of oldest\n- oldest keeps this\n");
}

// ---------------------------------------------------------------------------
// The crash matrix
// ---------------------------------------------------------------------------

/// Cut between EVERY pair of durable steps of a 1 + 2 move, and prove the next
/// open converges from each one.
///
/// Convergence here is the strong form: after recovery the graph is byte-equal
/// EITHER to the pre-move graph (rolled back) OR to the graph an uncrashed move
/// produces (completed). "Some sensible state" is not the contract.
#[test]
fn every_crash_cut_of_a_two_source_move_converges_on_reopen() {
    let before = {
        let fixture = Fixture::new("before-oracle");
        two_source_graph(&fixture);
        fixture.snapshot()
    };
    let after = uncrashed_terminal_bytes("two-source", two_source_graph);
    assert_ne!(before, after, "the fixture move must actually change bytes");

    // commit, destination, source 0, source 1, retire
    let total_steps = 5;
    for cut in 0..=total_steps {
        let fixture = Fixture::new(&format!("cut-{cut}"));
        two_source_graph(&fixture);
        let record = {
            let graph = Graph::open(&fixture.graph_root);
            let (destination, sources) = plan_move(&graph, "Today", &["Older", "Oldest"]);
            run_to_cut(&fixture, &graph, &destination, &sources, cut)
            // the Graph is dropped here: recovery runs with no open graph, which
            // is the situation it is actually invoked in.
        };
        let record = record.unwrap();

        // FAIL-BEFORE, kept as a standing assertion rather than a one-off run:
        // at the interesting cuts the graph really IS divergent before recovery
        // touches it — the blocks are in the destination AND still in a source.
        // That divergent state is exactly what the pre-B2 tree left behind
        // permanently, and it is what makes this test necessary. If a future
        // change makes the mid-crash state already terminal, this assertion
        // fails and tells you the matrix has stopped testing anything.
        let mid_crash = fixture.snapshot();
        if (2..=3).contains(&cut) {
            assert_ne!(mid_crash, before, "cut {cut}: expected a half-move, not the pre-move graph");
            assert_ne!(mid_crash, after, "cut {cut}: expected a half-move, not a finished move");
        }

        let report = recover_all(&fixture.store_root, &fixture.graph_root);
        let observed = fixture.snapshot();

        let expected_outcome = match cut {
            0 => None, // nothing durable: recovery has no record to see
            1 => Some(RecordOutcome::NothingApplied),
            2 => Some(RecordOutcome::Completed { pages_written: 2 }),
            3 => Some(RecordOutcome::Completed { pages_written: 1 }),
            4 => Some(RecordOutcome::AlreadyComplete),
            _ => None, // retired by the successful move itself
        };
        match expected_outcome {
            None => assert!(
                report.is_empty(),
                "cut {cut}: expected no pending record, got {:?}",
                report.outcomes
            ),
            Some(expected) => assert_eq!(
                report.outcomes,
                vec![(record.move_id.clone(), expected)],
                "cut {cut}"
            ),
        }

        let expected_bytes = if cut <= 1 { &before } else { &after };
        assert_eq!(
            observed, *expected_bytes,
            "cut {cut}: the graph did not converge to a terminal state"
        );
        // The record is gone either way: nothing is left to re-apply later.
        assert!(
            recover_all(&fixture.store_root, &fixture.graph_root).is_empty(),
            "cut {cut}: a record survived recovery"
        );
    }
}

/// The same matrix for the 1 + 1 shape (`moveBlock`, `moveBlockFeedNow`,
/// `moveSelectionItems`, and a one-day carry all produce it).
#[test]
fn every_crash_cut_of_a_single_source_move_converges_on_reopen() {
    fn build(fixture: &Fixture) {
        fixture.write("journals/2026_09_01.md", "- today keeps this\n");
        fixture.write("journals/2026_08_31.md", "- TODO carry me\n- stays put\n");
    }
    let before = {
        let fixture = Fixture::new("single-before");
        build(&fixture);
        fixture.snapshot()
    };
    let after = {
        let fixture = Fixture::new("single-oracle");
        build(&fixture);
        let graph = Graph::open(&fixture.graph_root);
        let (destination, sources) = plan_move(&graph, "Sep 1st, 2026", &["Aug 31st, 2026"]);
        graph
            .save_page(&destination, destination.rev.as_deref())
            .unwrap();
        graph
            .save_page(&sources[0], sources[0].rev.as_deref())
            .unwrap();
        fixture.snapshot()
    };
    assert_ne!(before, after);

    for cut in 0..=4 {
        let fixture = Fixture::new(&format!("single-cut-{cut}"));
        build(&fixture);
        {
            let graph = Graph::open(&fixture.graph_root);
            let (destination, sources) = plan_move(&graph, "Sep 1st, 2026", &["Aug 31st, 2026"]);
            run_to_cut(&fixture, &graph, &destination, &sources, cut);
        }
        recover_all(&fixture.store_root, &fixture.graph_root);
        let expected = if cut <= 1 { &before } else { &after };
        assert_eq!(fixture.snapshot(), *expected, "cut {cut}");
    }
}

/// The rollback direction. Under the production ordering the destination is
/// always written first, so the state this exercises — a source removal durable
/// while the destination addition is not — is the one the contract exists to
/// undo. It is constructed directly rather than waited for.
#[test]
fn a_removal_without_its_addition_is_rolled_back() {
    let fixture = Fixture::new("rollback");
    two_source_graph(&fixture);
    let before = fixture.snapshot();
    let record = {
        let graph = Graph::open(&fixture.graph_root);
        let (destination, sources) = plan_move(&graph, "Today", &["Older", "Oldest"]);
        let prepared = graph
            .prepare_direct_cross_page_move(&destination, &sources)
            .unwrap()
            .unwrap();
        fixture
            .store()
            .commit(&prepared.record, &prepared.images)
            .unwrap();
        // Only the removal lands. The destination never gains the block.
        graph
            .save_page(&sources[0], sources[0].rev.as_deref())
            .unwrap();
        prepared.record
    };
    assert_ne!(fixture.snapshot(), before, "the removal really landed");

    let report = recover_all(&fixture.store_root, &fixture.graph_root);
    assert_eq!(
        report.outcomes,
        vec![(record.move_id, RecordOutcome::RolledBack { pages_written: 1 })]
    );
    assert_eq!(
        fixture.snapshot(),
        before,
        "a removal with no addition must be undone, never carried forward"
    );
}

/// Markdown/Org stays authoritative. If any participant's bytes match neither
/// recorded image, recovery must not complete OR roll back — both versions are
/// preserved and the ordinary conflict machinery owns the decision.
#[test]
fn an_external_write_to_any_participant_quarantines_instead_of_converging() {
    for victim in ["pages/Today.md", "pages/Older.md", "pages/Oldest.md"] {
        let fixture = Fixture::new(&format!(
            "external-{}",
            victim.replace(['/', '.'], "-")
        ));
        two_source_graph(&fixture);
        let record = {
            let graph = Graph::open(&fixture.graph_root);
            let (destination, sources) = plan_move(&graph, "Today", &["Older", "Oldest"]);
            let prepared = graph
                .prepare_direct_cross_page_move(&destination, &sources)
                .unwrap()
                .unwrap();
            fixture
                .store()
                .commit(&prepared.record, &prepared.images)
                .unwrap();
            graph
                .save_page(&destination, destination.rev.as_deref())
                .unwrap();
            prepared.record
        };
        // An external editor, a second honest instance, or a sync delivery.
        fixture.write(victim, "- somebody else wrote this\n");
        let mid_crash = fixture.snapshot();

        let report = recover_all(&fixture.store_root, &fixture.graph_root);
        assert!(
            matches!(
                report.outcomes.as_slice(),
                [(id, RecordOutcome::Quarantined { .. })] if *id == record.move_id
            ),
            "{victim}: expected a quarantine, got {:?}",
            report.outcomes
        );
        assert_eq!(
            fixture.snapshot(),
            mid_crash,
            "{victim}: quarantine must not write a single graph byte"
        );
        assert!(
            fixture
                .store_root
                .join("quarantine")
                .join(format!("{}.json", record.move_id))
                .exists(),
            "{victim}: the recorded versions must be preserved beside the file's"
        );
        // And it never runs again: a quarantined record is not a licence to
        // overwrite the external write on some later open.
        assert!(recover_all(&fixture.store_root, &fixture.graph_root).is_empty());
    }
}

/// The window `src/store.ts` documents: the destination is marked dirty
/// synchronously, so the debounce may publish it before the record round-trip
/// returns. The record then observes an already-terminal destination — and
/// recovery must still carry the move FORWARD, which is the safe direction.
#[test]
fn record_composed_after_the_destination_landed_still_completes_forward() {
    let after = uncrashed_terminal_bytes("window", two_source_graph);
    let fixture = Fixture::new("window");
    two_source_graph(&fixture);
    {
        let graph = Graph::open(&fixture.graph_root);
        let (destination, sources) = plan_move(&graph, "Today", &["Older", "Oldest"]);
        // The destination lands BEFORE the record is composed.
        graph
            .save_page(&destination, destination.rev.as_deref())
            .unwrap();
        let prepared = graph
            .prepare_direct_cross_page_move(&destination, &sources)
            .unwrap()
            .unwrap();
        fixture
            .store()
            .commit(&prepared.record, &prepared.images)
            .unwrap();
    }
    let report = recover_all(&fixture.store_root, &fixture.graph_root);
    assert!(matches!(
        report.outcomes.as_slice(),
        [(_, RecordOutcome::Completed { pages_written: 2 })]
    ));
    assert_eq!(fixture.snapshot(), after);
}

// ---------------------------------------------------------------------------
// Number-of-pages shapes
// ---------------------------------------------------------------------------

/// A same-page "move" resolves to ONE file on both sides. It must NOT create a
/// record: it is a single ordinary save, and the base-revision guard already
/// makes that safe. A record would be a second, redundant authority over the
/// same bytes.
#[test]
fn a_same_page_move_composes_no_record() {
    let fixture = Fixture::new("degenerate");
    fixture.write("pages/Today.md", "- a\n- b\n");
    let graph = Graph::open(&fixture.graph_root);
    let mut dto = graph
        .load_named("Today", PageKind::Page)
        .unwrap()
        .unwrap();
    dto.blocks.swap(0, 1);
    assert!(graph
        .prepare_direct_cross_page_move(&dto, std::slice::from_ref(&dto))
        .unwrap()
        .is_none());
    assert!(!fixture.store_root.join("records").exists());
}

/// A feed day and a routed named page take the same path through composition —
/// both resolve to a save target and a pair of images. (`doc.feed` vs
/// `pageVisibleOrder` is a frontend distinction; storage sees files.)
#[test]
fn a_journal_and_a_named_page_participate_identically() {
    let fixture = Fixture::new("feed-vs-page");
    fixture.write("journals/2026_09_01.md", "- today\n");
    fixture.write("pages/Project.md", "- move me\n- keep me\n");
    let graph = Graph::open(&fixture.graph_root);
    let (destination, sources) = plan_move(&graph, "Sep 1st, 2026", &["Project"]);
    let prepared = graph
        .prepare_direct_cross_page_move(&destination, &sources)
        .unwrap()
        .unwrap();
    let roles: Vec<_> = prepared
        .record
        .participants
        .iter()
        .map(|participant| (participant.role, participant.page_kind.as_str()))
        .collect();
    assert_eq!(
        roles,
        vec![
            (ParticipantRole::Destination, "journal"),
            (ParticipantRole::Source, "page"),
        ]
    );
    assert_eq!(
        prepared.record.participants[0].relative_path,
        "journals/2026_09_01.md"
    );
    assert_eq!(prepared.record.participants[1].relative_path, "pages/Project.md");
}

/// A move whose destination file does not exist yet: the preimage is genuinely
/// `Absent`, and rolling back means removing the file the move created.
#[test]
fn a_move_into_a_page_with_no_file_rolls_back_by_removing_it() {
    let fixture = Fixture::new("absent-destination");
    fixture.write("pages/Older.md", "- move me\n- keep me\n");
    let before = fixture.snapshot();
    let record = {
        let graph = Graph::open(&fixture.graph_root);
        let mut destination = crate::model::markdown_page_dto("Today", "Today", "").unwrap();
        let mut source = graph.load_named("Older", PageKind::Page).unwrap().unwrap();
        destination.blocks.push(source.blocks.remove(0));
        let prepared = graph
            .prepare_direct_cross_page_move(&destination, std::slice::from_ref(&source))
            .unwrap()
            .unwrap();
        assert_eq!(prepared.record.participants[0].preimage, ImageRef::Absent);
        assert_eq!(prepared.record.participants[0].base_revision, None);
        fixture
            .store()
            .commit(&prepared.record, &prepared.images)
            .unwrap();
        // Only the removal lands — the destination file is never created.
        graph.save_page(&source, source.rev.as_deref()).unwrap();
        prepared.record
    };
    let report = recover_all(&fixture.store_root, &fixture.graph_root);
    assert_eq!(
        report.outcomes,
        vec![(record.move_id, RecordOutcome::RolledBack { pages_written: 1 })]
    );
    assert_eq!(fixture.snapshot(), before);
    assert!(!fixture.graph_root.join("pages/Today.md").exists());
}

// ---------------------------------------------------------------------------
// Byte compatibility (I-4) — gated, not asserted
// ---------------------------------------------------------------------------

/// For every format/line-ending/header shape, prove three things at once:
///
/// 1. every file the move does NOT own is byte-identical afterwards;
/// 2. the completed recovery state is byte-identical to what an UNCRASHED move
///    writes — which is what discharges the Logseq-oracle requirement by
///    reduction: recovery publishes the very bytes the ordinary Direct save
///    publishes (both come from `serialize_page_dto_for_path`), so no new byte
///    string ever reaches the user's graph through this path and there is
///    nothing for an oracle to disagree with that the ordinary save has not
///    already been gated on;
/// 3. the rolled-back state is byte-identical to the pre-move graph.
#[test]
fn both_terminal_states_are_byte_exact_for_every_format_shape() {
    struct Shape {
        tag: &'static str,
        destination: (&'static str, &'static str, &'static str),
        source: (&'static str, &'static str, &'static str),
        bystander: (&'static str, &'static str),
    }

    let shapes = [
        Shape {
            tag: "markdown-lf",
            destination: ("Today", "pages/Today.md", "- today keeps this\n"),
            source: ("Older", "pages/Older.md", "- move me\n- keep me\n"),
            bystander: ("pages/Untouched.md", "- not part of the move\n"),
        },
        Shape {
            tag: "markdown-crlf",
            destination: ("Today", "pages/Today.md", "- today keeps this\r\n"),
            source: ("Older", "pages/Older.md", "- move me\r\n- keep me\r\n"),
            bystander: ("pages/Untouched.md", "- crlf bystander\r\n"),
        },
        Shape {
            tag: "markdown-properties",
            destination: (
                "Today",
                "pages/Today.md",
                "title:: Today\nalias:: T\ntags:: a, b\n\n- today keeps this\n",
            ),
            source: (
                "Older",
                "pages/Older.md",
                "title:: Older\ntags:: z\n\n- move me\n  key:: value\n- keep me\n",
            ),
            bystander: (
                "pages/Untouched.md",
                "title:: Untouched\nalias:: U\n\n- bystander\n",
            ),
        },
        Shape {
            tag: "markdown-headings",
            destination: ("Today", "pages/Today.md", "- ## today heading\n"),
            source: (
                "Older",
                "pages/Older.md",
                "- # move this heading\n\t- nested child\n- ### keep me\n",
            ),
            bystander: ("pages/Untouched.md", "- #### bystander heading\n"),
        },
        Shape {
            tag: "org-lf",
            destination: ("Today", "pages/Today.org", "* today keeps this\n"),
            source: ("Older", "pages/Older.org", "* move me\n* keep me\n"),
            bystander: ("pages/Untouched.org", "* not part of the move\n"),
        },
        Shape {
            tag: "org-crlf",
            destination: ("Today", "pages/Today.org", "* today keeps this\r\n"),
            source: ("Older", "pages/Older.org", "* move me\r\n* keep me\r\n"),
            bystander: ("pages/Untouched.org", "* crlf bystander\r\n"),
        },
    ];

    for shape in shapes {
        let build = |fixture: &Fixture| {
            fixture.write(shape.destination.1, shape.destination.2);
            fixture.write(shape.source.1, shape.source.2);
            fixture.write(shape.bystander.0, shape.bystander.1);
        };

        // The uncrashed oracle.
        let uncrashed = {
            let fixture = Fixture::new(&format!("bytes-{}-oracle", shape.tag));
            build(&fixture);
            let graph = Graph::open(&fixture.graph_root);
            let (destination, sources) = plan_move(&graph, shape.destination.0, &[shape.source.0]);
            graph
                .save_page(&destination, destination.rev.as_deref())
                .unwrap();
            graph
                .save_page(&sources[0], sources[0].rev.as_deref())
                .unwrap();
            fixture.snapshot()
        };

        // Completed by recovery after a crash between the two page writes.
        let completed = {
            let fixture = Fixture::new(&format!("bytes-{}-complete", shape.tag));
            build(&fixture);
            {
                let graph = Graph::open(&fixture.graph_root);
                let (destination, sources) =
                    plan_move(&graph, shape.destination.0, &[shape.source.0]);
                run_to_cut(&fixture, &graph, &destination, &sources, 2);
            }
            recover_all(&fixture.store_root, &fixture.graph_root);
            fixture.snapshot()
        };

        // Rolled back after the removal landed without its addition.
        let (rolled_back, before) = {
            let fixture = Fixture::new(&format!("bytes-{}-rollback", shape.tag));
            build(&fixture);
            let before = fixture.snapshot();
            {
                let graph = Graph::open(&fixture.graph_root);
                let (destination, sources) =
                    plan_move(&graph, shape.destination.0, &[shape.source.0]);
                let prepared = graph
                    .prepare_direct_cross_page_move(&destination, &sources)
                    .unwrap()
                    .unwrap();
                fixture
                    .store()
                    .commit(&prepared.record, &prepared.images)
                    .unwrap();
                graph
                    .save_page(&sources[0], sources[0].rev.as_deref())
                    .unwrap();
            }
            recover_all(&fixture.store_root, &fixture.graph_root);
            (fixture.snapshot(), before)
        };

        assert_eq!(
            completed, uncrashed,
            "{}: the completed recovery state must be the exact bytes an uncrashed move writes",
            shape.tag
        );
        assert_eq!(
            rolled_back, before,
            "{}: the rolled-back state must be the exact pre-move bytes",
            shape.tag
        );
        assert_eq!(
            completed.get(shape.bystander.0).map(Vec::as_slice),
            Some(shape.bystander.1.as_bytes()),
            "{}: a file the move does not own was rewritten",
            shape.tag
        );
        assert_eq!(
            rolled_back.get(shape.bystander.0).map(Vec::as_slice),
            Some(shape.bystander.1.as_bytes()),
            "{}: a file the move does not own was rewritten on rollback",
            shape.tag
        );
    }
}

/// Duplicate-looking page identities: a title-named journal beside its
/// canonical date-stem file. The move must bind the PHYSICAL file each editor
/// holds, not re-resolve a name — otherwise recovery could publish one page's
/// bytes into its twin.
#[test]
fn duplicate_looking_identities_bind_the_physical_file() {
    let fixture = Fixture::new("twin-identity");
    fixture.write("journals/2026_09_01.md", "- canonical day\n");
    fixture.write("journals/Sep 1st, 2026.md", "- stray twin\n- move me\n");
    fixture.write("pages/Other.md", "- move me too\n- keep me\n");
    let graph = Graph::open(&fixture.graph_root);

    let mut destination = graph
        .load_by_path("journals/Sep 1st, 2026.md")
        .unwrap()
        .expect("the stray twin loads by its own path");
    let mut source = graph.load_named("Other", PageKind::Page).unwrap().unwrap();
    destination.blocks.push(source.blocks.remove(0));

    let prepared = graph
        .prepare_direct_cross_page_move(&destination, std::slice::from_ref(&source))
        .unwrap()
        .unwrap();
    assert_eq!(
        prepared.record.participants[0].relative_path,
        "journals/Sep 1st, 2026.md",
        "the record must name the file the editor is pinned to, not the canonical twin"
    );
    fixture
        .store()
        .commit(&prepared.record, &prepared.images)
        .unwrap();
    graph
        .save_page(&destination, destination.rev.as_deref())
        .unwrap();
    drop(graph);

    recover_all(&fixture.store_root, &fixture.graph_root);
    assert_eq!(
        fixture.read("journals/2026_09_01.md").as_deref(),
        Some("- canonical day\n"),
        "the canonical twin is not a participant and must be untouched"
    );
}

// ---------------------------------------------------------------------------
// Store mechanics
// ---------------------------------------------------------------------------

/// Blobs must be durable BEFORE the record that names them is published; a
/// record pointing at bytes that are not on stable storage cannot recover.
/// Proven the only way that is observable without a power cut: after `commit`
/// returns, every named blob exists and hashes to its own name.
#[test]
fn every_image_is_durable_before_the_record_names_it() {
    let fixture = Fixture::new("blob-order");
    two_source_graph(&fixture);
    let graph = Graph::open(&fixture.graph_root);
    let (destination, sources) = plan_move(&graph, "Today", &["Older", "Oldest"]);
    let prepared = graph
        .prepare_direct_cross_page_move(&destination, &sources)
        .unwrap()
        .unwrap();
    fixture
        .store()
        .commit(&prepared.record, &prepared.images)
        .unwrap();
    for participant in &prepared.record.participants {
        for image in [&participant.preimage, &participant.postimage] {
            if let ImageRef::Blob { sha256, len } = image {
                let bytes = fixture.store().read_blob(sha256).expect("blob is durable");
                assert_eq!(bytes.len() as u64, *len);
            }
        }
    }
}

/// A record that will not decode, or names a path escaping the graph root, is
/// unrecognized private state: preserved, never acted on, never able to block
/// the open.
#[test]
fn malformed_private_state_is_preserved_and_never_applied() {
    let fixture = Fixture::new("malformed");
    fixture.write("pages/Today.md", "- untouched\n");
    let before = fixture.snapshot();
    let records = fixture.store_root.join("records");
    fs::create_dir_all(&records).unwrap();
    fs::write(records.join("garbage.json"), b"{not json").unwrap();

    let escaping = DirectMoveRecord {
        schema: RECORD_SCHEMA,
        move_id: "escaping".to_string(),
        graph_root: fixture.graph_root.display().to_string(),
        created_unix_ms: 1,
        participants: vec![
            MoveParticipant {
                role: ParticipantRole::Destination,
                relative_path: "../outside.md".to_string(),
                page_name: "Outside".to_string(),
                page_kind: "page".to_string(),
                base_revision: None,
                preimage: ImageRef::Absent,
                postimage: ImageRef::blob_of(b"- written outside the graph\n"),
            },
            MoveParticipant {
                role: ParticipantRole::Source,
                relative_path: "pages/Today.md".to_string(),
                page_name: "Today".to_string(),
                page_kind: "page".to_string(),
                base_revision: Some(content_rev("- untouched\n")),
                preimage: ImageRef::blob_of(b"- untouched\n"),
                postimage: ImageRef::Absent,
            },
        ],
    };
    fs::write(
        records.join("escaping.json"),
        serde_json::to_vec(&escaping).unwrap(),
    )
    .unwrap();

    let report = recover_all(&fixture.store_root, &fixture.graph_root);
    assert!(report.is_empty(), "neither record may be acted on");
    assert_eq!(fixture.snapshot(), before);
    assert!(!fixture
        .graph_root
        .parent()
        .unwrap()
        .join("outside.md")
        .exists());
    assert!(fixture.store_root.join("quarantine/garbage.json").exists());
    assert!(fixture.store_root.join("quarantine/escaping.json").exists());
}

/// `finish` retires only a fully terminal move. A record whose source save is
/// still outstanding (a live conflict, say) is deliberately left for the next
/// open — `finish` must never write a graph byte to force the issue.
#[test]
fn finish_retires_only_a_fully_terminal_move() {
    let fixture = Fixture::new("finish");
    two_source_graph(&fixture);
    let graph = Graph::open(&fixture.graph_root);
    let (destination, sources) = plan_move(&graph, "Today", &["Older", "Oldest"]);
    let prepared = graph
        .prepare_direct_cross_page_move(&destination, &sources)
        .unwrap()
        .unwrap();
    let store_root = fixture.store_root.clone();
    fixture
        .store()
        .commit(&prepared.record, &prepared.images)
        .unwrap();
    graph
        .save_page(&destination, destination.rev.as_deref())
        .unwrap();
    graph
        .save_page(&sources[0], sources[0].rev.as_deref())
        .unwrap();

    let partial = fixture.snapshot();
    assert!(!retire_if_terminal(
        &store_root,
        &fixture.graph_root,
        &prepared.record.move_id
    ));
    assert_eq!(fixture.snapshot(), partial, "finish must not write");

    graph
        .save_page(&sources[1], sources[1].rev.as_deref())
        .unwrap();
    assert!(retire_if_terminal(
        &store_root,
        &fixture.graph_root,
        &prepared.record.move_id
    ));
    assert!(recover_all(&store_root, &fixture.graph_root).is_empty());
}

/// The bounded cleanup the contract promises (§5): quarantine is capped, and a
/// blob no live record names is reclaimed.
#[test]
fn cleanup_is_bounded_and_reclaims_unreferenced_images() {
    let fixture = Fixture::new("sweep");
    let store = fixture.store();
    let blobs = fixture.store_root.join("blobs");
    fs::create_dir_all(&blobs).unwrap();
    let orphan = crate::direct_move_recovery::hex_digest(b"nobody references me");
    fs::write(blobs.join(&orphan), b"nobody references me").unwrap();

    let quarantine = fixture.store_root.join("quarantine");
    fs::create_dir_all(&quarantine).unwrap();
    for index in 0..(crate::direct_move_recovery::QUARANTINE_RETENTION + 8) {
        fs::write(quarantine.join(format!("q{index:03}.json")), b"{}").unwrap();
    }

    store.sweep();

    assert!(!blobs.join(&orphan).exists(), "an unreferenced image is reclaimed");
    let retained = fs::read_dir(&quarantine).unwrap().count();
    assert_eq!(retained, crate::direct_move_recovery::QUARANTINE_RETENTION);
}

// ---------------------------------------------------------------------------
// Doc-code consistency
// ---------------------------------------------------------------------------

/// The living contract records the durable-step order the crash matrix cuts
/// between and the retention bound the sweep enforces. Both are load-bearing
/// values, so they fail CI when they drift instead of quietly becoming fiction
/// (the same-commit contract rule).
#[test]
fn the_contract_document_states_the_load_bearing_values() {
    let record = DirectMoveRecord {
        schema: RECORD_SCHEMA,
        move_id: "abc".to_string(),
        graph_root: "/graph".to_string(),
        created_unix_ms: 0,
        participants: vec![
            MoveParticipant {
                role: ParticipantRole::Destination,
                relative_path: "pages/A.md".to_string(),
                page_name: "A".to_string(),
                page_kind: "page".to_string(),
                base_revision: None,
                preimage: ImageRef::Absent,
                postimage: ImageRef::blob_of(b"a"),
            },
            MoveParticipant {
                role: ParticipantRole::Source,
                relative_path: "pages/B.md".to_string(),
                page_name: "B".to_string(),
                page_kind: "page".to_string(),
                base_revision: None,
                preimage: ImageRef::blob_of(b"b"),
                postimage: ImageRef::blob_of(b"c"),
            },
        ],
    };
    assert_eq!(
        direct_move_durable_steps(&record),
        vec![
            DurableStep::CommitRecord,
            DurableStep::WriteDestination,
            DurableStep::WriteSource(0),
            DurableStep::RetireRecord,
        ]
    );
    for phrase in [
        "commit the record",
        "write the destination",
        "write each source",
        "retire the record",
    ] {
        assert!(
            CONTRACT.to_lowercase().contains(phrase),
            "the contract must name the durable step: {phrase}"
        );
    }
    assert!(
        CONTRACT.contains(&format!(
            "QUARANTINE_RETENTION = {}",
            crate::direct_move_recovery::QUARANTINE_RETENTION
        )),
        "the contract must state the retention bound the code enforces"
    );
    assert!(
        CONTRACT.contains(&format!("schema {RECORD_SCHEMA}")),
        "the contract must state the one current record schema"
    );
}
