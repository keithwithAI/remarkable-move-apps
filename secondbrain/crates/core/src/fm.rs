//! Minimal Obsidian-YAML frontmatter: an ordered key map + surgical scalar writes.
//!
//! Supported subset (what Obsidian's properties UI emits): `key: scalar`, inline lists
//! `key: [a, b]`, and block lists (`key:` followed by `  - item` lines). Anything richer
//! (nested maps, multiline strings) is left untouched by [`set_scalar`] and surfaced by
//! [`parse_map`] as a raw [`Value::Scalar`] of the remainder of the key's line.
//!
//! The reader crate's `set_status`/`set_tags` are older per-key versions of [`set_scalar`];
//! migrating them here is a known follow-up.

/// One frontmatter value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Trimmed, with one layer of surrounding `"`/`'` quotes stripped.
    Scalar(String),
    List(Vec<String>),
}

impl Value {
    /// The scalar text, if this is a scalar.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Scalar(s) => Some(s),
            Value::List(_) => None,
        }
    }
    /// The items — a scalar reads as a one-item list (Obsidian treats `tags: x` that way).
    pub fn items(&self) -> Vec<&str> {
        match self {
            Value::Scalar(s) if s.is_empty() => Vec::new(),
            Value::Scalar(s) => vec![s.as_str()],
            Value::List(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

/// Ordered `key → value` pairs from the leading `---` block. Empty when there is none.
pub fn parse_map(source: &str) -> Vec<(String, Value)> {
    let Some((ys, ye, _)) = span(source) else {
        return Vec::new();
    };
    let mut out: Vec<(String, Value)> = Vec::new();
    for line in source[ys..ye].lines() {
        // A block-list item extends the previous key's list.
        let trimmed = line.trim_start();
        if let Some(item) = trimmed.strip_prefix("- ") {
            if let Some((_, Value::List(items))) = out.last_mut() {
                items.push(unquote(item));
            }
            continue;
        }
        // Ignore indented continuations we don't model (nested maps etc.).
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let rest = rest.trim();
        let value = if rest.is_empty() {
            Value::List(Vec::new()) // a bare `key:` line opens a block list
        } else if let Some(inner) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            Value::List(inner.split(',').map(unquote).filter(|s| !s.is_empty()).collect())
        } else {
            Value::Scalar(unquote(rest))
        };
        out.push((key.to_string(), value));
    }
    out
}

/// Case-insensitive key lookup.
pub fn get<'a>(map: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    map.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
}

/// Insert-or-replace `key: value` in the frontmatter, preserving every other byte of the file
/// (unknown keys, other list blocks, and the entire body). Synthesises a minimal `---` block when
/// the file has none. Replacing a key that currently holds a block list drops the key line AND its
/// `  - item` continuation lines (same rule as the reader's `set_tags`). Writes are LF-only.
pub fn set_scalar(source: &str, key: &str, value: &str) -> String {
    let Some((ys, ye, _)) = span(source) else {
        return format!("---\n{key}: {value}\n---\n{source}");
    };
    // Rebuild the YAML, dropping the existing key (and any block-list items under it).
    let mut new_yaml = String::with_capacity(ye - ys + key.len() + value.len() + 4);
    let mut in_dropped = false;
    for line in source[ys..ye].split_inclusive('\n') {
        if in_dropped && line.trim_start().starts_with("- ") {
            continue; // dropped key's list item
        }
        in_dropped = false;
        if let Some((k, _)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case(key) {
                in_dropped = true;
                continue;
            }
        }
        new_yaml.push_str(line);
    }
    if !new_yaml.is_empty() && !new_yaml.ends_with('\n') {
        new_yaml.push('\n');
    }
    new_yaml.push_str(key);
    new_yaml.push_str(": ");
    new_yaml.push_str(value);
    new_yaml.push('\n');

    let mut out = String::with_capacity(source.len() + key.len() + value.len() + 4);
    out.push_str(&source[..ys]); // opening `---\n`
    out.push_str(&new_yaml);
    out.push_str(&source[ye..]); // closing `---` + body, byte-for-byte
    out
}

/// Byte offsets of a leading `---`…`---` block: `(yaml_start, yaml_end, body_start)`.
/// Copy of the reader's `frontmatter_span` (tolerates a CRLF opening fence; writes stay LF).
fn span(source: &str) -> Option<(usize, usize, usize)> {
    if !(source.starts_with("---\n") || source.starts_with("---\r\n")) {
        return None;
    }
    let after_open = source.find('\n')? + 1;
    let mut cur = after_open;
    for line in source[after_open..].split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']) == "---" {
            return Some((after_open, cur, cur + line.len()));
        }
        cur += line.len();
    }
    None // unterminated → no frontmatter
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix('"').and_then(|r| r.strip_suffix('"')).unwrap_or(s);
    let s = s.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')).unwrap_or(s);
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\ntype: recipe\ncuisine: \"Middle Eastern\"\ntags:\n  - breakfast\n  - eggs\nsource: https://example.com/shakshuka\ntime_min: 30\n---\n\n## Ingredients\n- eggs\n";

    #[test]
    fn parses_scalars_inline_and_block_lists() {
        let m = parse_map(SAMPLE);
        assert_eq!(get(&m, "type").unwrap().as_str(), Some("recipe"));
        assert_eq!(get(&m, "cuisine").unwrap().as_str(), Some("Middle Eastern"), "quotes stripped");
        assert_eq!(get(&m, "TAGS").unwrap().items(), ["breakfast", "eggs"], "case-insensitive get");
        assert_eq!(get(&m, "source").unwrap().as_str(), Some("https://example.com/shakshuka"), "first-colon split keeps URLs whole");
        assert_eq!(get(&m, "time_min").unwrap().as_str(), Some("30"));
        // Inline and block lists parse identically.
        let inline = parse_map("---\ntags: [breakfast, \"eggs\"]\n---\nbody");
        assert_eq!(get(&inline, "tags").unwrap(), get(&m, "tags").unwrap());
    }

    #[test]
    fn no_frontmatter_and_empty_block() {
        assert!(parse_map("just a body\n").is_empty());
        assert!(parse_map("---\n---\n").is_empty());
        assert!(parse_map("---\nunterminated: yes\n").is_empty(), "unterminated fence is body text");
    }

    #[test]
    fn set_scalar_synthesises_replaces_and_preserves_everything_else() {
        // No frontmatter → synthesised block, body untouched after it.
        let out = set_scalar("body only\n", "rating", "4");
        assert_eq!(out, "---\nrating: 4\n---\nbody only\n");
        // Insert into an existing block: unknown keys (URL value with ':') + list blocks + body
        // preserved byte-for-byte; only the rating line is added.
        let out = set_scalar(SAMPLE, "rating", "4");
        assert_eq!(out, SAMPLE.replace("---\n\n## Ingredients", "rating: 4\n---\n\n## Ingredients"));
        // Replace is byte-stable on repeat.
        let once = set_scalar(&out, "rating", "5");
        assert_eq!(set_scalar(&once, "rating", "5"), once);
        assert!(once.contains("rating: 5\n") && !once.contains("rating: 4"));
        // Empty block gains one line.
        assert_eq!(set_scalar("---\n---\nB", "k", "v"), "---\nk: v\n---\nB");
    }

    #[test]
    fn set_scalar_over_a_block_list_drops_its_items() {
        let out = set_scalar(SAMPLE, "tags", "none");
        let yaml_end = out.rfind("---\n").unwrap();
        assert!(!out[..yaml_end].contains("- breakfast") && !out[..yaml_end].contains("- eggs"), "old items dropped from the frontmatter");
        assert!(out.contains("tags: none\n"));
        assert!(out.contains("source: https://example.com/shakshuka\n"), "sibling keys survive");
        assert!(out.ends_with("## Ingredients\n- eggs\n"), "body list untouched");
    }

    #[test]
    fn scalar_reads_as_one_item_list() {
        let m = parse_map("---\ntags: solo\n---\n");
        assert_eq!(get(&m, "tags").unwrap().items(), ["solo"]);
    }
}
