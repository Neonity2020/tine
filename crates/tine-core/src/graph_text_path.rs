//! Lexically safe graph-relative text paths and their portable identity.
//!
//! These types were the storage-neutral core of the retired Managed Storage
//! receipt module (`oplog/receipt.rs`, removed 2026-09-15). Direct Files uses
//! them for one question only: is this graph-relative path a safe Markdown/Org
//! text path, and which other spellings share its portable identity (case and
//! Unicode normalization) on a filesystem that may fold them together? They
//! grant no filesystem authority; whether a path is admitted is decided by the
//! graph capability and [`crate::graph_text_scope`].

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use caseless::Caseless;

pub const PORTABLE_PATH_KEY_VERSION: u32 = 1;
pub const PORTABLE_PATH_NORMALIZATION_UNICODE_VERSION: (u8, u8, u8) = (17, 0, 0);
pub const PORTABLE_PATH_CASE_FOLD_UNICODE_VERSION: (u64, u64, u64) = (16, 0, 0);

const _: () = {
    let (major, minor, patch) = unicode_normalization::UNICODE_VERSION;
    assert!(major == 17 && minor == 0 && patch == 0);
};
const _: () = {
    let (major, minor, patch) = caseless::UNICODE_VERSION;
    assert!(major == 16 && minor == 0 && patch == 0);
};

/// A graph-relative spelling that is not a lexically safe text path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsafeGraphTextPath(pub String);

impl fmt::Display for UnsafeGraphTextPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unsafe graph text path: {}", self.0)
    }
}

impl std::error::Error for UnsafeGraphTextPath {}

/// A lexically safe graph-relative Markdown/Org text path.
///
/// This type establishes portable lexical safety only. Whether an existing path
/// is admitted is authorized by the graph capability and its graph-text scope.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GraphTextPath(String);

