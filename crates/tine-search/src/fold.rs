use regex::Regex;
use std::ops::Range;
use std::sync::OnceLock;
use unicode_normalization::char::{canonical_combining_class, compose, decompose_compatible};
use unicode_segmentation::UnicodeSegmentation;

/// A6-folded text and one raw UTF-16 source range for every output scalar.
#[derive(Clone, Debug)]
pub struct MappedFold {
    pub text: String,
    pub sources: Vec<Range<usize>>,
}

struct FoldBuffer {
    chars: Vec<char>,
    sources: Option<Vec<Range<usize>>>,
    clusters: Option<Vec<usize>>,
}

impl FoldBuffer {
    fn new(with_provenance: bool, capacity: usize) -> Self {
        Self {
            chars: Vec::with_capacity(capacity),
            sources: with_provenance.then(|| Vec::with_capacity(capacity)),
            clusters: with_provenance.then(|| Vec::with_capacity(capacity)),
        }
    }
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
struct RemovalWork {
    input_scalars: usize,
    work_units: usize,
    raw_graphemes: usize,
    provenance_scalars: usize,
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
/// step, and composition unions all contributors. The provenance sidecars are
/// optional, so text-only callers use the same normalization stages without
/// segmenting raw graphemes or constructing source-map state.
pub(crate) fn fold(raw: &str) -> MappedFold {
    fold_with_removal_work(raw).0
}

pub(crate) fn fold_text(raw: &str) -> String {
    fold_text_with_removal_work(raw).0
}

fn fold_with_removal_work(raw: &str) -> (MappedFold, RemovalWork) {
    let (text, sources, work) = fold_pipeline(raw, true);
    (
        MappedFold {
            text,
            sources: sources.expect("mapped fold requests provenance"),
        },
        work,
    )
}

fn fold_text_with_removal_work(raw: &str) -> (String, RemovalWork) {
    let (text, sources, work) = fold_pipeline(raw, false);
    debug_assert!(sources.is_none());
    (text, work)
}

fn fold_pipeline(
    raw: &str,
    with_provenance: bool,
) -> (String, Option<Vec<Range<usize>>>, RemovalWork) {
    let lowered = raw.to_lowercase();
    let mut decomposed = FoldBuffer::new(with_provenance, lowered.chars().count());
    let raw_graphemes = if with_provenance {
        let mut lowered_scalars = lowered.chars();
        let mut original_utf16 = 0;
        let mut raw_graphemes = 0;
        let sources = decomposed
            .sources
            .as_mut()
            .expect("mapped fold has source storage");
        let clusters = decomposed
            .clusters
            .as_mut()
            .expect("mapped fold has cluster storage");

        for (cluster_index, cluster) in raw.graphemes(true).enumerate() {
            raw_graphemes = cluster_index + 1;
            for original in cluster.chars() {
                let scalar_start = original_utf16;
                original_utf16 += original.len_utf16();
                for _ in original.to_lowercase() {
                    let contextual = lowered_scalars
                        .next()
                        .expect("whole-string lowercase preserves scalar partition length");
                    decompose_compatible(contextual, |ch| {
                        decomposed.chars.push(ch);
                        sources.push(scalar_start..original_utf16);
                        clusters.push(cluster_index);
                    });
                }
            }
        }
        assert!(
            lowered_scalars.next().is_none(),
            "whole-string lowercase preserves scalar partition length"
        );
        raw_graphemes
    } else {
        for contextual in lowered.chars() {
            decompose_compatible(contextual, |ch| decomposed.chars.push(ch));
        }
        0
    };

    let (retained, mut removal_work) = remove_mn(decomposed);
    removal_work.raw_graphemes = raw_graphemes;
    removal_work.provenance_scalars = if with_provenance {
        removal_work.input_scalars
    } else {
        0
    };
    let composed = canonical_compose(canonical_reorder(retained));
    let text = composed.chars.into_iter().collect();
    (text, composed.sources, removal_work)
}

fn source_distance(mark: &Range<usize>, retained: &Range<usize>) -> (usize, bool) {
    if retained.end <= mark.start {
        (mark.start - retained.end, false)
    } else if mark.end <= retained.start {
        (retained.start - mark.end, true)
    } else {
        (0, false)
    }
}

fn remove_mn(mut input: FoldBuffer) -> (FoldBuffer, RemovalWork) {
    let input_scalars = input.chars.len();
    if input.sources.is_none() {
        input.chars.retain(|ch| !is_mn(*ch));
        return (
            input,
            RemovalWork {
                input_scalars,
                work_units: input_scalars,
                raw_graphemes: 0,
                provenance_scalars: 0,
            },
        );
    }

    let sources = input.sources.take().expect("mapped fold has sources");
    let clusters = input.clusters.take().expect("mapped fold has clusters");
    let mut work_units = 0;

    // Tags are still in raw source order. The closest retained contributor in
    // the same grapheme is therefore one of the two retained neighbors. Delay
    // range unions so attaching one mark cannot change a later distance/tie.
    let mut removed = vec![false; input_scalars];
    let mut previous = vec![None; input_scalars];
    let mut previous_retained = None;
    for (at, ch) in input.chars.iter().enumerate() {
        work_units += 1;
        if previous_retained.is_some_and(|before: usize| clusters[before] != clusters[at]) {
            previous_retained = None;
        }
        previous[at] = previous_retained;
        removed[at] = is_mn(*ch);
        if !removed[at] {
            previous_retained = Some(at);
        }
    }

    let mut next = vec![None; input_scalars];
    let mut next_retained = None;
    for at in (0..input_scalars).rev() {
        work_units += 1;
        if next_retained.is_some_and(|after: usize| clusters[after] != clusters[at]) {
            next_retained = None;
        }
        next[at] = next_retained;
        if !removed[at] {
            next_retained = Some(at);
        }
    }

    let mut provenance: Vec<Option<Range<usize>>> = vec![None; input_scalars];
    for (at, mark) in sources.iter().enumerate() {
        work_units += 1;
        if !removed[at] {
            continue;
        }
        let nearest = match (previous[at], next[at]) {
            (Some(before), Some(after)) => {
                if source_distance(mark, &sources[before]) <= source_distance(mark, &sources[after])
                {
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
            let span = provenance[nearest].get_or_insert_with(|| sources[nearest].clone());
            span.start = span.start.min(mark.start);
            span.end = span.end.max(mark.end);
        }
    }

    let mut retained = FoldBuffer {
        chars: Vec::with_capacity(input_scalars),
        sources: Some(Vec::with_capacity(input_scalars)),
        clusters: None,
    };
    for (at, ch) in input.chars.into_iter().enumerate() {
        work_units += 1;
        if removed[at] {
            continue;
        }
        let mut source = sources[at].clone();
        if let Some(span) = provenance[at].take() {
            source.start = source.start.min(span.start);
            source.end = source.end.max(span.end);
        }
        retained.chars.push(ch);
        retained
            .sources
            .as_mut()
            .expect("mapped fold has retained sources")
            .push(source);
    }
    (
        retained,
        RemovalWork {
            input_scalars,
            work_units,
            raw_graphemes: 0,
            provenance_scalars: input_scalars,
        },
    )
}

fn canonical_reorder(input: FoldBuffer) -> FoldBuffer {
    fn push_index(input: &FoldBuffer, at: usize, output: &mut FoldBuffer) {
        output.chars.push(input.chars[at]);
        if let (Some(input_sources), Some(output_sources)) =
            (input.sources.as_ref(), output.sources.as_mut())
        {
            output_sources.push(input_sources[at].clone());
        }
    }

    fn flush(
        input: &FoldBuffer,
        start: usize,
        end: usize,
        order: &mut Vec<usize>,
        output: &mut FoldBuffer,
    ) {
        if end - start <= 1 {
            if start < end {
                push_index(input, start, output);
            }
            return;
        }

        // Stable counting sort by canonical combining class. CCC is one byte,
        // so this is linear with a fixed-size table.
        let mut counts = [0_usize; 256];
        for ch in &input.chars[start..end] {
            counts[canonical_combining_class(*ch) as usize] += 1;
        }
        let mut positions = [0_usize; 256];
        for class in 1..positions.len() {
            positions[class] = positions[class - 1] + counts[class - 1];
        }
        order.clear();
        order.resize(end - start, 0);
        for input_at in start..end {
            let class = canonical_combining_class(input.chars[input_at]) as usize;
            let output_at = positions[class];
            order[output_at] = input_at;
            positions[class] += 1;
        }
        for at in order.iter().copied() {
            push_index(input, at, output);
        }
    }

    let mut output = FoldBuffer {
        chars: Vec::with_capacity(input.chars.len()),
        sources: input
            .sources
            .as_ref()
            .map(|_| Vec::with_capacity(input.chars.len())),
        clusters: None,
    };
    let mut order = Vec::new();
    let mut segment_start = 0;
    for at in 1..input.chars.len() {
        if canonical_combining_class(input.chars[at]) == 0 {
            flush(&input, segment_start, at, &mut order, &mut output);
            segment_start = at;
        }
    }
    flush(
        &input,
        segment_start,
        input.chars.len(),
        &mut order,
        &mut output,
    );
    output
}

fn canonical_compose(input: FoldBuffer) -> FoldBuffer {
    let mut output = FoldBuffer {
        chars: Vec::with_capacity(input.chars.len()),
        sources: input
            .sources
            .as_ref()
            .map(|_| Vec::with_capacity(input.chars.len())),
        clusters: None,
    };
    let mut starter = None;
    let mut last_class = 0_u8;

    for (input_at, ch) in input.chars.into_iter().enumerate() {
        let class = canonical_combining_class(ch);
        let composed = starter
            .filter(|_| last_class < class || last_class == 0)
            .and_then(|at: usize| compose(output.chars[at], ch));
        let composite = starter.zip(composed);
        if let Some((at, ch)) = composite {
            output.chars[at] = ch;
            if let (Some(input_sources), Some(output_sources)) =
                (input.sources.as_ref(), output.sources.as_mut())
            {
                output_sources[at].start =
                    output_sources[at].start.min(input_sources[input_at].start);
                output_sources[at].end = output_sources[at].end.max(input_sources[input_at].end);
            }
            continue;
        }

        if class == 0 {
            starter = Some(output.chars.len());
        }
        last_class = class;
        output.chars.push(ch);
        if let (Some(input_sources), Some(output_sources)) =
            (input.sources.as_ref(), output.sources.as_mut())
        {
            output_sources.push(input_sources[input_at].clone());
        }
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
    fn text_only_and_mapped_text_share_broad_fold_semantics() {
        let mut fixtures = vec![
            String::new(),
            "Project Alpha 2026 / Planning".to_owned(),
            "東京計画／開発ノート頁面検索".to_owned(),
            "Ｐｒｏｊｅｃｔ ｶﾞｲﾄﾞ 豈".to_owned(),
            "Příliš žluťoučký kůň".to_owned(),
            "Cafe\u{301} déjà vu İstanbul".to_owned(),
            "한글 한글 ㄱㅏ".to_owned(),
            "ΟΣ Σ ΟΣΑ ΟΣ.".to_owned(),
            "ofﬁce Straße 𝐀 Kelvin".to_owned(),
            "\u{301}\u{342}a\u{315}\u{300}z".to_owned(),
            "\u{301}\u{342}".to_owned(),
            "का a⃝ 😀".to_owned(),
        ];
        fixtures.push(format!("a{}", "\u{301}\u{034f}\u{1715}".repeat(1_000)));

        for raw in fixtures {
            let text_only = fold_text(&raw);
            let mapped = fold(&raw);
            assert_eq!(text_only, mapped.text, "raw={raw:?}");
            assert_eq!(text_only, oracle(&raw), "raw={raw:?}");
        }
    }

    #[test]
    fn text_only_pipeline_constructs_no_mapping_state() {
        let raw = format!(
            "Příliš İstanbul 한글 ΟΣ Σ a{}",
            "\u{301}\u{034f}".repeat(1_000)
        );
        let (text, sources, text_work) = fold_pipeline(&raw, false);
        let (mapped, mapped_work) = fold_with_removal_work(&raw);

        assert_eq!(text, mapped.text);
        assert!(sources.is_none());
        assert_eq!(text_work.raw_graphemes, 0);
        assert_eq!(text_work.provenance_scalars, 0);
        assert_eq!(text_work.work_units, text_work.input_scalars);
        assert!(mapped_work.raw_graphemes > 0);
        assert_eq!(mapped_work.provenance_scalars, mapped_work.input_scalars);
        assert_eq!(mapped.sources.len(), mapped.text.chars().count());
    }

    #[test]
    fn fold_is_not_mistaken_for_full_casefold_or_an_idempotent_transform() {
        assert_ne!(fold("Straße").text, fold("STRASSE").text);
        assert_eq!(fold("𝐀").text, "A");
        assert_eq!(fold(&fold("𝐀").text).text, "a");
    }
}
