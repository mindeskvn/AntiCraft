//! YAML frontmatter of rules, skills and workflow files: the subset those files use.
//!
//! A block is a `---` line at the very top (after an optional BOM), `key: value` lines, then a
//! closing `---` (or `...`) line. Supported values:
//! - plain scalars, continued on more-indented lines (folded with spaces), ` #` comments cut;
//! - single- and double-quoted scalars (escapes in double quotes), also across lines;
//! - block scalars `|` and `>` with the `-` / `+` chomping indicators;
//! - flow sequences `[a, "b"]` and block sequences (`- item` lines under the key).
//!
//! Nested mappings, anchors and tags are not interpreted; such keys are skipped. Keys are
//! case-sensitive, as in YAML.

use std::collections::BTreeMap;

/// A frontmatter value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    List(Vec<String>),
}

/// Parsed file: the frontmatter fields and the text after the block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontmatter {
    /// A complete block was found at the top of the file.
    pub present: bool,
    pub fields: BTreeMap<String, Value>,
    /// Text after the closing line; the whole input when there is no block.
    pub body: String,
}

impl Frontmatter {
    /// Trimmed, non-empty string value (a list yields its items joined with `, `).
    pub fn str(&self, key: &str) -> Option<String> {
        let s = match self.fields.get(key)? {
            Value::Str(s) => s.trim().to_owned(),
            Value::List(v) => v.join(", "),
        };
        (!s.is_empty()).then_some(s)
    }

    /// List value: a sequence as is, or a string split on commas. Empty items are dropped.
    pub fn list(&self, key: &str) -> Vec<String> {
        let items: Vec<String> = match self.fields.get(key) {
            Some(Value::List(v)) => v.clone(),
            Some(Value::Str(s)) => s.split(',').map(str::to_owned).collect(),
            None => Vec::new(),
        };
        items.into_iter().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect()
    }
}

/// Parse `raw`. Without a complete block at the top, `present` is false and `body` is the
/// whole text (minus a BOM).
pub fn parse(raw: &str) -> Frontmatter {
    let s = raw.trim_start_matches('\u{feff}');
    let mut lines = Vec::new();
    let mut offset = 0;
    for piece in s.split_inclusive('\n') {
        lines.push((offset, piece.trim_end_matches(['\n', '\r'])));
        offset += piece.len();
    }
    let no_block = || Frontmatter { present: false, fields: BTreeMap::new(), body: s.to_owned() };
    if lines.first().map(|l| l.1.trim_end()) != Some("---") {
        return no_block();
    }
    let Some(close) = lines.iter().skip(1).position(|l| matches!(l.1.trim_end(), "---" | "...")).map(|i| i + 1) else {
        return no_block();
    };
    let body_start = lines.get(close + 1).map_or(s.len(), |l| l.0);
    let front: Vec<&str> = lines[1..close].iter().map(|l| l.1).collect();
    Frontmatter { present: true, fields: parse_fields(&front), body: s[body_start..].to_owned() }
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn parse_fields(lines: &[&str]) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if is_blank(line) || indent_of(line) > 0 || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, rest)) = line.split_once(':') else { continue };
        let key = key.trim().trim_matches(['"', '\'']);
        if key.is_empty() || key.contains(char::is_whitespace) {
            continue;
        }
        let rest = rest.trim_start();
        // Lines that belong to this key: blank or indented, up to the next top-level line.
        let start = i;
        while i < lines.len() && (is_blank(lines[i]) || indent_of(lines[i]) > 0) {
            i += 1;
        }
        let more = &lines[start..i];
        if let Some(v) = parse_value(rest, more) {
            out.insert(key.to_owned(), v);
        }
    }
    out
}

