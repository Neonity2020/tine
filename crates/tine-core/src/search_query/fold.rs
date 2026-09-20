use regex::Regex;
use std::ops::Range;
use std::sync::OnceLock;
use unicode_normalization::char::{canonical_combining_class, compose, decompose_compatible};
use unicode_segmentation::UnicodeSegmentation;

/// A6-folded text and one raw UTF-16 source range for every output scalar.
#[derive(Clone, Debug)]
pub(crate) struct MappedFold {
    pub(crate) text: String,
    pub(crate) sources: Vec<Range<usize>>,
}

#[derive(Clone, Debug)]
struct TaggedScalar {
    ch: char,
    source: Range<usize>,
    cluster: usize,
}

#[derive(Clone, Copy, Debug)]
struct RemovalWork {
    input_scalars: usize,
    work_units: usize,
}

fn mn_regex() -> &'static Regex {
    static MN: OnceLock<Regex> = OnceLock::new();
    MN.get_or_init(|| Regex::new(r"\A\p{General_Category=Nonspacing_Mark}\z").unwrap())
}

fn is_mn(ch: char) -> bool {
    let mut bytes = [0_u8; 4];
    mn_regex().is_match(ch.encode_utf8(&mut bytes))
}

/// The one native search fold: whole-string lowercase, compatibility
/// decomposition, Mn removal, canonical reorder, and canonical composition.
///
/// Whole-string lowercase supplies contextual forms such as final sigma. The
/// raw UTF-16 range attached to each scalar follows it through every later
/// step, and composition unions all contributors. Text-only callers use this
/// same mapped algorithm and discard the map.
pub(crate) fn fold(raw: &str) -> MappedFold {
    fold_with_removal_work(raw).0
}

fn fold_with_removal_work(raw: &str) -> (MappedFold, RemovalWork) {
    let lowered = raw.to_lowercase();
    let mut lowered_scalars = lowered.chars();
    let mut decomposed = Vec::new();
    let mut original_utf16 = 0;

    for (cluster_index, cluster) in raw.graphemes(true).enumerate() {
        for original in cluster.chars() {
            let scalar_start = original_utf16;
            original_utf16 += original.len_utf16();
            for _ in original.to_lowercase() {
                let contextual = lowered_scalars
                    .next()
                    .expect("whole-string lowercase preserves scalar partition length");
                decompose_compatible(contextual, |ch| {
                    decomposed.push(TaggedScalar {
                        ch,
                        source: scalar_start..original_utf16,
                        cluster: cluster_index,
                    });
                });
            }
        }
    }
    assert!(
        lowered_scalars.next().is_none(),
        "whole-string lowercase preserves scalar partition length"
    );

    let (retained, removal_work) = remove_mn_with_provenance(decomposed);
    let composed = canonical_compose(canonical_reorder(retained));
    let text = composed.iter().map(|tagged| tagged.ch).collect();
    let sources = composed.into_iter().map(|tagged| tagged.source).collect();
    (MappedFold { text, sources }, removal_work)
}

fn source_distance(mark: &TaggedScalar, retained: &TaggedScalar) -> (usize, bool) {
    if retained.source.end <= mark.source.start {
        (mark.source.start - retained.source.end, false)
    } else if mark.source.end <= retained.source.start {
        (retained.source.start - mark.source.end, true)
    } else {
        (0, false)
    }
}

