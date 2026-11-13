//! The static per-provider model catalog: every model id a provider is known to serve, and the
//! input window of each.
//!
//! It exists so the Account tab has something to show **before** the first live fetch: a row, a
//! picker group and the supported-models popup all need a model list, and an empty one would make
//! a freshly signed-in account look broken.
//!
//! Two things this is not:
//!
//! * It is not [`super::AccountView::models`]. That field is the **fetched** set — what the
//!   provider actually said it serves — and the popup unions the two for its count so both
//!   numbers come from one list. Merging here would make the fetched set look complete when it
//!   may not be.
//! * It is not used for every provider equally. Antigravity has a real endpoint
//!   (`ModelList::Fetch`), so its entry is just the spec's defaults, used until the first fetch
//!   lands. Codex and Claude have no documented list endpoint, so for them this table *is* the
//!   supported set (`ModelList::Static`); inventing a request would be a 404 that reads like a
//!   bug.
//!
//! Sources: Anthropic's model table for Claude (current and still-served models only — a retired
//! id is a 404, and one retiring within weeks is a trap), and CLIProxyAPI's model registry for the
//! ChatGPT plans and Antigravity's windows.
//!
//! Ids are lowercased and sorted so a chip strip is stable between runs, and every id is checked
//! against [`valid_model_id`] by the tests: these strings are pasted into request bodies.

use super::{valid_model_id, AccountProvider};

/// Codex models every paid ChatGPT plan includes (Plus, Pro, Team, Business, Go, and plans the
/// registry does not name).
///
/// `codex-auto-review` is served too and left out: it is Codex's approval classifier, not a model
/// to hold a conversation with.
const CODEX_PAID: &[&str] = &[
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-6-astra",
    "gpt-6-luna",
    "gpt-6-sol",
    "gpt-6.1-sol",
    "gpt-6.1-terra",
];

/// The part of [`CODEX_PAID`] the free plan includes.
const CODEX_FREE: &[&str] = &["gpt-5.6-luna", "gpt-5.6-terra", "gpt-6-luna", "gpt-6.1-terra"];

/// Claude models a Claude Code sign-in can use: the current generation and the older models still
/// served. Not Claude Sonnet 4.5 (retiring on 2026-11-30), nothing retired (Opus 4.1, the 3.x
/// line), and not the Mythos models (Project Glasswing only).
const CLAUDE: &[&str] = &[
    "claude-fable-5",
    "claude-fable-5-1",
    "claude-haiku-4-5",
    "claude-haiku-5-5",
    "claude-opus-4-5",
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-opus-5-5",
    "claude-sonnet-4-6",
    "claude-sonnet-5",
    "claude-sonnet-5-5",
];

/// Whether a ChatGPT plan (the ID token's `chatgpt_plan_type`) is the free one.
fn is_free_plan(plan: Option<&str>) -> bool {
    plan.is_some_and(|p| p.trim().eq_ignore_ascii_case("free"))
}

/// Every model id the provider is known to serve to an account on `plan`, sorted, deduped and
/// ASCII-lowercased.
///
/// `plan` only matters for Codex, where the free plan sees a subset; it is the account's `plan`
/// field, which for Claude holds the organization name and is ignored. Antigravity's entry is its
/// spec's defaults: the placeholder until the first fetch lands.
///
/// Pure; it is a handful of strings, so no caching layer and no allocation anxiety. A caller that
/// wants to show it beside a fetched list should union the two (see the models popup), not call
/// this twice.
pub fn models_for(p: AccountProvider, plan: Option<&str>) -> Vec<String> {
    let ids: &[&str] = match p {
        AccountProvider::Antigravity => p.spec().default_models,
        AccountProvider::Codex if is_free_plan(plan) => CODEX_FREE,
        AccountProvider::Codex => CODEX_PAID,
        AccountProvider::ClaudeCode => CLAUDE,
    };
    let mut out: Vec<String> = ids.iter().map(|m| m.trim().to_ascii_lowercase()).filter(|m| valid_model_id(m)).collect();
    out.sort();
    out.dedup();
    out
}

