//! Graph-owned writers for `logseq/config.edn`.

use super::*;
use crate::config::{
    edn_str_end, find_keyword, find_keyword_at_map_level, find_top_level_keyword,
    match_close_brace, match_close_bracket, next_value_span, root_map_bounds, skip_blank,
};

/// Serializes ALL config.edn writers so two concurrent setting changes (or one
/// racing a read-modify-write) can't clobber each other (audit M2).
static CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn config_path_for_write(graph: &Graph) -> io::Result<std::path::PathBuf> {
    let path = graph.root.join("logseq").join("config.edn");
    graph.ensure_config_write_target(&path)?;
    Ok(path)
}

impl Graph {
    /// Record which page holds the Favorites arrangement, as
    /// `:tine/favorites-page "Name"`. Logseq ignores unknown keys, so this is
    /// invisible to it; `:favorites` remains the shared membership list.
    ///
    /// Surgical and key-local like `set_favorites`: unknown keys, comments and
    /// formatting elsewhere in the file survive untouched. The existing value is
    /// located with the comment/string-aware `find_keyword` and replaced only
    /// when it really is a string, so a stray non-string value is appended
    /// beside rather than mis-scanned.
    /// Publish one configuration edit.
    ///
    /// Every setter goes through here rather than calling `atomic_update`
    /// directly, so a self-write is always taken in. Without that the
    /// configuration watcher would see disk differ from the served
    /// configuration, and every star toggled in the sidebar would cost a
    /// whole-graph reopen — which discards every cache the graph has built.
    fn write_config(
        &self,
        path: &std::path::Path,
        edit: impl Fn(&str) -> io::Result<String>,
    ) -> io::Result<()> {
        crate::filesystem_durability::atomic_update(path, &CONFIG_LOCK, edit)?;
        // A setting is read when it is used, so it applies now; a change that
        // reaches the graph waits for the caller to open a new one.
        self.take_in_config();
        Ok(())
    }

