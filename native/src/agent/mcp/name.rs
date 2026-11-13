//! Model-facing names for MCP tools: `mcp__<server>__<tool>`.
//!
//! The prefix is a security property, not cosmetics. `Agent::run_one` claims `activate_skill`,
//! `read_rule`, `finish` and the orchestration tools *before* it consults the tool registry, so
//! a server exposing a tool literally named `finish` would end a turn instead of running. A
//! mandatory prefix makes that collision structurally impossible.
//!
//! Names must also be usable as a Code Assist function name (`[A-Za-z0-9_]`, at most 64
//! bytes), while real MCP names contain `.`, `/`, `:` and run long. So both halves are slugged
//! and the whole is capped. Truncation is suffixed with six hex digits of an FNV-1a hash of
//! the *full* remote name, which keeps two long names distinct and — because history and
//! workflow scripts store names, not tool objects — keeps the mapping identical across
//! restarts.

/// Prefix shared by every exposed MCP tool name.
pub const PREFIX: &str = "mcp__";
/// Separator between the two slugs. Slugs never contain a double underscore, so splitting on
/// the first one is unambiguous.
pub const SEPARATOR: &str = "__";
/// Longest exposed name, in bytes. Matches the Code Assist function-name limit.
pub const MAX_EXPOSED_BYTES: usize = 64;
/// Longest server slug produced. Bounded separately so a pathological server name cannot eat
/// the whole name budget and leave every tool of that server hashed.
pub const MAX_SERVER_SLUG_BYTES: usize = 24;

/// Bytes of `tool` left after the hash suffix: `_` plus six hex digits.
const HASH_SUFFIX_BYTES: usize = 7;

/// The model-facing name of one tool, deterministic for a given `(server, tool)` pair.
///
/// Never fails and never returns an empty string: an unusable name still yields a legal
/// identifier, because dropping a tool silently is worse than exposing one under a fallback.
pub fn exposed_name(server: &str, tool: &str) -> String {
    let server_slug = slug(server, "server", MAX_SERVER_SLUG_BYTES);
    let mut tool_slug = slug(tool, "tool", MAX_EXPOSED_BYTES);
    let fixed = PREFIX.len() + server_slug.len() + SEPARATOR.len();
    let room = MAX_EXPOSED_BYTES.saturating_sub(fixed);
    if tool_slug.len() > room {
        let keep = room.saturating_sub(HASH_SUFFIX_BYTES);
        let mut cut = keep.min(tool_slug.len());
        // The slug is ASCII, but stay boundary-safe anyway so a later change to `slug` cannot
        // turn this into a panic.
        while cut > 0 && !tool_slug.is_char_boundary(cut) {
            cut -= 1;
        }
        tool_slug.truncate(cut);
        let trimmed = tool_slug.trim_end_matches('_').len();
        tool_slug.truncate(trimmed);
        tool_slug.push('_');
        // Hashed from the *remote* name, not the slug: two remote names that slug identically
        // still get distinct, stable suffixes, and a rename cannot silently reuse a hash.
        tool_slug.push_str(&hash6(tool));
    }
    format!("{PREFIX}{server_slug}{SEPARATOR}{tool_slug}")
}

/// Split an exposed name back into `(server_slug, tool_slug)`.
///
/// This is only a debugging aid: dispatch goes through the catalog, never through the name.
/// `None` when the string is not shaped like an exposed MCP tool name.
pub fn split_exposed(exposed: &str) -> Option<(&str, &str)> {
    let rest = exposed.strip_prefix(PREFIX)?;
    let (server, tool) = rest.split_once(SEPARATOR)?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

/// Lowercase `[a-z0-9_]`, runs of `_` collapsed, no leading or trailing `_`, capped at `max`
/// bytes and never empty.
fn slug(raw: &str, fallback: &str, max: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max));
    let mut prev_underscore = false;
    for ch in raw.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            prev_underscore = false;
            out.push(ch);
        } else {
            if prev_underscore || out.is_empty() {
                continue;
            }
            prev_underscore = true;
            out.push('_');
        }
        if out.len() >= max {
            break;
        }
    }
    let trimmed = out.trim_end_matches('_');
    if trimmed.is_empty() {
        fallback.to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Six lowercase hex digits of the 64-bit FNV-1a hash of `s`.
fn hash6(s: &str) -> String {
    format!("{:06x}", fnv1a(s) & 0x00ff_ffff)
}

/// FNV-1a, 64-bit. Chosen over SHA-256 because the name only needs to be stable and cheap;
/// it is not a security boundary.
fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_a_plain_name_without_a_hash() {
        assert_eq!(exposed_name("fs", "read_file"), "mcp__fs__read_file");
    }

    #[test]
    fn slugs_punctuation_and_case() {
        assert_eq!(exposed_name("My Files", "Read.File"), "mcp__my_files__read_file");
        // Runs of separators collapse, so the result cannot contain the `__` separator by
        // accident and `split_exposed` stays unambiguous.
        assert_eq!(exposed_name("a--b", "x::y"), "mcp__a_b__x_y");
        assert!(!exposed_name("a..b", "c//d").contains("__b"));
    }

    #[test]
    fn falls_back_for_names_with_no_legal_characters() {
        assert_eq!(exposed_name("***", "+++"), "mcp__server__tool");
        assert_eq!(exposed_name("", ""), "mcp__server__tool");
    }

    #[test]
    fn caps_the_length_and_keeps_the_hash_stable() {
        let server = "server-with-a-very-long-name-indeed-yes";
        let tool = "a-tool-with-an-equally-long-name-that-will-not-fit-in-sixty-four-bytes";
        let first = exposed_name(server, tool);
        let second = exposed_name(server, tool);
        assert_eq!(first, second, "the mapping must be deterministic across calls");
        assert!(first.len() <= MAX_EXPOSED_BYTES, "{} bytes: {first}", first.len());
        assert!(first.starts_with("mcp__"), "{first}");
        // Same long prefix, different tail: the hash must keep them apart.
        let sibling = exposed_name(server, &format!("{tool}-two"));
        assert_ne!(first, sibling);
    }

    #[test]
    fn a_long_server_name_is_truncated_first() {
        let server = "s".repeat(200);
        let name = exposed_name(&server, "tool");
        assert!(name.len() <= MAX_EXPOSED_BYTES, "{} bytes", name.len());
        let (server_slug, tool_slug) = split_exposed(&name).expect("well formed");
        assert_eq!(server_slug.len(), MAX_SERVER_SLUG_BYTES);
        assert_eq!(tool_slug, "tool");
    }

    #[test]
    fn split_reverses_the_name() {
        let name = exposed_name("git-hub", "list.issues");
        assert_eq!(split_exposed(&name), Some(("git_hub", "list_issues")));
        assert_eq!(split_exposed("read_file"), None);
        assert_eq!(split_exposed("mcp__only-server"), None);
        assert_eq!(split_exposed("mcp____tool"), None);
    }

    #[test]
    fn every_exposed_name_is_a_legal_function_name() {
        for (server, tool) in [
            ("fs", "read_file"),
            ("Ünïcödé-сервер", "задача"),
            (&"x".repeat(300), &"y".repeat(300)),
        ] {
            let name = exposed_name(server, tool);
            assert!(name.len() <= MAX_EXPOSED_BYTES);
            assert!(name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'), "{name}");
        }
    }
}
