//! Journal-feed selection: which journal pages the Journals surface shows, in
//! what order, and how its day cursor paginates.
//!
//! Both storage modes select the feed through the rules here. Direct Files
//! supplies candidates from its warmed page cache (`Graph::journals_desc`, which
//! calls [`journal_feed_candidates_desc`] below); managed storage supplies them
//! from the actor's retained journal index. One implementation is the point: the
//! feed's dedup/ordering/cursor rules are exactly where a silent divergence would
//! drop a day out of a user's journal history, and two hand-kept copies agreeing
//! only by inspection is how that happens.
//!
//! `model.rs` did carry a second copy (`dedup_journal_days`), and it had already
//! drifted: its canonicality test read only `path` where
//! [`canonical_journal_entry`] reads `rel_path` first. `tests::direct_files_journals_desc_uses_this_files_dedup`
//! is the architectural fact that keeps the delegation in place.

use crate::date::JournalDate;
use crate::model::{PageDto, PageEntry, PageKind};

/// One rendered feed page plus the day cursor that continues it.
#[derive(Debug)]
pub struct JournalFeedSelection {
    pub pages: Vec<PageDto>,
    pub next_before_day: Option<i64>,
    pub done: bool,
}

/// True when this entry's own filename is a journal date stem.
///
/// A day may own more than one file — most often a leftover title-named
/// duplicate (`Jun 18th, 2026.md`) of the graph's `yyyy_MM_dd` file. Both
/// resolve to the same page, so the feed shows the day once and prefers the
/// date-stem file; the stray stays reachable for reconciliation.
pub fn canonical_journal_entry(entry: &PageEntry) -> bool {
    let relative = std::path::Path::new(&entry.rel_path);
    let path = if entry.rel_path.is_empty() {
        entry.path.as_path()
    } else {
        relative
    };
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| JournalDate::from_file_stem(stem).is_some())
}

/// Every dated journal day in one raw page inventory, one entry per day,
/// newest first.
///
/// Deliberately independent of the feed's as-of day so a caller may retain the
/// result across a calendar rollover: [`journal_feed_inventory`] applies the
/// cutoff afterwards, and cutting by day after deduplicating by day selects
/// the same representative as cutting before it.
pub fn journal_feed_candidates_desc(entries: Vec<PageEntry>) -> Vec<PageEntry> {
    let mut positions = std::collections::HashMap::new();
    let mut deduplicated: Vec<PageEntry> = Vec::new();
    for entry in entries {
        if entry.kind != PageKind::Journal {
            continue;
        }
        let Some(day) = entry.date_key else {
            continue;
        };
        if let Some(&position) = positions.get(&day) {
            if canonical_journal_entry(&entry) && !canonical_journal_entry(&deduplicated[position])
            {
                deduplicated[position] = entry;
            }
        } else {
            positions.insert(day, deduplicated.len());
            deduplicated.push(entry);
        }
    }
    deduplicated.sort_by_key(|entry| std::cmp::Reverse(entry.date_key.unwrap_or(0)));
    deduplicated
}

/// Feed membership is narrower than the raw journal inventory: a future
/// journal remains a directly reachable page but is not in the feed.
pub fn journal_feed_inventory(entries: Vec<PageEntry>, as_of_day: i64) -> Vec<PageEntry> {
    let mut candidates = journal_feed_candidates_desc(entries);
    candidates.retain(|entry| entry.date_key.is_some_and(|day| day <= as_of_day));
    candidates
}

/// True when this candidate belongs on the page the caller asked for: at or
/// before the feed's as-of day, and strictly older than the cursor.
pub fn journal_feed_candidate_in_window(
    entry: &PageEntry,
    as_of_day: i64,
    before_day: Option<i64>,
) -> bool {
    entry
        .date_key
        .is_some_and(|day| day <= as_of_day && before_day.is_none_or(|before| day < before))
}