/// [`models_for`] when the plan is not known: for Codex, the paid plans' list.
pub fn models(p: AccountProvider) -> Vec<String> {
    models_for(p, None)
}

/// The input window of `model` on this provider, in tokens: what compaction keeps a conversation
/// under. `None` for a model this table does not know, where the caller falls back to the wire
/// kind's (conservative) default rather than guess high — too small a window only compacts
/// earlier, too large a one fails the turn once the conversation outgrows the real limit.
///
/// The same model id can have a different window behind a different door: Antigravity serves its
/// Claude 4.6 models at 200K where Anthropic's API offers a million, and ChatGPT Pro gets a larger
/// window on the GPT-5.6 models than the other plans.
pub fn context_window(p: AccountProvider, model: &str, plan: Option<&str>) -> Option<u64> {
    let m = model.trim().to_ascii_lowercase();
    match p {
        AccountProvider::Codex => codex_window(&m, plan),
        AccountProvider::ClaudeCode => claude_window(&m),
        AccountProvider::Antigravity => antigravity_window(&m),
    }
}

fn codex_window(m: &str, plan: Option<&str>) -> Option<u64> {
    // Only a plan that says so gets Pro's window: an unknown plan is held to the smaller one.
    let pro = plan.is_some_and(|p| p.trim().eq_ignore_ascii_case("pro"));
    match m {
        "gpt-6.1-terra" => Some(500_000),
        "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna" => Some(if pro { 921_000 } else { 372_000 }),
        "gpt-6-astra" | "gpt-6-sol" | "gpt-6.1-sol" | "gpt-6-luna" | "codex-auto-review" => Some(272_000),
        _ => None,
    }
}

fn claude_window(m: &str) -> Option<u64> {
    match m {
        "claude-fable-5-1" | "claude-fable-5" | "claude-opus-5-5" | "claude-opus-5" | "claude-opus-4-8"
        | "claude-opus-4-7" | "claude-opus-4-6" | "claude-sonnet-5-5" | "claude-sonnet-5" | "claude-haiku-5-5" => {
            Some(1_000_000)
        }
        // Sonnet 4.6's million is not confirmed for a subscription token (CLIProxyAPI lists it at
        // 200K for exactly this sign-in), so it is held to the window that certainly works.
        "claude-sonnet-4-6" | "claude-opus-4-5" | "claude-haiku-4-5" => Some(200_000),
        _ => None,
    }
}

