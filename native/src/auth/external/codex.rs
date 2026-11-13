//! ChatGPT (Codex) sign-in: PKCE, the form token exchange and the ID token's claims.
//!
//! The flow is the plain OAuth 2.0 authorization-code one: PKCE because the client is public, a
//! form-encoded token endpoint, and an ID token. The interesting part is the ID token — it is the
//! only place the account id and the plan are named, and the account id is not decoration: every
//! generation request carries it as `Chatgpt-Account-Id`, so an account signed in without one
//! cannot send a turn.
//!
//! The claims are read, not verified. We are the client that just received the token over TLS
//! from the issuer; see `super::jwt` for the full reasoning.

use super::jwt::Claims;
use super::login::{post_exchange, Completed, Enrichment, LoginAttempt};
use crate::auth::AuthError;

/// Exchange the code and read the account out of the ID token.
///
/// An ID token that cannot be parsed is not fatal by itself, but an account with no account id and
/// no email cannot be used *or* named, so the login fails with one message that says which.
pub(crate) async fn exchange(
    http: &reqwest::Client,
    attempt: &LoginAttempt,
    code: &str,
) -> Result<Completed, AuthError> {
    let (resp, _value) = post_exchange(http, attempt, code, None).await?;
    let Some(id_token) = resp.id_token.as_deref().filter(|t| !t.is_empty()) else {
        return Err(AuthError::Denied(
            "OpenAI signed you in without an id token, so this account cannot be identified. Try again.".into(),
        ));
    };
    let claims = Claims::read(id_token).unwrap_or_default();
    let account_id = claims.chatgpt_account_id.as_deref().and_then(super::display_text);
    let plan = claims.chatgpt_plan_type.as_deref().and_then(super::display_text);
    let email = claims.email.as_deref().and_then(super::display_text);
    if account_id.is_none() && email.is_none() {
        return Err(AuthError::Denied(
            "the ID token names neither an email nor a ChatGPT account id, so this sign-in cannot be filed. Try again.".into(),
        ));
    }
    Ok(Completed { resp, enrich: Enrichment::Codex { account_id, plan, email } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::external::login::ProviderEndpoints;
    use crate::auth::external::AccountProvider;

    #[test]
    fn the_flow_uses_pkce_and_the_registered_redirect() {
        let p = AccountProvider::Codex;
        let attempt = LoginAttempt::new(1, p, ProviderEndpoints::for_spec(p.spec())).unwrap();
        let q: std::collections::HashMap<String, String> = attempt.auth_url.query_pairs().into_owned().collect();
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["redirect_uri"], "http://localhost:1455/auth/callback");
        assert_eq!(q["prompt"], "login");
        assert_eq!(q["codex_cli_simplified_flow"], "true");
        assert_eq!(attempt.verifier().unwrap().len(), 128);
    }
}