/// Feed-only pagination over already-windowed candidates, newest first.
///
/// The cursor this returns is an ordinal-day value rather than a mutable
/// vector offset, so a file disappearing after selection cannot make a later
/// day duplicate or disappear from the next request. A candidate whose page
/// has vanished is skipped, but its day still advances the cursor.
pub fn collect_journal_feed_page<I, F>(
    candidates: I,
    limit: usize,
    mut load: F,
) -> Result<JournalFeedSelection, String>
where
    I: Iterator<Item = PageEntry>,
    F: FnMut(&PageEntry) -> Result<PageDto, std::io::Error>,
{
    let mut candidates = candidates.peekable();
    // A zero limit is authoritative: do not scan/load the feed merely to
    // discover that the caller requested no rows. No cursor advances because
    // no day was examined.
    if limit == 0 {
        return Ok(JournalFeedSelection {
            pages: Vec::new(),
            next_before_day: None,
            done: candidates.peek().is_none(),
        });
    }
    let mut pages = Vec::new();
    let mut last_examined = None;
    while let Some(entry) = candidates.next() {
        let day = entry
            .date_key
            .expect("feed candidates only contain dated journals");
        last_examined = Some(day);
        match load(&entry) {
            Ok(page) => pages.push(page),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        if pages.len() == limit {
            break;
        }
    }
    let done = candidates.peek().is_none();
    Ok(JournalFeedSelection {
        pages,
        next_before_day: if done { None } else { last_examined },
        done,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The module header says both storage modes select the feed through this
    /// file. Direct Files reaches it through `Graph::journals_desc`, which used
    /// to carry its own copy of the dedup rule instead. A comment cannot hold
    /// that; this can.
    #[test]
    fn direct_files_journals_desc_uses_this_files_dedup() {
        let model = include_str!("model.rs");
        assert!(
            !model.contains("fn dedup_journal_days"),
            "model.rs has grown a second journal-day dedup implementation; the feed's \
             representative-file rule must have exactly one owner (this file), or a day \
             silently drops out of a user's history when the two disagree"
        );
        let start = model
            .find("pub fn journals_desc(&self)")
            .expect("model.rs must still define Graph::journals_desc");
        let body = &model[start..start + 2000.min(model.len() - start)];
        let end = body
            .find("\n    }")
            .expect("journals_desc body must terminate within the scanned window");
        assert!(
            body[..end].contains("journal_feed_candidates_desc"),
            "Graph::journals_desc no longer delegates to journal_feed::journal_feed_candidates_desc"
        );
    }

    /// `canonical_journal_entry` prefers `rel_path`; the deleted `model.rs` copy
    /// inspected `path` only. Pin the authority, since that was the live drift.
    #[test]
    fn the_representative_file_is_chosen_by_relative_path() {
        let strayed = PageEntry {
            name: "Jun 18th, 2026".into(),
            kind: PageKind::Journal,
            date_key: Some(20260618),
            rel_path: "journals/Jun 18th, 2026.md".into(),
            // A stale/absolute path whose stem WOULD read as canonical must not
            // win over the entry's own relative path.
            path: PathBuf::from("/graph/journals/2026_06_18.md"),
        };
        assert!(!canonical_journal_entry(&strayed));

        let canonical = PageEntry {
            rel_path: "journals/2026_06_18.md".into(),
            path: PathBuf::from("/graph/journals/Jun 18th, 2026.md"),
            ..strayed.clone()
        };
        assert!(canonical_journal_entry(&canonical));

        let deduplicated = journal_feed_candidates_desc(vec![strayed, canonical]);
        assert_eq!(deduplicated.len(), 1);
        assert_eq!(deduplicated[0].rel_path, "journals/2026_06_18.md");
    }

    fn entry(day: i64) -> PageEntry {
        PageEntry {
            name: day.to_string(),
            kind: PageKind::Journal,
            date_key: Some(day),
            rel_path: String::new(),
            path: PathBuf::new(),
        }
    }

    fn dto(entry: &PageEntry) -> PageDto {
        serde_json::from_value(serde_json::json!({
            "name": entry.name, "kind": "journal", "title": entry.name,
            "pre_block": null, "blocks": []
        }))
        .unwrap()
    }

    fn window(days: &[i64], as_of_day: i64, before_day: Option<i64>) -> Vec<PageEntry> {
        days.iter()
            .copied()
            .map(entry)
            .filter(|candidate| journal_feed_candidate_in_window(candidate, as_of_day, before_day))
            .collect()
    }

    #[test]
    fn deletion_stable_day_cursor_fills_then_continues_without_duplicates() {
        let first = collect_journal_feed_page(
            window(&[5, 4, 3, 2, 1], 5, None).into_iter(),
            3,
            |candidate| {
                if candidate.date_key == Some(5) {
                    Err(std::io::Error::from(std::io::ErrorKind::NotFound))
                } else {
                    Ok(dto(candidate))
                }
            },
        )
        .unwrap();
        assert_eq!(
            first
                .pages
                .iter()
                .map(|page| page.name.as_str())
                .collect::<Vec<_>>(),
            ["4", "3", "2"]
        );
        assert_eq!(first.next_before_day, Some(2));
        assert!(!first.done);
        let second = collect_journal_feed_page(
            window(&[5, 4, 3, 2, 1], 5, first.next_before_day).into_iter(),
            3,
            |candidate| Ok(dto(candidate)),
        )
        .unwrap();
        assert_eq!(
            second
                .pages
                .iter()
                .map(|page| page.name.as_str())
                .collect::<Vec<_>>(),
            ["1"]
        );
        assert!(second.done);
        assert_eq!(second.next_before_day, None);
    }

    #[test]
    fn cursor_handles_second_page_loss_empty_suffix_exact_limit_and_hard_errors() {
        let first =
            collect_journal_feed_page(window(&[5, 4, 3, 2, 1], 5, None).into_iter(), 3, |e| {
                Ok(dto(e))
            })
            .unwrap();
        assert_eq!(first.next_before_day, Some(3));
        let second = collect_journal_feed_page(
            window(&[5, 4, 3, 2, 1], 5, first.next_before_day).into_iter(),
            3,
            |candidate| {
                if candidate.date_key == Some(2) {
                    Err(std::io::Error::from(std::io::ErrorKind::NotFound))
                } else {
                    Ok(dto(candidate))
                }
            },
        )
        .unwrap();
        assert_eq!(
            second
                .pages
                .iter()
                .map(|page| page.name.as_str())
                .collect::<Vec<_>>(),
            ["1"]
        );
        assert!(
            second.done,
            "a missing second-page row still exhausts the suffix"
        );

        let empty =
            collect_journal_feed_page(window(&[5, 4], 5, Some(4)).into_iter(), 3, |e| Ok(dto(e)))
                .unwrap();
        assert!(empty.pages.is_empty());
        assert!(empty.done);

        let exact =
            collect_journal_feed_page(window(&[3, 2, 1], 3, None).into_iter(), 3, |e| Ok(dto(e)))
                .unwrap();
        assert!(exact.done, "an exactly-full final page is done");
        assert_eq!(exact.next_before_day, None);

        let hard = collect_journal_feed_page(window(&[3], 3, None).into_iter(), 1, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "denied",
            ))
        });
        assert!(matches!(hard, Err(error) if error.contains("denied")));
    }

    #[test]
    fn zero_limit_reports_remaining_days_without_loading_any() {
        let mut loads = 0;
        let page =
            collect_journal_feed_page(window(&[5, 4, 3], 5, None).into_iter(), 0, |candidate| {
                loads += 1;
                Ok(dto(candidate))
            })
            .unwrap();
        assert_eq!(loads, 0);
        assert!(page.pages.is_empty());
        assert!(!page.done);
        assert_eq!(page.next_before_day, None);

        let exhausted =
            collect_journal_feed_page(window(&[5, 4, 3], 5, Some(3)).into_iter(), 0, |candidate| {
                Ok(dto(candidate))
            })
            .unwrap();
        assert!(exhausted.done);
    }

    #[test]
    fn future_days_stay_out_of_the_feed_but_keep_their_page() {
        let inventory = vec![entry(9), entry(5), entry(4)];
        let feed = journal_feed_inventory(inventory.clone(), 5);
        assert_eq!(
            feed.iter().map(|e| e.date_key.unwrap()).collect::<Vec<_>>(),
            [5, 4]
        );
        // The same raw inventory retains the future day for direct navigation.
        assert_eq!(journal_feed_candidates_desc(inventory).len(), 3);
    }

    #[test]
    fn one_day_appears_once_and_prefers_its_date_stem_file() {
        let mut titled = entry(20_260_618);
        titled.rel_path = "journals/Jun 18th, 2026.md".to_owned();
        titled.name = "titled".to_owned();
        let mut stemmed = entry(20_260_618);
        stemmed.rel_path = "journals/2026_06_18.md".to_owned();
        stemmed.name = "stemmed".to_owned();

        for order in [
            vec![titled.clone(), stemmed.clone()],
            vec![stemmed.clone(), titled.clone()],
        ] {
            let deduplicated = journal_feed_candidates_desc(order);
            assert_eq!(deduplicated.len(), 1);
            assert_eq!(deduplicated[0].name, "stemmed");
        }
    }

    #[test]
    fn non_journal_and_undated_rows_never_become_candidates() {
        let mut ordinary = entry(3);
        ordinary.kind = PageKind::Page;
        ordinary.date_key = None;
        let mut undated_journal = entry(2);
        undated_journal.date_key = None;
        let candidates = journal_feed_candidates_desc(vec![ordinary, undated_journal, entry(1)]);
        assert_eq!(
            candidates.iter().map(|e| e.date_key).collect::<Vec<_>>(),
            [Some(1)]
        );
    }
}
