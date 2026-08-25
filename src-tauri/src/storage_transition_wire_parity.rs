//! The storage-transition receipt is one wire format declared twice.
//!
//! `StorageTransitionKind`, `StorageTransitionPhase` and
//! `StorageTransitionOutcome` are serialized straight onto the event the
//! frontend renders, and `src/types.ts` re-declares each of them as a string
//! union. Adding a phase in Rust and forgetting the union does not fail
//! anything: the event still arrives, TypeScript narrows it to a type it does
//! not know, and the UI shows nothing for the state the user is actually in.
//! Removing one on the TS side is just as quiet in the other direction.
//!
//! This module reads both declarations and compares the variant SETS, with the
//! Rust names put through the same `snake_case` rename serde applies. It also
//! asserts the rename attribute is still there, so the comparison cannot become
//! a lie by someone dropping `#[serde(rename_all = "snake_case")]`.

#[cfg(test)]
use std::collections::BTreeSet;

#[cfg(test)]
const SUPERVISOR_RS: &str = include_str!("storage_mode_supervisor.rs");

#[cfg(test)]
const TYPES_TS: &str = include_str!("../../src/types.ts");

/// Variant names of a Rust enum, already rename-mapped to the strings serde
/// puts on the wire. Panics unless the enum still carries
/// `#[serde(rename_all = "snake_case")]` — the mapping below is only correct
/// while it does.
#[cfg(test)]
fn serialized_enum_variants(source: &str, name: &str) -> BTreeSet<String> {
    let header = format!("enum {name} {{");
    let start = source
        .find(&header)
        .unwrap_or_else(|| panic!("`{header}` not found -- the enum was renamed or moved"));
    let attributes = &source[start.saturating_sub(200)..start];
    assert!(
        attributes.contains(r#"#[serde(rename_all = "snake_case")]"#),
        "`{name}` no longer carries #[serde(rename_all = \"snake_case\")]; the wire strings \
         changed and this parity check would silently compare the wrong names"
    );

    let body_start = start + header.len();
    let end = source[body_start..]
        .find("\n}")
        .unwrap_or_else(|| panic!("unterminated `{name}` enum body"))
        + body_start;

    let mut variants = BTreeSet::new();
    for line in source[body_start..end].lines() {
        let line = line.trim().trim_end_matches(',');
        if line.is_empty() || line.starts_with("//") || line.starts_with("#[") {
            continue;
        }
        assert!(
            line.chars().all(|c| c.is_ascii_alphanumeric()),
            "`{name}` variant `{line}` is not a plain unit variant; this scan only understands \
             fieldless enums"
        );
        variants.insert(to_snake_case(line));
    }
    assert!(!variants.is_empty(), "scanned no variants out of `{name}`");
    variants
}

/// serde's `snake_case` rename: lowercase everything, `_` before each uppercase
/// letter that is not the first.
#[cfg(test)]
fn to_snake_case(variant: &str) -> String {
    let mut out = String::with_capacity(variant.len() + 4);
    for (index, c) in variant.chars().enumerate() {
        if c.is_ascii_uppercase() && index > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// Members of a named TypeScript string union, `export type NAME = "a" | "b";`.
#[cfg(test)]
fn ts_named_union(source: &str, name: &str) -> BTreeSet<String> {
    let header = format!("export type {name} =");
    let start = source
        .find(&header)
        .unwrap_or_else(|| panic!("`{header}` not found in src/types.ts"));
    let body_start = start + header.len();
    let end = source[body_start..]
        .find(';')
        .unwrap_or_else(|| panic!("unterminated `{name}` union in src/types.ts"))
        + body_start;
    string_literals(&source[body_start..end], name)
}

/// Members of an INLINE string union written as one property of an interface,
/// e.g. `outcome?: "succeeded" | "failed";`. The Rust `StorageTransitionOutcome`
/// has no named TS counterpart -- it is spelled out on the event.
#[cfg(test)]
fn ts_inline_union(source: &str, interface: &str, property: &str) -> BTreeSet<String> {
    let header = format!("export interface {interface} {{");
    let start = source
        .find(&header)
        .unwrap_or_else(|| panic!("`{header}` not found in src/types.ts"));
    let body_start = start + header.len();
    let end = source[body_start..]
        .find("\n}")
        .unwrap_or_else(|| panic!("unterminated `{interface}` interface body"))
        + body_start;
    let needle = format!("{property}?:");
    let alternative = format!("{property}:");
    let line = source[body_start..end]
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(&needle) || line.starts_with(&alternative))
        .unwrap_or_else(|| panic!("`{interface}` has no `{property}` property"));
    string_literals(line, property)
}

#[cfg(test)]
fn string_literals(text: &str, label: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut rest = text;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        found.insert(after[..close].to_owned());
        rest = &after[close + 1..];
    }
    assert!(
        !found.is_empty(),
        "scanned no string literals out of `{label}`"
    );
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_same(name: &str, rust: BTreeSet<String>, ts: BTreeSet<String>) {
        let only_rust: Vec<&String> = rust.difference(&ts).collect();
        let only_ts: Vec<&String> = ts.difference(&rust).collect();
        assert!(
            only_rust.is_empty() && only_ts.is_empty(),
            "{name} disagrees across the wire; only in Rust \
             (src-tauri/src/storage_mode_supervisor.rs): {only_rust:?}; only in TypeScript \
             (src/types.ts): {only_ts:?}"
        );
    }

    /// A transition kind the frontend does not know is a receipt it cannot
    /// route.
    #[test]
    fn storage_transition_kind_matches_the_typescript_union() {
        assert_same(
            "StorageTransitionKind",
            serialized_enum_variants(SUPERVISOR_RS, "StorageTransitionKind"),
            ts_named_union(TYPES_TS, "StorageTransitionKind"),
        );
    }

    /// A phase the frontend does not know is a stage of a graph open with no
    /// progress shown for it.
    #[test]
    fn storage_transition_phase_matches_the_typescript_union() {
        assert_same(
            "StorageTransitionPhase",
            serialized_enum_variants(SUPERVISOR_RS, "StorageTransitionPhase"),
            ts_named_union(TYPES_TS, "StorageTransitionPhase"),
        );
    }

    /// An outcome the frontend does not know is a terminal event it cannot
    /// classify as success or failure.
    #[test]
    fn storage_transition_outcome_matches_the_typescript_union() {
        assert_same(
            "StorageTransitionOutcome",
            serialized_enum_variants(SUPERVISOR_RS, "StorageTransitionOutcome"),
            ts_inline_union(TYPES_TS, "StorageTransitionEvent", "outcome"),
        );
    }
}