fn parse_value(first: &str, more: &[&str]) -> Option<Value> {
    if first.starts_with('|') || first.starts_with('>') {
        return Some(Value::Str(block_scalar(first, more)));
    }
    if first.starts_with('"') || first.starts_with('\'') {
        let joined = join_folded(first, more);
        return Some(Value::Str(unquote(&joined)));
    }
    if first.starts_with('[') {
        let joined = join_folded(first, more);
        return Some(Value::List(flow_items(&joined)));
    }
    let first = strip_comment(first);
    if first.is_empty() {
        let content: Vec<&str> = more.iter().copied().filter(|l| !is_blank(l)).collect();
        if content.is_empty() {
            return Some(Value::Str(String::new()));
        }
        if content.iter().all(|l| l.trim_start().starts_with('-')) {
            let items = content.iter().map(|l| scalar(l.trim_start().trim_start_matches('-').trim())).collect();
            return Some(Value::List(items));
        }
        if content.iter().any(|l| looks_like_mapping(l)) {
            return None; // nested mapping: not interpreted
        }
        return Some(Value::Str(fold_plain(&content)));
    }
    let mut parts = vec![first];
    parts.extend(more.iter().copied().filter(|l| !is_blank(l)).map(|l| strip_comment(l.trim())));
    Some(Value::Str(scalar(&parts.join(" "))))
}

fn looks_like_mapping(line: &str) -> bool {
    let t = line.trim();
    !t.starts_with(['"', '\'']) && t.split_once(':').is_some_and(|(k, v)| !k.contains(char::is_whitespace) && (v.is_empty() || v.starts_with(' ')))
}

/// `|` / `>` block scalar with optional `-` / `+` chomping (an explicit indent digit is
/// accepted and ignored: the indent comes from the first content line).
fn block_scalar(header: &str, more: &[&str]) -> String {
    let header = strip_comment(header);
    let folded = header.starts_with('>');
    let chomp = if header.contains('-') { '-' } else if header.contains('+') { '+' } else { ' ' };
    let indent = more.iter().find(|l| !is_blank(l)).map_or(0, |l| indent_of(l));
    let lines: Vec<&str> = more.iter().map(|l| if is_blank(l) { "" } else { l.get(indent..).unwrap_or(l.trim_start()) }).collect();
    let mut text = String::new();
    if folded {
        let mut prev_text = false;
        for l in &lines {
            if l.is_empty() {
                text.push('\n');
                prev_text = false;
            } else if l.starts_with([' ', '\t']) {
                // More-indented lines keep their line breaks.
                if prev_text {
                    text.push('\n');
                }
                text.push_str(l);
                text.push('\n');
                prev_text = false;
            } else {
                if prev_text {
                    text.push(' ');
                }
                text.push_str(l);
                prev_text = true;
            }
        }
        if prev_text {
            text.push('\n');
        }
    } else {
        for l in &lines {
            text.push_str(l);
            text.push('\n');
        }
    }
    match chomp {
        '-' => text.trim_end_matches('\n').to_owned(),
        '+' => text,
        _ => {
            let t = text.trim_end_matches('\n');
            if t.is_empty() { String::new() } else { format!("{t}\n") }
        }
    }
}

/// First line plus continuation lines, joined with single spaces (quoted and flow values).
fn join_folded(first: &str, more: &[&str]) -> String {
    let mut s = first.trim().to_owned();
    for l in more.iter().filter(|l| !is_blank(l)) {
        s.push(' ');
        s.push_str(l.trim());
    }
    s
}

/// Cut a ` #` comment from a plain scalar line.
fn strip_comment(s: &str) -> &str {
    let s = s.trim();
    if s.starts_with('#') {
        return "";
    }
    match s.find(" #") {
        Some(i) => s[..i].trim_end(),
        None => s,
    }
}

/// Unquote a quoted scalar; plain text is returned trimmed.
fn scalar(s: &str) -> String {
    let s = s.trim();
    if s.starts_with('"') || s.starts_with('\'') { unquote(s) } else { strip_comment(s).to_owned() }
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let mut chars = s.chars();
    match chars.next() {
        Some('\'') => {
            let mut out = String::new();
            let mut it = chars.peekable();
            while let Some(c) = it.next() {
                if c == '\'' {
                    if it.peek() == Some(&'\'') {
                        it.next();
                        out.push('\'');
                    } else {
                        break;
                    }
                } else {
                    out.push(c);
                }
            }
            out
        }
        Some('"') => {
            let mut out = String::new();
            let mut it = chars;
            while let Some(c) = it.next() {
                match c {
                    '"' => break,
                    '\\' => match it.next() {
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some('r') => out.push('\r'),
                        Some('0') => out.push('\0'),
                        Some('u') => {
                            let hex: String = it.by_ref().take(4).collect();
                            if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                                out.push(ch);
                            }
                        }
                        Some(other) => out.push(other),
                        None => {}
                    },
                    c => out.push(c),
                }
            }
            out
        }
        _ => s.to_owned(),
    }
}

