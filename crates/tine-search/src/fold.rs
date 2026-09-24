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

/// The nonspacing marks that are default-ignorable: the combining grapheme
/// joiner, the Khmer inherent vowels, and the variation selectors. They only
/// pick a glyph. `the_ignorable_marks_are_every_default_ignorable_mark` pins
/// the list against Unicode.
fn is_ignorable_mark(ch: char) -> bool {
    matches!(
        ch,
        '\u{034f}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180d}'
            | '\u{180f}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{e0100}'..='\u{e01ef}'
    )
}

/// Combining classes of marks that make a different letter or syllable rather
/// than accent one: kana voicing (8), virama (9), the Telugu length marks (84,
/// 91), the Thai vowel sign u/uu (103), the Lao vowel sign u/uu (118) and the
/// Tibetan vowel signs (129, 130, 132). Firefox's find-in-page keeps the same
/// classes, after Japanese users reported は = ば = ぱ (bug 1624244).
const LETTER_CLASSES: [u8; 9] = [8, 9, 84, 91, 103, 118, 129, 130, 132];

/// Whether `ch` is a Cyrillic letter. Cyrillic marks make letters of their
/// own (й is not и), so they are kept, except that ё searches as е, as
/// Russian commonly writes it.
fn is_cyrillic(ch: char) -> bool {
    matches!(
        ch,
        '\u{0400}'..='\u{052f}'
            | '\u{1c80}'..='\u{1c8f}'
            | '\u{2de0}'..='\u{2dff}'
            | '\u{a640}'..='\u{a69f}'
    )
}

/// Letters with a stroke or slash have no decomposition, so no mark removal
/// reaches them; they search as their base letter (Firefox bug 1649187).
fn strip_stroke(ch: char) -> char {
    match ch {
        'ł' => 'l',
        'ø' => 'o',
        'đ' => 'd',
        'ħ' => 'h',
        'ŧ' => 't',
        other => other,
    }
}

/// Whether the fold drops the decomposed scalar `ch`, whose base is the last
/// scalar before it that is not a mark. An accent drops (é finds e, Hebrew
/// points and Arabic harakat drop); a mark with combining class 0 or one of
/// [`LETTER_CLASSES`] stays, because it makes another letter (Devanagari कु is
/// not क, が is not か). Variation selectors and the combining grapheme joiner
/// only select a glyph and always drop. Decided by Martin 2026-09-24.
fn drops(ch: char, base: Option<char>) -> bool {
    if !is_mn(ch) {
        return false;
    }
    if is_ignorable_mark(ch) {
        return true;
    }
    let class = canonical_combining_class(ch);
    if class == 0 || LETTER_CLASSES.contains(&class) {
        return false;
    }
    match base {
        // ё is е + U+0308 once decomposed.
        Some('\u{0435}') => ch == '\u{0308}',
        Some(base) if is_cyrillic(base) => false,
        _ => true,
    }
}

/// The removal decision for every decomposed scalar, in order.
fn removal_marks(chars: &[char]) -> Vec<bool> {
    let mut base = None;
    chars
        .iter()
        .map(|&ch| {
            let removed = drops(ch, base);
            if canonical_combining_class(ch) == 0 && !is_mn(ch) {
                base = Some(ch);
            }
            removed
        })
        .collect()
}