fn remove_mn_with_provenance(input: Vec<TaggedScalar>) -> (Vec<TaggedScalar>, RemovalWork) {
    let input_scalars = input.len();
    let mut work_units = 0;

    // Tags are still in raw source order. The closest retained contributor in
    // the same grapheme is therefore one of the two retained neighbors. Delay
    // range unions so attaching one mark cannot change a later distance/tie.
    let mut removed = vec![false; input_scalars];
    let mut previous = vec![None; input_scalars];
    let mut previous_retained = None;
    for (at, tagged) in input.iter().enumerate() {
        work_units += 1;
        if previous_retained.is_some_and(|before: usize| input[before].cluster != tagged.cluster) {
            previous_retained = None;
        }
        previous[at] = previous_retained;
        removed[at] = is_mn(tagged.ch);
        if !removed[at] {
            previous_retained = Some(at);
        }
    }

    let mut next = vec![None; input_scalars];
    let mut next_retained = None;
    for (at, tagged) in input.iter().enumerate().rev() {
        work_units += 1;
        if next_retained.is_some_and(|after: usize| input[after].cluster != tagged.cluster) {
            next_retained = None;
        }
        next[at] = next_retained;
        if !removed[at] {
            next_retained = Some(at);
        }
    }

    let mut provenance: Vec<Option<Range<usize>>> = vec![None; input_scalars];
    for (at, mark) in input.iter().enumerate() {
        work_units += 1;
        if !removed[at] {
            continue;
        }
        let nearest = match (previous[at], next[at]) {
            (Some(before), Some(after)) => {
                if source_distance(mark, &input[before]) <= source_distance(mark, &input[after]) {
                    Some(before)
                } else {
                    Some(after)
                }
            }
            (before @ Some(_), None) => before,
            (None, after @ Some(_)) => after,
            (None, None) => None,
        };
        if let Some(nearest) = nearest {
            let span = provenance[nearest].get_or_insert_with(|| input[nearest].source.clone());
            span.start = span.start.min(mark.source.start);
            span.end = span.end.max(mark.source.end);
        }
    }

    let mut retained = Vec::with_capacity(input_scalars);
    for (at, mut tagged) in input.into_iter().enumerate() {
        work_units += 1;
        if removed[at] {
            continue;
        }
        if let Some(span) = provenance[at].take() {
            tagged.source.start = tagged.source.start.min(span.start);
            tagged.source.end = tagged.source.end.max(span.end);
        }
        retained.push(tagged);
    }
    (
        retained,
        RemovalWork {
            input_scalars,
            work_units,
        },
    )
}

fn canonical_reorder(input: Vec<TaggedScalar>) -> Vec<TaggedScalar> {
    fn flush(segment: &mut Vec<TaggedScalar>, output: &mut Vec<TaggedScalar>) {
        if segment.len() <= 1 {
            output.append(segment);
            return;
        }

        // Stable counting sort by canonical combining class. CCC is one byte,
        // so this is linear with a fixed-size table.
        let mut counts = [0_usize; 256];
        for tagged in segment.iter() {
            counts[canonical_combining_class(tagged.ch) as usize] += 1;
        }
        let mut positions = [0_usize; 256];
        for class in 1..positions.len() {
            positions[class] = positions[class - 1] + counts[class - 1];
        }
        let mut ordered = vec![None; segment.len()];
        for tagged in segment.drain(..) {
            let class = canonical_combining_class(tagged.ch) as usize;
            let at = positions[class];
            ordered[at] = Some(tagged);
            positions[class] += 1;
        }
        output.extend(ordered.into_iter().map(Option::unwrap));
    }

    let mut output = Vec::with_capacity(input.len());
    let mut segment = Vec::new();
    for tagged in input {
        if canonical_combining_class(tagged.ch) == 0 && !segment.is_empty() {
            flush(&mut segment, &mut output);
        }
        segment.push(tagged);
    }
    flush(&mut segment, &mut output);
    output
}