/// Items of a `[a, "b, c", d]` flow sequence.
fn flow_items(s: &str) -> Vec<String> {
    let inner = s.trim().trim_start_matches('[');
    let inner = inner.rfind(']').map_or(inner, |i| &inner[..i]);
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in inner.chars() {
        match quote {
            Some(q) if c == q => {
                quote = None;
                cur.push(c);
            }
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            None if c == ',' => items.push(std::mem::take(&mut cur)),
            None => cur.push(c),
        }
    }
    items.push(cur);
    items.into_iter().map(|i| scalar(&i)).filter(|i| !i.is_empty()).collect()
}

fn fold_plain(lines: &[&str]) -> String {
    lines.iter().map(|l| strip_comment(l.trim())).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folded_description_from_the_skill_docs() {
        let fm = parse(
            "---\nname: my-specialized-skill\ndescription: >-\n  Describe when the agent should use this skill. Use third-person.\n  Example: \"Use this skill when tests fail.\"\n---\n\n# Title\n",
        );
        assert!(fm.present);
        assert_eq!(fm.str("name").as_deref(), Some("my-specialized-skill"));
        assert_eq!(
            fm.str("description").as_deref(),
            Some("Describe when the agent should use this skill. Use third-person. Example: \"Use this skill when tests fail.\"")
        );
        assert_eq!(fm.body, "\n# Title\n");
    }

    #[test]
    fn scalars_quotes_comments_and_crlf() {
        let fm = parse("\u{feff}---\r\ntrigger: always_on   # on\r\nname: \"ship \\\"it\\\"\"\r\nq: 'it''s'\r\nplain: first\r\n  second\r\n---\r\nbody\r\n");
        assert_eq!(fm.str("trigger").as_deref(), Some("always_on"));
        assert_eq!(fm.str("name").as_deref(), Some("ship \"it\""));
        assert_eq!(fm.str("q").as_deref(), Some("it's"));
        assert_eq!(fm.str("plain").as_deref(), Some("first second"));
        assert_eq!(fm.body, "body\r\n");
    }

    #[test]
    fn literal_block_and_chomping() {
        let fm = parse("---\na: |\n  line 1\n  line 2\n\nb: |-\n  x\nc: >\n  one\n  two\n\n  three\n---\n");
        assert_eq!(fm.fields.get("a"), Some(&Value::Str("line 1\nline 2\n".into())));
        assert_eq!(fm.fields.get("b"), Some(&Value::Str("x".into())));
        assert_eq!(fm.fields.get("c"), Some(&Value::Str("one two\nthree\n".into())));
    }

    #[test]
    fn lists_flow_block_and_comma_strings() {
        let fm = parse("---\nglobs: \"*.ts, *.tsx\"\nflow: [\"*.proto\", '**/*.pb.go', x]\nblock:\n  - a\n  - \"b c\"\nnested:\n  k: v\n---\n");
        assert_eq!(fm.list("globs"), ["*.ts", "*.tsx"]);
        assert_eq!(fm.list("flow"), ["*.proto", "**/*.pb.go", "x"]);
        assert_eq!(fm.list("block"), ["a", "b c"]);
        assert!(!fm.fields.contains_key("nested"));
        assert!(fm.list("missing").is_empty());
    }

    #[test]
    fn missing_or_unclosed_block() {
        let fm = parse("plain text");
        assert!(!fm.present);
        assert_eq!(fm.body, "plain text");
        let fm = parse("---\nname: x\nno close");
        assert!(!fm.present);
        assert!(fm.body.starts_with("---"));
        let fm = parse("---\n---\nx");
        assert!(fm.present);
        assert!(fm.fields.is_empty());
        assert_eq!(fm.body, "x");
        assert_eq!(parse("---\nk:\n---\n").str("k"), None);
    }
}
