//! GH #535: a rename touches only the pages it moves or rewrites, reports
//! them, leaves every other editor alone, and refuses to rewrite a file the
//! frontend holds unsaved edits for.

use super::*;

fn referrer_graph(tag: &str) -> (PathBuf, Graph) {
    let dir = scratch(tag);
    fs::write(dir.join("pages/Target.md"), "- the target\n").unwrap();
    fs::write(dir.join("pages").join("Target%2FChild.md"), "- child\n").unwrap();
    fs::write(dir.join("pages/Referrer.md"), "- links [[Target]]\n").unwrap();
    fs::write(
        dir.join("journals/2026_09_21.md"),
        "- met [[Target/Child]]\n",
    )
    .unwrap();
    fs::write(dir.join("pages/Unrelated.md"), "- nothing to see\n").unwrap();
    let graph = Graph::open(&dir);
    graph.warm_cache();
    (dir, graph)
}

#[test]
fn a_rename_reports_every_page_it_moved_or_rewrote_and_nothing_else() {
    let (dir, graph) = referrer_graph("gh535-rename-touched");
    let outcome = graph
        .rename_page_reporting("Target", "Renamed", None)
        .unwrap();
    let mut touched = outcome.touched.clone();
    touched.sort_by(|a, b| a.path.cmp(&b.path));
    let expected =
        |name: &str, kind: PageKind, path: &str, renamed_to: Option<&str>| RenameTouchedPage {
            name: name.into(),
            kind,
            path: path.into(),
            renamed_to: renamed_to.map(Into::into),
        };
    assert_eq!(
        touched,
        vec![
            expected(
                "Sep 21st, 2026",
                PageKind::Journal,
                "journals/2026_09_21.md",
                None
            ),
            expected("Referrer", PageKind::Page, "pages/Referrer.md", None),
            expected(
                "Target/Child",
                PageKind::Page,
                "pages/Target%2FChild.md",
                Some("Renamed/Child")
            ),
            expected("Target", PageKind::Page, "pages/Target.md", Some("Renamed")),
        ]
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_rename_refuses_to_rewrite_a_file_with_unsaved_edits_and_changes_nothing() {
    let (dir, graph) = referrer_graph("gh535-rename-guarded");
    let before: Vec<_> = [
        "pages/Target.md",
        "pages/Referrer.md",
        "journals/2026_09_21.md",
    ]
    .iter()
    .map(|path| fs::read_to_string(dir.join(path)).unwrap())
    .collect();

    let error = graph
        .rename_page_guarded("Target", "Renamed", None, &["pages/Referrer.md".into()])
        .expect_err("rewriting a file under unsaved edits must refuse");
    assert!(error.to_string().contains("“Referrer”"), "{error}");
    let after: Vec<_> = [
        "pages/Target.md",
        "pages/Referrer.md",
        "journals/2026_09_21.md",
    ]
    .iter()
    .map(|path| fs::read_to_string(dir.join(path)).unwrap())
    .collect();
    assert_eq!(before, after);
    assert!(!dir.join("pages/Renamed.md").exists());

    // Unsaved edits on a page the rename does not touch do not block it.
    let outcome = graph
        .rename_page_guarded("Target", "Renamed", None, &["pages/Unrelated.md".into()])
        .unwrap();
    assert!(outcome
        .touched
        .iter()
        .all(|page| page.path != "pages/Unrelated.md"));
    assert!(dir.join("pages/Renamed.md").exists());
    let _ = fs::remove_dir_all(dir);
}

/// Only the moved page's editor dies with the rename. A referrer's editor
/// survives, because the frontend no longer resets every page: an instance it
/// keeps (edited while the rename ran) still saves against its own
/// activation, and the stale draft meets the rewrite as a reviewable conflict
/// (it carries an epoch) rather than overwriting it. An untouched page's
/// editor is not disturbed at all.
#[test]
fn a_rename_retires_only_the_moved_editor_and_a_stale_referrer_conflicts_reviewably() {
    let (dir, graph) = referrer_graph("gh535-rename-editors");
    let activate = |path: &str| {
        let mut page = graph.load_by_path(path).unwrap().unwrap();
        let handle = graph
            .activate_editor(path, ActivationIntent::Replace, page.rev.as_deref())
            .unwrap();
        page.activation = Some(handle.activation.as_u64());
        (page, handle.activation)
    };
    let (_, target) = activate("pages/Target.md");
    let (mut referrer, referrer_activation) = activate("pages/Referrer.md");
    let (mut unrelated, _) = activate("pages/Unrelated.md");

    graph
        .rename_page_reporting("Target", "Renamed", None)
        .unwrap();

    assert!(
        !graph.retire_editor_activation("pages/Target.md", target),
        "the moved page's editor must be retired"
    );
    assert!(graph.editor_activation_is_live(&dir.join("pages/Referrer.md"), referrer_activation));

    referrer.blocks[0].raw = "typed during the rename".into();
    let error = graph
        .save_page(&referrer, referrer.rev.as_deref())
        .expect_err("the stale referrer draft must not overwrite the rewrite");
    let typed = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<DirectSaveError>())
        .expect("a typed save failure");
    assert!(
        typed.conflict_epoch().is_some(),
        "the conflict must be reviewable: {:?}",
        typed.code()
    );
    assert_eq!(
        fs::read_to_string(dir.join("pages/Referrer.md")).unwrap(),
        "- links [[Renamed]]\n"
    );

    unrelated.blocks[0].raw = "still editing".into();
    graph
        .save_page(&unrelated, unrelated.rev.as_deref())
        .expect("an untouched page saves as if nothing happened");
    let _ = fs::remove_dir_all(dir);
}
