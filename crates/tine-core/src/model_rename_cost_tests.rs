//! Work a page rename does per file it rewrites (GH #406).
//!
//! A CHILD module of `model_tests`, like `model_alias_admission_tests`, so it
//! shares the `scratch` fixture instead of growing another copy while
//! `model_tests.rs` sits at its B1 size cap.

use super::*;

/// How many times the portable-name (case/NFC) check listed a directory while
/// renaming a page that `referrers` other pages link to.
fn portable_listings_for_rename(referrers: usize) -> usize {
    let dir = scratch(&format!("gh406-rename-listings-{referrers}"));
    for index in 0..40 {
        fs::write(
            dir.join("pages").join(format!("Unrelated {index}.md")),
            format!("- unrelated {index}\n"),
        )
        .unwrap();
    }
    fs::write(dir.join("pages/Target.md"), "- the target\n").unwrap();
    for index in 0..referrers {
        fs::write(
            dir.join("pages").join(format!("Referrer {index}.md")),
            format!("- links [[Target]] here\n"),
        )
        .unwrap();
    }
    let graph = Graph::open(&dir);
    graph.warm_cache();

    GRAPH_TEXT_PORTABLE_DIRECTORY_LISTINGS.with(|count| count.set(0));
    graph
        .rename_page_reporting("Target", "Renamed", None)
        .expect("a clean rename succeeds");
    let listings = GRAPH_TEXT_PORTABLE_DIRECTORY_LISTINGS.with(Cell::get);

    // The positive property the count is about: the rename really rewrote
    // every referrer, so a low count cannot come from skipped writes.
    for index in 0..referrers {
        let text =
            fs::read_to_string(dir.join("pages").join(format!("Referrer {index}.md"))).unwrap();
        assert!(
            text.contains("[[Renamed]]"),
            "referrer {index} was not rewritten: {text:?}"
        );
    }
    assert!(dir.join("pages/Renamed.md").exists());
    let _ = fs::remove_dir_all(&dir);
    listings
}

/// GH #406. Each rewritten file used to re-list its whole parent directory to
/// look for a case/NFC twin, so a rename touching k files of a folder holding
/// N pages did k x N work — measured at 2.2 ms per written file on an
/// 8,000-page `pages/`, 80% of an 800-referrer rename. The listing must be
/// paid once per transaction, not once per file.
#[test]
fn rename_lists_each_directory_a_bounded_number_of_times_regardless_of_referrers() {
    let few = portable_listings_for_rename(2);
    let many = portable_listings_for_rename(30);
    assert_eq!(
        few, many,
        "portable-name listings must not grow with the number of rewritten files \
         (2 referrers: {few}, 30 referrers: {many})"
    );
}

/// The rename's own destination is a CREATION, so its portable check must
/// stay live inside the batch: a case twin of the destination that appears
/// just before it is published, after `pages/` was already batched, must
/// refuse the rename and roll every file back, leaving the twin untouched.
#[test]
fn a_case_twin_appearing_mid_rename_still_refuses_and_rolls_back() {
    let dir = scratch("gh406-rename-mid-write-twin");
    fs::write(dir.join("pages/Target.md"), "- the target\n").unwrap();
    for index in 0..3 {
        fs::write(
            dir.join("pages").join(format!("Referrer {index}.md")),
            "- links [[Target]] here\n",
        )
        .unwrap();
    }
    let graph = Graph::open(&dir);
    graph.warm_cache();

    let twin = dir.join("pages/renamed.md");
    // The rename writes three referrers and moves the page: four mutations.
    // Arm the twin for the LAST one, so `pages/` has already been listed by
    // the transaction's batch and only a live check can see it.
    arm_twin_at_mutation(4, twin.clone());

    let error = graph
        .rename_page_reporting("Target", "Renamed", None)
        .expect_err("a portable twin of the destination must refuse the rename");
    let unfired = GRAPH_TEXT_WRITE_BEFORE_MUTATION.with(|hook| hook.borrow_mut().take());
    assert!(
        unfired.is_none(),
        "the rename made fewer mutations than the fixture expects"
    );
    assert!(
        twin.exists(),
        "the twin must have appeared during the rename"
    );

    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
    assert_eq!(
        fs::read_to_string(dir.join("pages/Target.md")).unwrap(),
        "- the target\n"
    );
    assert!(
        !dir.join("pages/Renamed.md").exists(),
        "the destination must be rolled back"
    );
    for index in 0..3 {
        assert_eq!(
            fs::read_to_string(dir.join("pages").join(format!("Referrer {index}.md"))).unwrap(),
            "- links [[Target]] here\n",
            "referrer {index} must be rolled back"
        );
    }
    assert_eq!(
        fs::read_to_string(&twin).unwrap(),
        "- someone else's page\n"
    );
    let _ = fs::remove_dir_all(&dir);
}

fn arm_twin_at_mutation(remaining: usize, twin: std::path::PathBuf) {
    GRAPH_TEXT_WRITE_BEFORE_MUTATION.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            if remaining == 1 {
                fs::write(&twin, "- someone else's page\n")?;
            } else {
                arm_twin_at_mutation(remaining - 1, twin);
            }
            Ok(())
        }));
    });
}

/// A referrer's case twin that appears after the transaction listed `pages/`
/// is invisible to the batched check of that referrer's in-place rewrite —
/// the same race the per-file check always had, over a longer window. The
/// rename must stay all-or-nothing: never a half-rolled-back graph whose
/// referrers point at a page that was restored under its old name.
#[test]
fn a_referrer_twin_appearing_mid_rename_leaves_the_rename_all_or_nothing() {
    let dir = scratch("gh406-rename-referrer-twin");
    fs::write(dir.join("pages/Target.md"), "- the target\n").unwrap();
    for index in 0..3 {
        fs::write(
            dir.join("pages").join(format!("Referrer {index}.md")),
            "- links [[Target]] here\n",
        )
        .unwrap();
    }
    let graph = Graph::open(&dir);
    graph.warm_cache();

    // Mutation 1 rewrites the first referrer and lists `pages/` into the
    // batch; the twin of the last referrer appears before mutation 2.
    let twin = dir.join("pages/referrer 2.md");
    arm_twin_at_mutation(2, twin.clone());

    let result = graph.rename_page_reporting("Target", "Renamed", None);
    let unfired = GRAPH_TEXT_WRITE_BEFORE_MUTATION.with(|hook| hook.borrow_mut().take());
    assert!(
        unfired.is_none(),
        "the rename made fewer mutations than the fixture expects"
    );

    let (page, link) = match &result {
        Ok(_) => ("pages/Renamed.md", "- links [[Renamed]] here\n"),
        Err(_) => ("pages/Target.md", "- links [[Target]] here\n"),
    };
    assert_eq!(
        fs::read_to_string(dir.join(page)).unwrap(),
        "- the target\n",
        "{result:?}"
    );
    let other = if page == "pages/Target.md" {
        "pages/Renamed.md"
    } else {
        "pages/Target.md"
    };
    assert!(!dir.join(other).exists(), "{result:?}");
    for index in 0..3 {
        assert_eq!(
            fs::read_to_string(dir.join("pages").join(format!("Referrer {index}.md"))).unwrap(),
            link,
            "referrer {index} disagrees with the page's name: {result:?}"
        );
    }
    assert_eq!(
        fs::read_to_string(&twin).unwrap(),
        "- someone else's page\n"
    );
    let _ = fs::remove_dir_all(&dir);
}
