//! Pure, egui-free helpers for the UI: relative times, project grouping, history date buckets,
//! and the model picker descriptor table. All unit tested.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::settings::{self, MODEL_CHOICES};
use crate::store::ConversationSummary;

/// Compact relative time like the reference sidebar: "now", "5m", "3h", "2d", "3w", "5mo", "2y".
pub fn relative_time(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - t).num_seconds().max(0);
    let (m, h, d) = (60, 3600, 86_400);
    match secs {
        s if s < m => "now".into(),
        s if s < h => format!("{}m", s / m),
        s if s < d => format!("{}h", s / h),
        s if s < 7 * d => format!("{}d", s / d),
        s if s < 30 * d => format!("{}w", s / (7 * d)),
        s if s < 365 * d => format!("{}mo", s / (30 * d)),
        s => format!("{}y", s / (365 * d)),
    }
}

/// "Worked for ..." duration text: "12s", "1m", "1m 5s", "1h 2m".
pub fn duration_text(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 if s.is_multiple_of(60) => format!("{}m", s / 60),
        60..=3599 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

/// Label for a workspace path: the last component, or the whole path for roots like `C:\`.
pub fn project_label(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// Label of the group for conversations without a workspace.
pub const OTHER_GROUP: &str = "Other";

/// Case-insensitive match of `query` against title and project label.
pub fn matches_filter(c: &ConversationSummary, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    c.title.to_lowercase().contains(&q) || c.workspace.as_deref().is_some_and(|w| project_label(w).to_lowercase().contains(&q))
}

/// Local wall-clock time `HH:MM` of `t`, the way the Account tab says when a model list was
/// last checked: hour and minute only, no date, no seconds.
pub fn clock(t: DateTime<Utc>) -> String {
    t.with_timezone(&chrono::Local).format("%H:%M").to_string()
}

/// Model picker entry. `id` is what is persisted in settings (IDs are unchanged). Owned, because
/// the picker also lists subscription accounts' models, which arrive at run time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub id: String,
    pub label: String,
    pub note: &'static str,
    pub preview: bool,
    /// Alias rows (resolved at send time) vs concrete IDs.
    pub alias: bool,
}

const ALIASES: &[(&str, &str, &str)] = &[
    (settings::ALIAS_AUTO, "Auto", "Pro, preview when enabled"),
    (settings::ALIAS_PRO, "Pro", "Best quality"),
    (settings::ALIAS_FLASH, "Flash", "Fast"),
    (settings::ALIAS_FLASH_LITE, "Flash Lite", "Fastest"),
];

/// Display label for a model id: "gemini-3.1-pro-preview" -> "Gemini 3.1 Pro",
/// "gpt-5.1-codex" -> "GPT 5.1 Codex", "claude-sonnet-4-6" -> "Claude Sonnet 4.6".
///
/// Any other id is returned as it is. A subscription account can send models of several
/// families, and a label must never name a family the model is not from.
pub fn model_label(id: &str) -> String {
    if let Some((_, l, _)) = ALIASES.iter().find(|a| a.0 == id) {
        return (*l).to_owned();
    }
    if let Some(rest) = id.strip_prefix("gemini-") {
        let rest = rest.strip_suffix("-preview").unwrap_or(rest);
        return format!("Gemini {}", title_words(rest, false));
    }
    if let Some(rest) = id.strip_prefix("gpt-") {
        return format!("GPT {}", title_words(rest, true));
    }
    if let Some(rest) = id.strip_prefix("claude-") {
        return format!("Claude {}", title_words(rest, true));
    }
    id.to_owned()
}

/// The `-`-separated words of a model id, each starting upper-case when it starts with a letter.
///
/// With `versions`, a short number right after another number continues its version: Anthropic
/// spells 4.6 as `4-6`. Longer numbers (a date stamp such as `20250929`) stay words of their own.
fn title_words(rest: &str, versions: bool) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut after_number = false;
    for w in rest.split('-').filter(|w| !w.is_empty()) {
        let number = w.chars().all(|c| c.is_ascii_digit());
        match out.last_mut() {
            Some(last) if versions && number && after_number && w.len() <= 2 => {
                last.push('.');
                last.push_str(w);
            }
            _ => {
                let mut c = w.chars();
                out.push(match c.next() {
                    Some(f) if f.is_ascii_alphabetic() => f.to_ascii_uppercase().to_string() + c.as_str(),
                    _ => w.to_owned(),
                });
            }
        }
        after_number = number;
    }
    out.join(" ")
}

