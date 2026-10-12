//! What nth reads from the tokens without verifying them: they came straight
//! from the issuer over TLS, and only the account id and email are wanted.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
struct Claims {
    chatgpt_account_id: Option<String>,
    #[serde(rename = "https://api.openai.com/auth")]
    auth: Option<AuthClaims>,
    #[serde(default)]
    organizations: Vec<Organization>,
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthClaims {
    chatgpt_account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Organization {
    id: String,
}

fn claims(token: &str) -> Option<Claims> {
    let mut parts = token.split('.');
    let (Some(_), Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let payload = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&payload).ok()
}

impl Claims {
    /// opencode's order: the plain claim, then the namespaced one, then the
    /// first organization.
    fn account_id(self) -> Option<String> {
        let non_empty = |id: Option<String>| id.filter(|id| !id.is_empty());
        non_empty(self.chatgpt_account_id)
            .or_else(|| non_empty(self.auth.and_then(|a| a.chatgpt_account_id)))
            .or_else(|| non_empty(self.organizations.into_iter().next().map(|o| o.id)))
    }
}

/// The ChatGPT account the tokens are for: from the id token, else the
/// access token.
pub(crate) fn account_id(id_token: Option<&str>, access_token: &str) -> Option<String> {
    id_token
        .and_then(claims)
        .and_then(Claims::account_id)
        .or_else(|| claims(access_token)?.account_id())
}

pub(crate) fn email(id_token: &str) -> Option<String> {
    claims(id_token)?.email
}

#[cfg(test)]
pub(crate) fn token(claims: serde_json::Value) -> String {
    format!(
        "e30.{}.sig",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn plain_claim_comes_first() {
        let id = token(json!({
            "chatgpt_account_id": "plain",
            "https://api.openai.com/auth": {"chatgpt_account_id": "namespaced"},
            "organizations": [{"id": "org"}],
        }));
        assert_eq!(account_id(Some(&id), "").as_deref(), Some("plain"));
    }

    #[test]
    fn namespaced_claim_before_organizations() {
        let id = token(json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "namespaced"},
            "organizations": [{"id": "org"}],
        }));
        assert_eq!(account_id(Some(&id), "").as_deref(), Some("namespaced"));
    }

    #[test]
    fn first_organization_last() {
        let id = token(json!({"organizations": [{"id": "org"}, {"id": "other"}]}));
        assert_eq!(account_id(Some(&id), "").as_deref(), Some("org"));
    }

    #[test]
    fn access_token_when_the_id_token_says_nothing() {
        let id = token(json!({"email": "you@example.com"}));
        let access = token(json!({"chatgpt_account_id": "from-access"}));
        assert_eq!(
            account_id(Some(&id), &access).as_deref(),
            Some("from-access")
        );
        assert_eq!(account_id(None, &access).as_deref(), Some("from-access"));
        assert_eq!(account_id(Some("not a jwt"), "nor this"), None);
        assert_eq!(email(&id).as_deref(), Some("you@example.com"));
    }
}