impl GraphTextPath {
    pub fn parse(value: impl Into<String>) -> Result<Self, UnsafeGraphTextPath> {
        let value = value.into();
        if is_graph_text_path(&value) {
            Ok(Self(value))
        } else {
            Err(UnsafeGraphTextPath(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn file_name(&self) -> &str {
        self.0
            .rsplit('/')
            .next()
            .expect("validated graph paths are nonempty")
    }

    pub fn parent_relative(&self) -> Option<&str> {
        self.0.rsplit_once('/').map(|(parent, _)| parent)
    }

    /// Form a graph-relative sibling without granting filesystem authority.
    pub fn join_sibling(&self, name: &str) -> Result<String, UnsafeGraphTextPath> {
        if name.contains('/') || !graph_text_component_is_portable(name) {
            return Err(UnsafeGraphTextPath(name.to_owned()));
        }
        Ok(match self.parent_relative() {
            Some(parent) => format!("{parent}/{name}"),
            None => name.to_owned(),
        })
    }

    pub fn extension(&self) -> &str {
        self.file_name()
            .rsplit_once('.')
            .map(|(_, extension)| extension)
            .expect("validated graph paths have a text extension")
    }

    pub fn is_markdown(&self) -> bool {
        self.extension().eq_ignore_ascii_case("md")
            || self.extension().eq_ignore_ascii_case("markdown")
    }

    pub fn is_org(&self) -> bool {
        self.extension().eq_ignore_ascii_case("org")
    }

    /// Compute the versioned portable comparison key without changing the
    /// exact spelling retained and projected by this path.
    pub fn portable_key(&self) -> PortablePathKey {
        PortablePathKey::from_graph_text_path(self.as_str())
    }
}

/// Canonical portable comparison bytes. This value is not a projected path.
///
/// Each component uses `NFC(default_case_fold(NFD(component)))`; components
/// are then joined by a literal slash. Compatibility normalization is
/// deliberately excluded.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PortablePathKey(String);

impl PortablePathKey {
    /// Apply the canonical versioned fold to an already-validated graph text
    /// path without introducing a second approximation of the component fold.
    pub(crate) fn from_graph_text_path(path: &str) -> Self {
        Self::from_components(path)
    }

    /// Do two single path components share one portable identity?
    ///
    /// Answers exactly `from_graph_text_path(a) == from_graph_text_path(b)`, but
    /// without building either key when both sides are ASCII. That case is the
    /// hot one: the per-save alias scan compares every entry of the target's
    /// parent directory against one requested component, and each comparison
    /// allocated a `String` and ran NFD → default case fold → NFC over the name.
    /// On a flat `journals/` directory that is the whole cost of a save.
    /// (Direct Files performance audit 2026-08-09, finding F6.)
    ///
    /// Correct because on ASCII input NFD and NFC are the identity and default
    /// case folding is exactly ASCII lowercasing. The guard demands ASCII on
    /// BOTH sides, which is not redundant: folding can turn non-ASCII into
    /// ASCII (U+212A KELVIN SIGN folds to `k`, U+FB01 LATIN SMALL LIGATURE FI to
    /// `fi`), so a non-ASCII name can legitimately match an ASCII one and must
    /// still go through the full fold.
    #[cfg(test)]
    pub(crate) fn graph_text_components_match(a: &str, b: &str) -> bool {
        Self::graph_text_component_matches(a, b, Self::graph_text_component_probe(b).as_ref())
    }

    /// The fold of the ONE component a directory scan compares every entry
    /// against, computed once. `None` means "ASCII", which is the case the fast
    /// path serves without a key at all.
    ///
    /// This exists because the scan is a loop: folding the unchanged requested
    /// component per entry measured 1.79-1.88x SLOWER than the pre-fast-path
    /// code whenever that component is non-ASCII — which for a graph written in
    /// a language with diacritics is the ordinary case, not the exotic one.
    /// (F6 verification, medium follow-up.)
    pub(crate) fn graph_text_component_probe(component: &str) -> Option<PortablePathKey> {
        (!component.is_ascii()).then(|| Self::from_graph_text_path(component))
    }

    /// Does `name` share `component`'s portable identity, given the `probe`
    /// `graph_text_component_probe(component)` returned?
    pub(crate) fn graph_text_component_matches(
        name: &str,
        component: &str,
        probe: Option<&PortablePathKey>,
    ) -> bool {
        // `probe` is None exactly when `component` is ASCII, so this is the
        // both-sides-ASCII case and needs no key on either side.
        if probe.is_none() && name.is_ascii() {
            return name.eq_ignore_ascii_case(component);
        }
        let name_key = Self::from_graph_text_path(name);
        match probe {
            Some(key) => name_key == *key,
            None => name_key == Self::from_graph_text_path(component),
        }
    }

    fn from_components(path: &str) -> Self {
        let mut key = String::with_capacity(path.len());
        for (index, component) in path.split('/').enumerate() {
            if index != 0 {
                key.push('/');
            }
            key.extend(component.chars().nfd().default_case_fold().nfc());
        }
        Self(key)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub fn digest(&self) -> PortablePathKeyDigest {
        let mut hasher = Sha256::new();
        hasher.update(b"tine/portable-path-key/v1\0");
        hasher.update(PORTABLE_PATH_KEY_VERSION.to_be_bytes());
        hasher.update((self.0.len() as u64).to_be_bytes());
        hasher.update(self.0.as_bytes());
        PortablePathKeyDigest(hasher.finalize().into())
    }
}

/// Domain-separated digest of a portable path key.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PortablePathKeyDigest([u8; 32]);

impl PortablePathKeyDigest {
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for GraphTextPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for GraphTextPath {
    type Err = UnsafeGraphTextPath;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for GraphTextPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for GraphTextPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

fn is_graph_text_path(value: &str) -> bool {
    if value.is_empty() || value != value.trim() || value.starts_with('/') || value.contains('\\') {
        return false;
    }
    let segments: Vec<_> = value.split('/').collect();
    if segments
        .iter()
        .any(|part| !graph_text_component_is_portable(part))
    {
        return false;
    }
    segments
        .last()
        .and_then(|name| name.rsplit_once('.'))
        .filter(|(stem, _)| !stem.is_empty())
        .map(|(_, extension)| {
            extension.eq_ignore_ascii_case("md")
                || extension.eq_ignore_ascii_case("markdown")
                || extension.eq_ignore_ascii_case("org")
        })
        .unwrap_or(false)
}

pub(crate) fn graph_text_component_is_portable(component: &str) -> bool {
    if component.is_empty()
        || matches!(component, "." | "..")
        || component.ends_with(' ')
        || component.ends_with('.')
        || component.chars().any(is_forbidden_win32_path_character)
    {
        return false;
    }
    let device_stem = component
        .split_once('.')
        .map_or(component, |(stem, _)| stem)
        .to_ascii_uppercase();
    !matches!(
        device_stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
            | "COM¹"
            | "COM²"
            | "COM³"
            | "LPT¹"
            | "LPT²"
            | "LPT³"
    )
}

fn is_forbidden_win32_path_character(character: char) -> bool {
    character == '\0'
        || character.is_control()
        || matches!(character, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*')
}

/// Whether a graph text path lives under the configured pages root or the
/// configured journals root. Decided by the graph capability's configured
/// roots, never inferred from a basename or a normalized spelling.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphTextKind {
    Page,
    Journal,
}

/// SHA-256 and exact byte length of an immutable blob.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BlobDescription {
    sha256: [u8; 32],
    byte_length: u64,
}

impl BlobDescription {
    pub fn of(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        let mut sha256 = [0_u8; 32];
        sha256.copy_from_slice(&digest);
        Self {
            sha256,
            byte_length: bytes.len() as u64,
        }
    }

    pub const fn from_parts(sha256: [u8; 32], byte_length: u64) -> Self {
        Self {
            sha256,
            byte_length,
        }
    }

    pub const fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    pub const fn byte_length(self) -> u64 {
        self.byte_length
    }
}

impl fmt::Debug for BlobDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlobDescription")
            .field("sha256", &DigestDisplay(&self.sha256))
            .field("byte_length", &self.byte_length)
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlobDescriptionWire {
    sha256: String,
    byte_length: u64,
}

impl Serialize for BlobDescription {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        BlobDescriptionWire {
            sha256: DigestDisplay(&self.sha256).to_string(),
            byte_length: self.byte_length,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BlobDescription {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BlobDescriptionWire::deserialize(deserializer)?;
        let sha256 = parse_digest(&wire.sha256).map_err(serde::de::Error::custom)?;
        Ok(Self::from_parts(sha256, wire.byte_length))
    }
}

struct DigestDisplay<'a>(&'a [u8]);

impl fmt::Display for DigestDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(self.0, f)
    }
}

impl fmt::Debug for DigestDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Stable identity of one canonical graph-root filesystem resource.
///
/// The digest is derived only from a retained no-follow directory capability:
/// device/inode on Unix (including Android) and volume/file ID on Windows.
/// Ambient path strings never enter this identity, so moving or renaming the
/// graph preserves the graph-text scope binding while substituting another
/// directory does not.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalGraphResourceId([u8; 32]);

impl CanonicalGraphResourceId {
    pub(crate) fn from_capability_identity(platform: &[u8], identity: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"tine/canonical-graph-resource/v1\0");
        hasher.update((platform.len() as u64).to_be_bytes());
        hasher.update(platform);
        hasher.update((identity.len() as u64).to_be_bytes());
        hasher.update(identity);
        Self(hasher.finalize().into())
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for CanonicalGraphResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CanonicalGraphResourceId({self})")
    }
}

impl fmt::Display for CanonicalGraphResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(&self.0, f)
    }
}

impl FromStr for CanonicalGraphResourceId {
    type Err = DigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_digest(value).map(Self)
    }
}