fn canonical_compose(input: Vec<TaggedScalar>) -> Vec<TaggedScalar> {
    let mut output: Vec<TaggedScalar> = Vec::with_capacity(input.len());
    let mut starter = None;
    let mut last_class = 0_u8;

    for tagged in input {
        let class = canonical_combining_class(tagged.ch);
        let composite = starter.and_then(|at: usize| {
            (last_class < class || last_class == 0)
                .then(|| compose(output[at].ch, tagged.ch))
                .flatten()
                .map(|ch| (at, ch))
        });
        if let Some((at, ch)) = composite {
            output[at].ch = ch;
            output[at].source.start = output[at].source.start.min(tagged.source.start);
            output[at].source.end = output[at].source.end.max(tagged.source.end);
            continue;
        }

        if class == 0 {
            starter = Some(output.len());
        }
        last_class = class;
        output.push(tagged);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_normalization::UnicodeNormalization;

    fn oracle(raw: &str) -> String {
        raw.to_lowercase()
            .nfkc()
            .nfd()
            .filter(|ch| !is_mn(*ch))
            .nfc()
            .collect()
    }

    fn mapped_span(haystack: &MappedFold, needle: &str) -> Option<Range<usize>> {
        let haystack_chars: Vec<char> = haystack.text.chars().collect();
        let needle_chars: Vec<char> = needle.chars().collect();
        let at = haystack_chars
            .windows(needle_chars.len())
            .position(|window| window == needle_chars)?;
        let sources = &haystack.sources[at..at + needle_chars.len()];
        Some(
            sources.iter().map(|source| source.start).min()?
                ..sources.iter().map(|source| source.end).max()?,
        )
    }

    #[test]
    fn accepted_a6_fixtures_equal_the_whole_string_oracle_and_map_utf16() {
        let fixtures = [
            ("ofﬁce", "office", Some(0..5)),
            ("😀aﬁx tail", "f", Some(3..4)),
            ("😀aﬁx tail", "i", Some(3..4)),
            ("😀Ｔｉｎｅ tail", "tine", Some(2..6)),
            ("prefix ㄱㅏ suffix", "가", Some(7..9)),
            ("prefix ㄱ\u{301}ㅏ suffix", "가", Some(7..10)),
            ("a\u{302e}\u{034f}\u{1715}", "a\u{1715}\u{302e}", Some(0..4)),
            (
                "😀a\u{302e}\u{034f}\u{1715}z",
                "\u{1715}\u{302e}",
                Some(3..6),
            ),
            ("Ｔｉｎｅ", "Tine", Some(0..4)),
            ("ｶﾞｲﾄﾞ", "ガイド", Some(0..5)),
            ("Příliš žluťoučký kůň", "prilis zlutoucky kun", Some(0..20)),
            ("Cafe\u{301}", "café", Some(0..5)),
            ("한글", "한글", Some(0..6)),
            ("Kelvin", "kelvin", Some(0..6)),
            ("İstanbul", "istanbul", Some(0..8)),
            ("ıstanbul", "istanbul", None),
            ("ΟΣ Σ", "ος σ", Some(0..4)),
            ("Straße", "STRASSE", None),
            ("豈", "豈", Some(0..1)),
            ("a\u{301}", "a", Some(0..2)),
            ("का", "का", Some(0..2)),
            ("a⃝", "a⃝", Some(0..2)),
        ];

        for (raw, needle, expected_span) in fixtures {
            let mapped = fold(raw);
            let folded_needle = oracle(needle);
            assert_eq!(mapped.text, oracle(raw), "raw={raw:?}");
            assert_eq!(
                mapped_span(&mapped, &folded_needle),
                expected_span,
                "raw={raw:?} needle={needle:?}"
            );
        }
    }

    #[test]
    fn removal_work_is_linear_for_accents_and_one_large_grapheme() {
        for repetitions in [100_usize, 1_000, 10_000] {
            let raw = "a\u{301}".repeat(repetitions);
            let (mapped, work) = fold_with_removal_work(&raw);
            assert!(work.work_units <= work.input_scalars * 4);
            assert_eq!(mapped.text, "a".repeat(repetitions));
        }

        let repetitions = 10_000;
        let mut raw = String::from("a");
        for _ in 0..repetitions {
            raw.push('\u{1715}');
            raw.push('\u{034f}');
        }
        assert_eq!(raw.graphemes(true).count(), 1);
        let (mapped, work) = fold_with_removal_work(&raw);
        assert!(work.work_units <= work.input_scalars * 4);
        assert_eq!(mapped.text, format!("a{}", "\u{1715}".repeat(repetitions)));
    }

    #[test]
    fn fold_is_not_mistaken_for_full_casefold_or_an_idempotent_transform() {
        assert_ne!(fold("Straße").text, fold("STRASSE").text);
        assert_eq!(fold("𝐀").text, "A");
        assert_eq!(fold(&fold("𝐀").text).text, "a");
    }
}