    pub fn set_favorites_page(&self, name: &str) -> io::Result<()> {
        let path = config_path_for_write(self)?;
        let quoted = format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""));
        self.write_config(&path, |content| {
            let mut content = content.to_string();
            const KEY: &str = ":tine/favorites-page";
            if let Some(start) = find_top_level_keyword(&content, KEY) {
                let after = start + KEY.len();
                let j = skip_blank(&content, after);
                if content.as_bytes().get(j) == Some(&b'"') {
                    let end = edn_str_end(&content, j);
                    content.replace_range(start..end, &format!("{KEY} {quoted}"));
                } else {
                    content.insert_str(after, &format!(" {quoted}"));
                }
            } else if let Some(brace) = content.find('{') {
                content.insert_str(brace + 1, &format!("\n {KEY} {quoted}\n"));
            } else {
                content = format!("{{{KEY} {quoted}}}\n");
            }
            Ok(content)
        })
    }

    /// Persist the favorites list to `:favorites [...]`, replacing the existing
    /// vector or inserting one, preserving the rest of the file.
    pub fn set_favorites(&self, names: &[String]) -> io::Result<()> {
        let path = config_path_for_write(self)?;
        self.write_config(&path, |content| {
            let mut content = content.to_string();
            let vec_str = format!(
                "[{}]",
                names
                    .iter()
                    .map(|n| format!("\"{}\"", n.replace('\\', "\\\\").replace('"', "\\\"")))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            if let Some(start) = find_top_level_keyword(&content, ":favorites") {
                // Replace the existing `:favorites [...]` vector. Require its value to
                // be a vector and find the matching `]` with an EDN-aware scan so a
                // favorite NAME containing `]` (or a comment in the vector) can't
                // truncate the replacement and corrupt config.edn.
                let after = start + ":favorites".len();
                let j = skip_blank(&content, after); // comment-aware, like the readers
                if content.as_bytes().get(j) == Some(&b'[') {
                    let end = match_close_bracket(&content, j) + 1;
                    content.replace_range(start..end, &format!(":favorites {vec_str}"));
                } else {
                    content.insert_str(after, &format!(" {vec_str}"));
                }
            } else if let Some(brace) = content.find('{') {
                content.insert_str(brace + 1, &format!("\n :favorites {vec_str}\n"));
            } else {
                content = format!("{{:favorites {vec_str}}}\n");
            }
            Ok(content)
        })
    }

    /// Persist the task workflow to `:preferred-workflow :todo`/`:now`, replacing
    /// the keyword value or inserting the key. `find_keyword` skips comments/strings
    /// so a commented or in-string `:preferred-workflow` is never edited.
    pub fn set_preferred_workflow(&self, wf: &str) -> io::Result<()> {
        let kw = if wf == "todo" { ":todo" } else { ":now" };
        let key = ":preferred-workflow";
        let path = config_path_for_write(self)?;
        self.write_config(&path, |content| {
            let mut content = content.to_string();

            if let Some(start) = find_top_level_keyword(&content, key) {
                let after = start + key.len();
                let vstart = skip_blank(&content, after); // comment-aware
                if content[vstart..].starts_with(':') {
                    let vrest = &content[vstart + 1..];
                    let end = vrest
                        .find(|c: char| c.is_whitespace() || c == '}' || c == ')')
                        .unwrap_or(vrest.len());
                    content.replace_range(vstart..vstart + 1 + end, kw);
                } else {
                    content.insert_str(after, &format!(" {kw}"));
                }
            } else if let Some(brace) = content.find('{') {
                content.insert_str(brace + 1, &format!("\n :preferred-workflow {kw}\n"));
            } else {
                content = format!("{{:preferred-workflow {kw}}}\n");
            }
            Ok(content)
        })
    }

    /// Persist `:feature/enable-timetracking?`. OG treats an absent key as ON,
    /// but writing the explicit boolean keeps the Settings toggle reversible.
    pub fn set_timetracking_enabled(&self, enabled: bool) -> io::Result<()> {
        self.set_config_bool(":feature/enable-timetracking?", enabled)
    }

    /// Persist `:ui/show-brackets?`. OG treats an absent key as ON, but writing
    /// the explicit boolean keeps the Settings toggle reversible.
    pub fn set_show_brackets(&self, enabled: bool) -> io::Result<()> {
        self.set_config_bool(":ui/show-brackets?", enabled)
    }

    /// Persist the document-mode escape hatch. OG declares the equivalent key in
    /// `src/main/frontend/schema/handler/common_config.cljc:41` at `6e7afa8eb`.
    pub fn set_doc_mode_enter_for_new_block(&self, enabled: bool) -> io::Result<()> {
        self.set_config_bool(":shortcut/doc-mode-enter-for-new-block?", enabled)
    }

    /// Persist logical (Roam-like) outdenting. OG declares the equivalent key in
    /// `src/main/frontend/schema/handler/common_config.cljc:83` at `6e7afa8eb`.
    pub fn set_logical_outdenting(&self, enabled: bool) -> io::Result<()> {
        self.set_config_bool(":editor/logical-outdenting?", enabled)
    }

    /// Write one graph-portable boolean through the existing config.edn atomic
    /// update path, preserving unrelated keys, comments, and formatting.
    fn set_config_bool(&self, key: &str, enabled: bool) -> io::Result<()> {
        let val = if enabled { "true" } else { "false" };
        let path = config_path_for_write(self)?;
        self.write_config(&path, |content| {
            let mut content = content.to_string();

            if let Some(start) = find_top_level_keyword(&content, key) {
                let after = start + key.len();
                match next_value_span(&content, after, content.len()) {
                    Some((vstart, vend, _)) if vend > vstart => {
                        content.replace_range(vstart..vend, val)
                    }
                    _ => content.insert_str(after, &format!(" {val}")),
                }
            } else if let Some(brace) = content.find('{') {
                content.insert_str(brace + 1, &format!("\n {key} {val}\n"));
            } else {
                content = format!("{{{key} {val}}}\n");
            }
            Ok(content)
        })
    }

    /// Persist the one-time in-app Guide announcement flag, graph-locally.
    pub fn set_guide_announced(&self, announced: bool) -> io::Result<()> {
        self.set_config_bool(":tine/guide-announced?", announced)
    }

    /// Persist the preferred format for new pages/journals as
    /// `:preferred-format "Markdown"|"Org"` (the capitalized string OG uses),
    /// replacing the existing value or inserting the key, preserving the rest of
    /// the file (comments, formatting, other keys).
    pub fn set_preferred_format(&self, fmt: crate::vocab::Format) -> io::Result<()> {
        let val = match fmt {
            crate::vocab::Format::Org => "\"Org\"",
            crate::vocab::Format::Md => "\"Markdown\"",
        };
        let key = ":preferred-format";
        let path = config_path_for_write(self)?;
        self.write_config(&path, |content| {
            let mut content = content.to_string();

            if let Some(start) = find_top_level_keyword(&content, key) {
                let after = start + key.len();
                // Replace the FULL existing value span — whether it's a string
                // (`"Markdown"`) or a keyword (`:org`) — so a keyword value isn't left
                // dangling beside the new string (which would corrupt the map).
                match next_value_span(&content, after, content.len()) {
                    Some((vstart, vend, _)) if vend > vstart => {
                        content.replace_range(vstart..vend, val)
                    }
                    _ => content.insert_str(after, &format!(" {val}")),
                }
            } else if let Some(brace) = content.find('{') {
                content.insert_str(brace + 1, &format!("\n {key} {val}\n"));
            } else {
                content = format!("{{{key} {val}}}\n");
            }
            Ok(content)
        })
    }

    /// `:journal/page-title-format "<pattern>"` — the journal *display* title
    /// format (e.g. `MMM do, yyyy`). Affects how journal dates render and how new
    /// journal titles/`[[date]]` references are written; the on-disk file name
    /// (governed by `:journal/file-name-format`, default `yyyy_MM_dd`) is left
    /// untouched, so existing journal files keep working. Replaces the existing
    /// value or inserts the key, preserving the rest of the file.
    pub fn set_journal_page_title_format(&self, fmt: &str) -> io::Result<()> {
        let escaped = fmt.replace('\\', "\\\\").replace('"', "\\\"");
        let val = format!("\"{escaped}\"");
        let key = ":journal/page-title-format";
        let path = config_path_for_write(self)?;
        self.write_config(&path, |content| {
            let mut content = content.to_string();

            if let Some(start) = find_top_level_keyword(&content, key) {
                let after = start + key.len();
                match next_value_span(&content, after, content.len()) {
                    Some((vstart, vend, _)) if vend > vstart => {
                        content.replace_range(vstart..vend, &val)
                    }
                    _ => content.insert_str(after, &format!(" {val}")),
                }
            } else if let Some(brace) = content.find('{') {
                content.insert_str(brace + 1, &format!("\n {key} {val}\n"));
            } else {
                content = format!("{{{key} {val}}}\n");
            }
            Ok(content)
        })
    }

    /// Persist the new-journal default template as `:default-templates {:journals
    /// "Name"}`. `Some` sets/replaces the `:journals` entry; `None` removes it.
    /// Other keys in `:default-templates`, the rest of the file, and comments are
    /// preserved.
    pub fn set_default_journal_template(&self, name: Option<&str>) -> io::Result<()> {
        let path = config_path_for_write(self)?;
        self.write_config(&path, |content| {
            let mut content = content.to_string();

            // Locate a real `:default-templates` whose value is a map literal `{ … }`.
            let dt = find_top_level_keyword(&content, ":default-templates").and_then(|start| {
                let after = start + ":default-templates".len();
                let j = skip_blank(&content, after); // comment-aware
                if content.as_bytes().get(j) != Some(&b'{') {
                    return None; // value isn't a map → don't touch it
                }
                let close = match_close_brace(&content, j);
                Some((j, close)) // byte indices of `{` and matching `}`
            });

            match name {
                Some(n) => {
                    let v = format!("\"{}\"", n.replace('\\', "\\\\").replace('"', "\\\""));
                    match dt {
                        Some((open, close)) => {
                            if let Some(jrel) = find_keyword(&content[open + 1..close], ":journals")
                            {
                                // Replace the value IMMEDIATELY after :journals (string or
                                // not) — never scan for the next quote anywhere, which could
                                // land on a later key's value.
                                let after = open + 1 + jrel + ":journals".len();
                                match next_value_span(&content, after, close) {
                                    Some((vstart, vend, _)) => {
                                        content.replace_range(vstart..vend, &v)
                                    }
                                    None => content.insert_str(after, &format!(" {v}")),
                                }
                            } else {
                                let sep = if content[open + 1..close].trim().is_empty() {
                                    ""
                                } else {
                                    " "
                                };
                                content.insert_str(open + 1, &format!(":journals {v}{sep}"));
                            }
                        }
                        None => {
                            let entry = format!("\n :default-templates {{:journals {v}}}\n");
                            if let Some(brace) = content.find('{') {
                                content.insert_str(brace + 1, &entry);
                            } else {
                                content = format!("{{:default-templates {{:journals {v}}}}}\n");
                            }
                        }
                    }
                }
                None => {
                    if let Some((open, close)) = dt {
                        if let Some(jrel) = find_keyword(&content[open + 1..close], ":journals") {
                            let jstart = open + 1 + jrel;
                            let after = jstart + ":journals".len();
                            let end = next_value_span(&content, after, close)
                                .map(|(_, vend, _)| vend)
                                .unwrap_or(after);
                            let tail: usize = content[end..close]
                                .chars()
                                .take_while(|c| c.is_whitespace() || *c == ',')
                                .map(|c| c.len_utf8())
                                .sum();
                            content.replace_range(jstart..end + tail, "");
                        }
                    }
                }
            }
            Ok(content)
        })
    }

    /// Persist the graph's startup page in Logseq's `:default-home {:page
    /// "Name"}` map. Sibling keys (notably OG's `:sidebar`) and the rest of the
    /// file remain byte-for-byte untouched. A malformed/non-map `:default-home`
    /// is refused rather than replaced, so an automatic legacy migration can
    /// never destroy graph-owned configuration it does not understand.
    pub fn set_default_home_page(&self, name: Option<&str>) -> io::Result<()> {
        let path = config_path_for_write(self)?;
        self.write_config(&path, |content| {
            let mut content = content.to_string();
            let (root_open, root_close) = root_map_bounds(&content).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "config.edn is not a balanced root map",
                )
            })?;
            let existing =
                find_keyword_at_map_level(&content[root_open + 1..root_close], ":default-home")
                    .map(|relative| {
                        let start = root_open + 1 + relative;
                        let after = start + ":default-home".len();
                        let open = skip_blank(&content, after);
                        if content.as_bytes().get(open) != Some(&b'{') {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                ":default-home exists but is not a map",
                            ));
                        }
                        let close = match_close_brace(&content, open);
                        if close >= content.len() || content.as_bytes().get(close) != Some(&b'}') {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                ":default-home map is not balanced",
                            ));
                        }
                        Ok((open, close))
                    });
            let existing = match existing {
                Some(result) => Some(result?),
                None => None,
            };

            match name.map(str::trim).filter(|name| !name.is_empty()) {
                Some(name) => {
                    let value = format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""));
                    match existing {
                        Some((open, close)) => {
                            if let Some(relative) =
                                find_keyword_at_map_level(&content[open + 1..close], ":page")
                            {
                                let after = open + 1 + relative + ":page".len();
                                match next_value_span(&content, after, close) {
                                    Some((value_start, value_end, _)) => {
                                        content.replace_range(value_start..value_end, &value)
                                    }
                                    None => content.insert_str(after, &format!(" {value}")),
                                }
                            } else {
                                let separator = if content[open + 1..close].trim().is_empty() {
                                    ""
                                } else {
                                    " "
                                };
                                content.insert_str(open + 1, &format!(":page {value}{separator}"));
                            }
                        }
                        None => {
                            let entry = format!("\n :default-home {{:page {value}}}\n");
                            content.insert_str(root_open + 1, &entry);
                        }
                    }
                }
                None => {
                    if let Some((open, close)) = existing {
                        if let Some(relative) =
                            find_keyword_at_map_level(&content[open + 1..close], ":page")
                        {
                            let start = open + 1 + relative;
                            let after = start + ":page".len();
                            let end = next_value_span(&content, after, close)
                                .map(|(_, value_end, _)| value_end)
                                .unwrap_or(after);
                            let tail: usize = content[end..close]
                                .chars()
                                .take_while(|c| c.is_whitespace() || *c == ',')
                                .map(char::len_utf8)
                                .sum();
                            content.replace_range(start..end + tail, "");
                        }
                    }
                }
            }
            Ok(content)
        })
    }

    /// Persist the first day of week to `:start-of-week N` (Logseq convention:
    /// 0=Monday … 6=Sunday), replacing the numeric value or inserting the key.
    /// `find_keyword` is comment/string-aware, so a commented `:start-of-week` is
    /// never edited (we insert a real one instead).
    pub fn set_start_of_week(&self, n: u32) -> io::Result<()> {
        let n = n.min(6);
        let key = ":start-of-week";
        let path = config_path_for_write(self)?;
        self.write_config(&path, |content| {
            let mut content = content.to_string();

            if let Some(start) = find_top_level_keyword(&content, key) {
                let after = start + key.len();
                let vstart = skip_blank(&content, after); // comment-aware
                let digits = content[vstart..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .count();
                if digits > 0 {
                    content.replace_range(vstart..vstart + digits, &n.to_string());
                } else {
                    content.insert_str(after, &format!(" {n}"));
                }
            } else if let Some(brace) = content.find('{') {
                content.insert_str(brace + 1, &format!("\n :start-of-week {n}\n"));
            } else {
                content = format!("{{:start-of-week {n}}}\n");
            }
            Ok(content)
        })
    }
}