impl Serialize for CanonicalGraphResourceId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CanonicalGraphResourceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DigestParseError;

impl fmt::Display for DigestParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected exactly 64 lower-case hexadecimal characters")
    }
}

impl std::error::Error for DigestParseError {}

pub(crate) fn parse_digest(value: &str) -> Result<[u8; 32], DigestParseError> {
    if value.len() != 64
        || value
            .bytes()
            .any(|byte| !matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(DigestParseError);
    }
    let mut result = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        result[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
    }
    Ok(result)
}

fn hex_nibble(value: u8) -> u8 {
    match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => value - b'a' + 10,
        _ => unreachable!("validated hexadecimal nibble"),
    }
}

pub(crate) fn write_hex(bytes: &[u8], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        f.write_str(
            std::str::from_utf8(&[HEX[(byte >> 4) as usize], HEX[(byte & 0x0f) as usize]])
                .expect("hexadecimal is UTF-8"),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ASCII fast path in `graph_text_components_match` is an optimisation of
    /// a case/NFC IDENTITY boundary — two files sharing one portable identity is
    /// a refusal, so a fast path that says "different" where the fold says "same"
    /// would admit exactly the collision the check exists to stop. It must agree
    /// with building both keys on every input, including the ones that make the
    /// both-sides-ASCII guard load-bearing: folding can produce ASCII from
    /// non-ASCII. (Direct Files performance audit 2026-08-09, finding F6.)
    /// The probe form is what the scan actually calls, so it has to agree with
    /// the two-argument form on every input — including the asymmetric cases,
    /// since the probe is built from ONE side.
    #[test]
    fn the_probe_form_agrees_with_the_full_fold_in_both_directions() {
        let names = [
            "Journals",
            "journals",
            "Ärger.md",
            "A\u{0308}rger.md",
            "ärger.md",
            "\u{212A}elvin.md",
            "kelvin.md",
            "\u{FB01}le.md",
            "file.md",
            "Стр.md",
            "стр.md",
            "Stra\u{00DF}e.md",
            "strasse.md",
            "",
        ];
        for component in names {
            let probe = PortablePathKey::graph_text_component_probe(component);
            assert_eq!(
                probe.is_none(),
                component.is_ascii(),
                "the probe must be absent exactly for an ASCII component: {component:?}"
            );
            for name in names {
                let folded = PortablePathKey::from_graph_text_path(name)
                    == PortablePathKey::from_graph_text_path(component);
                assert_eq!(
                    PortablePathKey::graph_text_component_matches(name, component, probe.as_ref()),
                    folded,
                    "{name:?} vs {component:?}"
                );
            }
        }
    }

    #[test]
    fn the_ascii_component_fast_path_agrees_with_the_full_fold() {
        let names = [
            "Journals",
            "journals",
            "JOURNALS",
            "2026_08_09.md",
            "2026_08_09.MD",
            "Ärger.md",         // NFC precomposed
            "A\u{0308}rger.md", // NFD, same page
            "ärger.md",
            "STRASSE.md",
            "strasse.md",
            "Stra\u{00DF}e.md", // ß folds to ss
            "\u{212A}elvin.md", // KELVIN SIGN folds to ASCII k
            "kelvin.md",
            "Kelvin.md",
            "\u{FB01}le.md", // ligature fi folds to ASCII fi
            "file.md",
            "FILE.md",
            "Стр.md",
            "стр.md",
            "",
        ];
        for a in names {
            for b in names {
                let folded = PortablePathKey::from_graph_text_path(a)
                    == PortablePathKey::from_graph_text_path(b);
                assert_eq!(
                    PortablePathKey::graph_text_components_match(a, b),
                    folded,
                    "{a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn graph_text_path_accepts_graph_relative_text_formats_and_preserves_spelling() {
        for path in [
            "Root.md",
            "Root.MD",
            "Mixed.MarkDown",
            "Outline.ORG",
            "nested/Page.md",
            "deep/nested/Page.markdown",
            "deep/nested/Page.Org",
        ] {
            assert_eq!(GraphTextPath::parse(path).unwrap().as_str(), path);
        }
    }

    #[test]
    fn graph_text_path_rejects_unsafe_components_extensions_and_empty_stems() {
        for path in [
            "",
            ".md",
            ".MARKDOWN",
            ".org",
            "root.txt",
            "root.md ",
            "/root.md",
            "../root.md",
            "nested/../root.md",
            "nested//root.md",
            "nested/root.",
            "nested/CON.md",
        ] {
            assert!(GraphTextPath::parse(path).is_err(), "{path}");
        }
    }

    // A comment in model.rs once claimed GraphTextPath "deliberately rejects
    // hidden graph-text names" — it does not, and that claim was load-bearing
    // for a storage design decision (whether a hidden dotfile could ever be a
    // graph-text file). Architectural facts live in tests, not comments: this
    // one pins the actual behaviour.
    #[test]
    fn graph_text_path_accepts_leading_dot_name() {
        for path in [".tine-favorites.md", "logseq/.hidden.md", ".a.org"] {
            assert!(
                GraphTextPath::parse(path).is_ok(),
                "leading-dot names are accepted: {path}"
            );
        }
        // What IS rejected is an EMPTY stem — ".md" has no name before the
        // extension. That is the distinction the old comment blurred.
        assert!(GraphTextPath::parse(".md").is_err());
    }

    #[test]
    fn graph_text_path_root_safe_helpers_and_portable_key_v1_are_stable() {
        let root = GraphTextPath::parse("Root.MarkDown").unwrap();
        assert_eq!(root.file_name(), "Root.MarkDown");
        assert_eq!(root.parent_relative(), None);
        assert_eq!(
            root.join_sibling(".Root.MarkDown.recovery").unwrap(),
            ".Root.MarkDown.recovery"
        );

        let nested = GraphTextPath::parse("Pages/Cafe\u{301}.MD").unwrap();
        assert_eq!(nested.file_name(), "Cafe\u{301}.MD");
        assert_eq!(nested.parent_relative(), Some("Pages"));
        assert_eq!(
            nested.join_sibling(".Cafe\u{301}.MD.recovery").unwrap(),
            "Pages/.Cafe\u{301}.MD.recovery"
        );
        assert_eq!(nested.portable_key().as_str(), "pages/café.md");
        assert_eq!(nested.portable_key().as_bytes(), "pages/café.md".as_bytes());

        for unsafe_name in ["", ".", "..", "nested/name", r"nested\name", "CON"] {
            assert!(root.join_sibling(unsafe_name).is_err(), "{unsafe_name}");
        }
    }

    #[test]
    fn blob_description_round_trips_through_serde_and_parses_only_lowercase_hex() {
        let description = BlobDescription::of(b"hello");
        let encoded = serde_json::to_string(&description).unwrap();
        let decoded: BlobDescription = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, description);
        assert_eq!(decoded.byte_length(), 5);
        assert!(
            serde_json::from_str::<BlobDescription>(r#"{"sha256":"ABCD","byte_length":5}"#)
                .is_err()
        );
    }
}