fn antigravity_window(m: &str) -> Option<u64> {
    if let Some(rest) = m.strip_prefix("claude-") {
        // Antigravity's own ids carry a suffix (`-thinking`, `-high`); the generation decides:
        // `opus-4-6-thinking` is a 4, `sonnet-5-5-high` a 5.
        let generation = rest.split('-').find_map(|part| part.parse::<u32>().ok())?;
        return Some(if generation >= 5 { 1_000_000 } else { 200_000 });
    }
    if m.starts_with("gpt-oss") {
        return Some(114_000);
    }
    // An image model's window is not published here; it falls to the conservative default.
    if m.starts_with("gemini-") && !m.contains("image") {
        return Some(1_048_576);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [AccountProvider; 3] = [AccountProvider::Antigravity, AccountProvider::Codex, AccountProvider::ClaudeCode];

    #[test]
    fn every_catalog_entry_is_a_valid_lowercase_model_id() {
        for p in ALL {
            for plan in [None, Some("free"), Some("plus"), Some("pro")] {
                let all = models_for(p, plan);
                assert!(!all.is_empty(), "{p:?} {plan:?}");
                for m in all {
                    assert!(valid_model_id(&m), "{p:?}: {m:?}");
                    assert_eq!(m.to_ascii_lowercase(), m, "{p:?}: {m:?} must be lowercase");
                }
            }
        }
    }

    #[test]
    fn the_catalog_is_sorted_deduped_and_contains_the_defaults() {
        for p in ALL {
            let list = models(p);
            let mut sorted = list.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(list, sorted, "{p:?}: sorted and deduped");
            for d in p.spec().default_models {
                assert!(list.contains(&d.to_ascii_lowercase()), "{p:?} is missing its default {d}");
            }
        }
    }

    #[test]
    fn every_catalog_model_has_a_known_window() {
        // A listed model without a window would run with the kind's fallback, which is usually
        // far smaller than it needs to be.
        for p in [AccountProvider::Codex, AccountProvider::ClaudeCode] {
            for m in models(p) {
                assert!(context_window(p, &m, None).is_some(), "{p:?}: {m}");
            }
        }
        for m in AccountProvider::Antigravity.spec().default_models {
            assert!(context_window(AccountProvider::Antigravity, m, None).is_some(), "{m}");
        }
    }

    #[test]
    fn the_free_chatgpt_plan_sees_its_own_subset() {
        let free = models_for(AccountProvider::Codex, Some(" Free "));
        let paid = models_for(AccountProvider::Codex, Some("plus"));
        assert!(free.len() < paid.len());
        assert!(free.iter().all(|m| paid.contains(m)), "{free:?} ⊄ {paid:?}");
        assert!(!free.contains(&"gpt-6.1-sol".to_owned()));
        assert_eq!(models_for(AccountProvider::Codex, Some("enterprise")), paid, "an unnamed plan is a paid one");
        // A free account does not land on a model its plan lacks.
        let spec = AccountProvider::Codex.spec();
        assert_eq!(super::super::default_model(spec, &free).as_deref(), Some("gpt-6.1-terra"));
        assert_eq!(super::super::default_model(spec, &paid).as_deref(), Some("gpt-6.1-sol"));
    }

    #[test]
    fn no_retired_or_retiring_claude_model_is_offered() {
        let list = models(AccountProvider::ClaudeCode);
        for gone in ["claude-sonnet-4-5", "claude-opus-4-1", "claude-haiku-4-6", "claude-3-7-sonnet-latest", "claude-3-5-haiku-latest"] {
            assert!(!list.iter().any(|m| m == gone), "{gone}");
        }
        assert!(list.iter().all(|m| !m.contains("mythos")), "Glasswing-only models are not offered");
    }

    #[test]
    fn windows_follow_the_model_and_the_door_it_is_reached_through() {
        use AccountProvider::*;
        assert_eq!(context_window(ClaudeCode, "claude-opus-5-5", None), Some(1_000_000));
        assert_eq!(context_window(ClaudeCode, " Claude-Haiku-4-5 ", None), Some(200_000), "trimmed, any case");
        assert_eq!(context_window(ClaudeCode, "claude-next-9", None), None);
        assert_eq!(context_window(Codex, "gpt-6.1-terra", Some("plus")), Some(500_000));
        assert_eq!(context_window(Codex, "gpt-5.6-sol", Some("pro")), Some(921_000));
        assert_eq!(context_window(Codex, "gpt-5.6-sol", Some("plus")), Some(372_000));
        assert_eq!(context_window(Codex, "gpt-5.6-sol", None), Some(372_000), "an unknown plan gets the smaller window");
        assert_eq!(context_window(Codex, "auto", None), None);
        // Antigravity serves Claude 4.6 at 200K and the 5 generation at a million.
        assert_eq!(context_window(Antigravity, "claude-opus-4-6-thinking", None), Some(200_000));
        assert_eq!(context_window(Antigravity, "claude-sonnet-5-5-high", None), Some(1_000_000));
        assert_eq!(context_window(Antigravity, "gemini-3-flash", None), Some(1_048_576));
        assert_eq!(context_window(Antigravity, "gpt-oss-120b-medium", None), Some(114_000));
        assert_eq!(context_window(Antigravity, "gemini-3.1-flash-image", None), None);
        assert_eq!(context_window(Antigravity, "claude-", None), None);
    }
}