/// The one native search fold: whole-string lowercase, compatibility
/// decomposition with stroked letters unstroked, accent removal ([`drops`]),
/// canonical reorder, and canonical composition.
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
                        decomposed.chars.push(strip_stroke(ch));
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
            decompose_compatible(contextual, |ch| decomposed.chars.push(strip_stroke(ch)));
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
    let removed = removal_marks(&input.chars);
    if input.sources.is_none() {
        let mut at = 0;
        input.chars.retain(|_| {
            at += 1;
            !removed[at - 1]
        });
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
    let mut previous = vec![None; input_scalars];
    let mut previous_retained = None;
    for at in 0..input_scalars {
        work_units += 1;
        if previous_retained.is_some_and(|before: usize| clusters[before] != clusters[at]) {
            previous_retained = None;
        }
        previous[at] = previous_retained;
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
        let decomposed: Vec<char> = raw.to_lowercase().nfkc().nfd().map(strip_stroke).collect();
        let removed = removal_marks(&decomposed);
        decomposed
            .into_iter()
            .zip(removed)
            .filter_map(|(ch, removed)| (!removed).then_some(ch))
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

    /// The mark policy Martin decided on 2026-09-24: accents fold, marks
    /// that make another letter or syllable do not.
    #[test]
    fn accents_fold_and_letter_making_marks_do_not() {
        let same = [
            ("café", "cafe"),
            ("Příliš žluťoučký kůň", "prilis zlutoucky kun"),
            ("γειά", "γεια"),
            // Hebrew points (ccc 10-26) and Arabic harakat (27-35).
            ("שָׁלוֹם", "שלום"),
            ("مَرْحَبًا", "مرحبا"),
            // Thai tone mark (ccc 107), Lao tone mark (122), Devanagari nukta (7).
            ("ก่า", "กา"),
            ("ກ່າ", "ກາ"),
            ("क़", "क"),
            ("ёлка", "елка"),
            ("ЁЛКА", "елка"),
            ("Łódź", "lodz"),
            ("Øresund", "oresund"),
            ("Đà Nẵng", "da nang"),
            ("Ħal", "hal"),
            ("Ŧ", "t"),
            // Variation selectors and the combining grapheme joiner only pick a glyph.
            ("\u{2764}\u{fe0f}", "\u{2764}"),
            ("a\u{034f}b", "ab"),
        ];
        for (raw, plain) in same {
            assert_eq!(
                fold_text(raw),
                fold_text(plain),
                "{raw:?} should find {plain:?}"
            );
        }

        let different = [
            // Kana voicing (ccc 8).
            ("が", "か"),
            ("ぱ", "は"),
            ("ｶﾞ", "カ"),
            // Virama (ccc 9) and Devanagari vowel signs (ccc 0).
            ("क्", "क"),
            ("कु", "क"),
            ("कि", "क"),
            // Thai sara u (ccc 103), thanthakhat and nikhahit (ccc 0).
            ("กุ", "ก"),
            ("ก์", "ก"),
            ("กํ", "ก"),
            // Lao vowel sign u (ccc 118).
            ("ກຸ", "ກ"),
            // Tibetan vowel signs (ccc 130, 132).
            ("ཀི", "ཀ"),
            ("ཀུ", "ཀ"),
            // Cyrillic letters made with a mark.
            ("й", "и"),
            ("ї", "і"),
            ("ў", "у"),
            ("ѐ", "е"),
        ];
        for (raw, plain) in different {
            assert_ne!(
                fold_text(raw),
                fold_text(plain),
                "{raw:?} must not find {plain:?}"
            );
            assert_eq!(fold(raw).text, fold_text(raw), "raw={raw:?}");
        }
    }

    #[test]
    fn the_ignorable_marks_are_every_default_ignorable_mark() {
        let ignorable = Regex::new(r"\A\p{Default_Ignorable_Code_Point}\z").unwrap();
        for ch in (0..=0x10ffff_u32).filter_map(char::from_u32) {
            let mut bytes = [0_u8; 4];
            let expected = is_mn(ch) && ignorable.is_match(ch.encode_utf8(&mut bytes));
            assert_eq!(is_ignorable_mark(ch), expected, "U+{:04X}", ch as u32);
        }
    }

    #[test]
    fn fold_is_not_mistaken_for_full_casefold_or_an_idempotent_transform() {
        assert_ne!(fold("Straße").text, fold("STRASSE").text);
        assert_eq!(fold("𝐀").text, "A");
        assert_eq!(fold(&fold("𝐀").text).text, "a");
    }
}