/// All picker entries: aliases first, then every concrete `MODEL_CHOICES` id (except `auto`).
pub fn model_choices() -> Vec<ModelChoice> {
    let mut v: Vec<ModelChoice> = ALIASES
        .iter()
        .map(|(id, label, note)| ModelChoice { id: (*id).to_owned(), label: (*label).into(), note, preview: false, alias: true })
        .collect();
    for id in MODEL_CHOICES.iter().copied().filter(|m| *m != settings::ALIAS_AUTO) {
        v.push(ModelChoice { id: id.to_owned(), label: model_label(id), note: id, preview: id.ends_with("-preview"), alias: false });
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use chrono::TimeZone;
    use uuid::Uuid;

    fn summary(title: &str, ws: Option<&str>, t: DateTime<Utc>) -> ConversationSummary {
        ConversationSummary { id: Uuid::new_v4(), title: title.into(), workspace: ws.map(PathBuf::from), model: "auto".into(), updated_at: t }
    }

    #[test]
    fn relative_times() {
        let now = Utc.with_ymd_and_hms(2026, 10, 26, 12, 0, 0).unwrap();
        let ago = |s: i64| relative_time(now - chrono::Duration::seconds(s), now);
        assert_eq!(ago(5), "now");
        assert_eq!(ago(-30), "now");
        assert_eq!(ago(5 * 60), "5m");
        assert_eq!(ago(3 * 3600), "3h");
        assert_eq!(ago(2 * 86_400), "2d");
        assert_eq!(ago(14 * 86_400), "2w");
        assert_eq!(ago(150 * 86_400), "5mo");
        assert_eq!(ago(800 * 86_400), "2y");
    }

    #[test]
    fn durations() {
        assert_eq!(duration_text(Duration::from_secs(12)), "12s");
        assert_eq!(duration_text(Duration::from_secs(60)), "1m");
        assert_eq!(duration_text(Duration::from_secs(65)), "1m 5s");
        assert_eq!(duration_text(Duration::from_secs(3720)), "1h 2m");
    }

    #[test]
    fn project_label_handles_roots() {
        assert_eq!(project_label(Path::new("C:/x/MyProj")), "MyProj");
        assert!(!project_label(Path::new("C:\\")).is_empty());
    }

    #[test]
    fn filter_matches_title_and_project() {
        let c = summary("Fix Encoding", Some("C:/w/Tool_Video"), Utc::now());
        assert!(matches_filter(&c, "encod"));
        assert!(matches_filter(&c, "tool_v"));
        assert!(matches_filter(&c, "  "));
        assert!(!matches_filter(&c, "zzz"));
    }

    #[test]
    fn model_table_keeps_ids() {
        let ch = model_choices();
        for m in MODEL_CHOICES {
            assert!(ch.iter().any(|c| c.id == *m), "{m} missing");
        }
        assert_eq!(model_label("gemini-3.1-pro-preview"), "Gemini 3.1 Pro");
        assert_eq!(model_label("gemini-3.1-flash-lite"), "Gemini 3.1 Flash Lite");
        assert_eq!(model_label("auto"), "Auto");
        assert!(ch.iter().find(|c| c.id == "gemini-3-pro-preview").unwrap().preview);
    }

    #[test]
    fn account_model_labels_name_their_own_family() {
        assert_eq!(model_label("gpt-5.1-codex"), "GPT 5.1 Codex");
        assert_eq!(model_label("gpt-5-codex-max"), "GPT 5 Codex Max");
        assert_eq!(model_label("claude-sonnet-4-6"), "Claude Sonnet 4.6");
        assert_eq!(model_label("claude-3-5-haiku-latest"), "Claude 3.5 Haiku Latest");
        assert_eq!(model_label("claude-sonnet-4-5-20250929"), "Claude Sonnet 4.5 20250929", "a date is no version part");
        assert_eq!(model_label("gemini-3-pro-high"), "Gemini 3 Pro High");
        // Anything else is shown as it is, and nothing but a Gemini id is called Gemini.
        for id in ["o3", "o4-mini", "gpt-oss-120b-medium", "claude-opus-4-1", "deepseek-r1", "x"] {
            let label = model_label(id);
            assert!(!label.contains("Gemini"), "{id} -> {label}");
        }
        assert_eq!(model_label("o4-mini"), "o4-mini");
    }

    #[test]
    fn clock_is_local_hours_and_minutes() {
        let t = Utc.with_ymd_and_hms(2026, 11, 6, 13, 50, 42).unwrap();
        let s = clock(t);
        assert_eq!(s, t.with_timezone(&chrono::Local).format("%H:%M").to_string());
        assert_eq!(s.len(), 5, "{s}");
        assert_eq!(&s[2..3], ":", "{s}");
    }
}
