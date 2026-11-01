//! What nth reads from a token: the account id and email it names, and for
//! the ID token of a login, whether the issuer really signed it for this
//! sign-in.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation,
    jwk::{Jwk, JwkSet},
};
use serde::Deserialize;

use crate::Error;

pub(crate) type Jwks = JwkSet;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Claims {
    chatgpt_account_id: Option<String>,
    #[serde(rename = "https://api.openai.com/auth")]
    auth: Option<AuthClaims>,
    #[serde(default)]
    organizations: Vec<Organization>,
    pub(crate) email: Option<String>,
    nonce: Option<String>,
    sub: Option<String>,
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
    /// The user the token names, once it is empty or only space.
    pub(crate) fn subject(&self) -> Option<&str> {
        self.sub.as_deref().filter(|sub| !sub.trim().is_empty())
    }

    /// opencode's order: the plain claim, then the namespaced one, then the
    /// first organization.
    fn account_id(self) -> Option<String> {
        let non_empty = |id: Option<String>| id.filter(|id| !id.is_empty());
        non_empty(self.chatgpt_account_id)
            .or_else(|| non_empty(self.auth.and_then(|a| a.chatgpt_account_id)))
            .or_else(|| non_empty(self.organizations.into_iter().next().map(|o| o.id)))
    }
}

/// The subject a token names, as a sign-in hint needs no more than the
/// claim itself.
pub(crate) fn subject(token: &str) -> Option<String> {
    claims(token)?.subject().map(String::from)
}

/// The ChatGPT account the tokens are for: from the id token, else the
/// access token.
pub(crate) fn account_id(id_token: Option<&str>, access_token: &str) -> Option<String> {
    id_token
        .and_then(claims)
        .and_then(Claims::account_id)
        .or_else(|| claims(access_token)?.account_id())
}

/// The ID token's claims, once its signature, issuer, audience, expiry and
/// nonce all check out.
pub(crate) fn verify(
    token: &str,
    jwks: &Jwks,
    issuer: &str,
    audience: &str,
    nonce: &str,
) -> Result<Claims, Error> {
    let header = jsonwebtoken::decode_header(token)
        .map_err(|error| Error::InvalidToken(error.to_string()))?;
    if header.alg != Algorithm::RS256 {
        return Err(Error::InvalidToken(
            "it is not signed with RS256".to_string(),
        ));
    }
    let key = key(jwks, header.kid.as_deref())?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[audience]);
    validation.set_required_spec_claims(&["exp", "aud", "iss", "sub"]);
    let claims = jsonwebtoken::decode::<Claims>(token, &key, &validation)
        .map_err(|error| Error::InvalidToken(error.to_string()))?
        .claims;
    if claims.nonce.as_deref() != Some(nonce) {
        return Err(Error::InvalidToken(
            "its nonce is not this sign-in's".to_string(),
        ));
    }
    if claims
        .sub
        .as_deref()
        .is_none_or(|sub| sub.trim().is_empty())
    {
        return Err(Error::InvalidToken("it names no subject".to_string()));
    }
    Ok(claims)
}

/// The key `kid` names, else the first published key that can check an
/// RS256 signature.
fn key(jwks: &Jwks, kid: Option<&str>) -> Result<DecodingKey, Error> {
    let candidates: Vec<&Jwk> = match kid {
        Some(kid) => jwks
            .keys
            .iter()
            .filter(|jwk| jwk.common.key_id.as_deref() == Some(kid))
            .collect(),
        None => jwks.keys.iter().collect(),
    };
    candidates
        .iter()
        .find_map(|jwk| DecodingKey::from_jwk(jwk).ok())
        .ok_or_else(|| Error::InvalidToken("the issuer published no key for it".to_string()))
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

    use super::{super::testing, *};

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
        assert_eq!(
            claims(&id).and_then(|c| c.email).as_deref(),
            Some("you@example.com")
        );
    }

    #[test]
    fn a_token_names_its_subject() {
        assert_eq!(
            subject(&token(json!({"sub": "user"}))).as_deref(),
            Some("user")
        );
        assert_eq!(subject(&token(json!({"sub": "  "}))), None);
        assert_eq!(subject(&token(json!({}))), None);
        assert_eq!(subject("not a jwt"), None);
    }

    #[test]
    fn a_signed_id_token_is_accepted_for_this_sign_in_only() {
        let issuer = "https://auth.example";
        let signed = testing::signed(
            json!({"sub": "user", "email": "you@example.com"}),
            issuer,
            "oaiapp_test",
            "the-nonce",
        );

        let verified = verify(
            &signed,
            &testing::jwks(),
            issuer,
            "oaiapp_test",
            "the-nonce",
        )
        .expect("valid");
        assert_eq!(verified.email.as_deref(), Some("you@example.com"));

        let error = verify(
            &signed,
            &testing::jwks(),
            issuer,
            "oaiapp_test",
            "another-nonce",
        )
        .expect_err("a nonce from another sign-in");
        assert!(error.to_string().contains("nonce"), "{error}");
    }

    #[test]
    fn an_id_token_for_another_client_is_refused() {
        let issuer = "https://auth.example";
        let signed = testing::signed(json!({"sub": "user"}), issuer, "oaiapp_other", "the-nonce");
        let error = verify(
            &signed,
            &testing::jwks(),
            issuer,
            "oaiapp_test",
            "the-nonce",
        )
        .expect_err("another audience");
        assert!(
            error.to_string().to_lowercase().contains("audience"),
            "{error}"
        );
    }

    #[test]
    fn an_id_token_without_a_subject_is_refused() {
        let issuer = "https://auth.example";
        let signed = testing::signed(json!({"sub": ""}), issuer, "oaiapp_test", "the-nonce");
        let error = verify(
            &signed,
            &testing::jwks(),
            issuer,
            "oaiapp_test",
            "the-nonce",
        )
        .expect_err("no subject");
        assert!(error.to_string().contains("subject"), "{error}");
    }
}
