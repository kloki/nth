//! Keeping the access token alive: it lasts about an hour, the refresh
//! token much longer.

use super::{ChatGpt, Credentials, RESOURCE};
use crate::{Error, OAuth};

/// A token this close to running out is refreshed first, so it cannot die
/// on the way to the server.
const MARGIN_MS: u64 = 60_000;

impl ChatGpt {
    /// What the next request is sent with. Reads `auth.json` each time, so a
    /// login made since this handle was built counts at once.
    pub async fn credentials(&self) -> Result<Credentials, Error> {
        let auth = self.saved().await?;
        let client_id = auth
            .client_id
            .clone()
            .filter(|client_id| !client_id.trim().is_empty())
            .ok_or(Error::Reconnect)?;
        if !auth.expires_within(MARGIN_MS) {
            return Ok(auth.into());
        }
        let _refreshing = self.inner.refreshing.lock().await;
        // Whoever held the lock before may have refreshed it already.
        let auth = self.saved().await?;
        if !auth.expires_within(MARGIN_MS) {
            return Ok(auth.into());
        }
        let tokens = match self
            .token(&[
                ("grant_type", "refresh_token"),
                ("client_id", &client_id),
                ("refresh_token", &auth.refresh),
                ("resource", RESOURCE),
            ])
            .await
        {
            Ok(tokens) => tokens,
            Err(Error::Status { status, body }) if unusable_refresh(status, &body) => {
                return Err(Error::Reconnect);
            }
            Err(error) => return Err(error),
        };
        let auth = tokens.into_oauth(Some(&auth), client_id);
        self.inner.store.save(Self::ID, &auth).await?;
        Ok(auth.into())
    }

    async fn saved(&self) -> Result<OAuth, Error> {
        self.inner
            .store
            .oauth(Self::ID)
            .await?
            .filter(OAuth::has_session)
            .ok_or(Error::NotConnected)
    }
}

impl From<OAuth> for Credentials {
    fn from(auth: OAuth) -> Self {
        Self {
            access: auth.access,
        }
    }
}

/// Whether OpenAI's refusal means the refresh token is gone and only a new
/// sign-in helps. `invalid_client` is different: the issued client ID is
/// wrong, which signing in again does not fix.
fn unusable_refresh(status: u16, body: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Refusal {
        error: String,
    }
    let Ok(refusal) = serde_json::from_str::<Refusal>(body) else {
        return false;
    };
    matches!(status, 400 | 401)
        && matches!(
            refusal.error.as_str(),
            "invalid_grant"
                | "invalid_refresh_token"
                | "token_expired"
                | "refresh_token_expired"
                | "refresh_token_invalidated"
                | "refresh_token_reused"
        )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        super::{
            RESOURCE, TOKEN,
            testing::{Issuer, signed},
        },
        *,
    };
    use crate::{Store, chatgpt::jwt::token, store::now_ms};

    fn saved(expires: u64) -> OAuth {
        OAuth {
            access: "old-access".into(),
            refresh: "old-refresh".into(),
            expires,
            account_id: Some("acct".into()),
            client_id: Some("oaiapp_saved".into()),
            id_token: None,
            scopes: vec!["openid".into()],
        }
    }

    #[tokio::test]
    async fn not_connected_without_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let chatgpt = ChatGpt::new(Store::at(dir.path().join("auth.json"))).unwrap();
        assert!(!chatgpt.connected());
        let error = chatgpt.credentials().await.unwrap_err();
        assert!(matches!(error, Error::NotConnected));
        assert_eq!(
            error.to_string(),
            "ChatGPT is not connected: run /connect or `nth connect`"
        );
    }

    #[tokio::test]
    async fn a_login_without_a_client_id_must_be_made_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        let mut legacy = saved(now_ms() + 3_600_000);
        legacy.client_id = None;
        store.save(ChatGpt::ID, &legacy).await.unwrap();
        let chatgpt = ChatGpt::with_issuer(store, "http://127.0.0.1:1", 0).unwrap();

        assert!(!chatgpt.connected());
        let error = chatgpt.credentials().await.unwrap_err();
        assert!(matches!(error, Error::Reconnect), "{error}");
        assert!(error.to_string().contains("sign in again"), "{error}");
    }

    #[tokio::test]
    async fn fresh_tokens_are_used_as_saved() {
        let issuer = Issuer::start(500, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        store
            .save(ChatGpt::ID, &saved(now_ms() + 3_600_000))
            .await
            .unwrap();
        let chatgpt = ChatGpt::with_issuer(store, &issuer.url, 0).unwrap();

        assert!(chatgpt.connected());
        let credentials = chatgpt.credentials().await.unwrap();
        assert_eq!(credentials.access, "old-access");
        assert!(issuer.requests().is_empty());
    }

    #[tokio::test]
    async fn expiring_tokens_are_refreshed_and_saved() {
        // The new tokens name no account, so the saved one is kept.
        let issuer = Issuer::start(
            200,
            json!({
                "access_token": token(json!({})),
                "refresh_token": "new-refresh",
                "expires_in": 7200,
            }),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        store
            .save(ChatGpt::ID, &saved(now_ms() + 30_000))
            .await
            .unwrap();
        let chatgpt = ChatGpt::with_issuer(store.clone(), &issuer.url, 0).unwrap();

        let credentials = chatgpt.credentials().await.unwrap();
        assert_eq!(credentials.access, token(json!({})));

        let requests = issuer.posts();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, TOKEN);
        assert_eq!(requests[0].form["grant_type"], "refresh_token");
        assert_eq!(requests[0].form["refresh_token"], "old-refresh");
        assert_eq!(requests[0].form["client_id"], "oaiapp_saved");
        assert_eq!(requests[0].form["resource"], RESOURCE);
        assert!(
            !requests[0].form.contains_key("scope"),
            "a refresh keeps the scopes the login granted"
        );

        let saved = store.oauth(ChatGpt::ID).await.unwrap().unwrap();
        assert_eq!(saved.access, credentials.access);
        assert_eq!(saved.refresh, "new-refresh");
        assert_eq!(saved.client_id.as_deref(), Some("oaiapp_saved"));
        assert_eq!(saved.account_id.as_deref(), Some("acct"));
        assert_eq!(saved.scopes, ["openid"]);
        assert!(saved.expires > now_ms() + 7_000_000);

        // Now fresh: no second request.
        chatgpt.credentials().await.unwrap();
        assert_eq!(issuer.posts().len(), 1);
    }

    #[tokio::test]
    async fn a_refresh_checks_and_keeps_the_new_id_token() {
        let issuer = Issuer::start(200, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        store
            .save(ChatGpt::ID, &saved(now_ms() + 30_000))
            .await
            .unwrap();
        let chatgpt = ChatGpt::with_issuer(store.clone(), &issuer.url, 0).unwrap();
        let id_token = signed(
            json!({"email": "you@example.com"}),
            &issuer.url,
            "oaiapp_saved",
            "unchecked-on-refresh",
        );
        issuer.respond(
            200,
            json!({
                "id_token": id_token,
                "access_token": token(json!({})),
                "refresh_token": "new-refresh",
                "expires_in": 7200,
                "scope": "openid chatgpt.tokens.use.direct",
            }),
        );

        chatgpt.credentials().await.unwrap();
        let saved = store.oauth(ChatGpt::ID).await.unwrap().unwrap();
        assert_eq!(saved.id_token.as_deref(), Some(id_token.as_str()));
        assert_eq!(saved.scopes, ["chatgpt.tokens.use.direct", "openid"]);
    }

    #[tokio::test]
    async fn an_unusable_refresh_token_asks_for_a_new_sign_in() {
        for (status, error) in [
            (400, "invalid_grant"),
            (401, "invalid_refresh_token"),
            (400, "refresh_token_reused"),
        ] {
            let issuer = Issuer::start(status, json!({"error": error})).await;
            let dir = tempfile::tempdir().unwrap();
            let store = Store::at(dir.path().join("auth.json"));
            store.save(ChatGpt::ID, &saved(0)).await.unwrap();
            let chatgpt = ChatGpt::with_issuer(store, &issuer.url, 0).unwrap();

            let error = chatgpt.credentials().await.unwrap_err();
            assert!(matches!(error, Error::Reconnect), "{status} {error}");
        }
    }

    #[tokio::test]
    async fn a_misconfigured_client_is_not_a_reason_to_sign_in_again() {
        let issuer = Issuer::start(401, json!({"error": "invalid_client"})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        store.save(ChatGpt::ID, &saved(0)).await.unwrap();
        let chatgpt = ChatGpt::with_issuer(store, &issuer.url, 0).unwrap();

        let error = chatgpt.credentials().await.unwrap_err();
        assert!(
            matches!(&error, Error::Status { status: 401, body } if body.contains("invalid_client")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn refused_refresh_says_why() {
        let issuer = Issuer::start(500, json!({"error": "temporarily_unavailable"})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        store.save(ChatGpt::ID, &saved(0)).await.unwrap();
        let chatgpt = ChatGpt::with_issuer(store, &issuer.url, 0).unwrap();

        let error = chatgpt.credentials().await.unwrap_err();
        assert!(
            matches!(&error, Error::Status { status: 500, body } if body.contains("temporarily_unavailable")),
            "{error}"
        );
    }
}
