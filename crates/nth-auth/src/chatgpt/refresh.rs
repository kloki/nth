//! Keeping the access token alive: it lasts about an hour, the refresh
//! token much longer.

use super::{CLIENT_ID, ChatGpt, Credentials};
use crate::{Error, OAuth};

/// A token this close to running out is refreshed first, so it cannot die
/// on the way to the server.
const MARGIN_MS: u64 = 60_000;

impl ChatGpt {
    /// What the next request is sent with. Reads `auth.json` each time, so a
    /// login made since this handle was built counts at once.
    pub async fn credentials(&self) -> Result<Credentials, Error> {
        let auth = self.saved().await?;
        if !auth.expires_within(MARGIN_MS) {
            return Ok(auth.into());
        }
        let _refreshing = self.inner.refreshing.lock().await;
        // Whoever held the lock before may have refreshed it already.
        let auth = self.saved().await?;
        if !auth.expires_within(MARGIN_MS) {
            return Ok(auth.into());
        }
        let tokens = self
            .token(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &auth.refresh),
                ("client_id", CLIENT_ID),
            ])
            .await?;
        let auth = tokens.into_oauth(auth.account_id);
        self.inner.store.save(Self::ID, &auth).await?;
        Ok(auth.into())
    }

    async fn saved(&self) -> Result<OAuth, Error> {
        self.inner
            .store
            .oauth(Self::ID)
            .await?
            .ok_or(Error::NotConnected)
    }
}

impl From<OAuth> for Credentials {
    fn from(auth: OAuth) -> Self {
        Self {
            access: auth.access,
            account_id: auth.account_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{super::testing::Issuer, *};
    use crate::{Store, chatgpt::jwt::token, store::now_ms};

    fn saved(expires: u64) -> OAuth {
        OAuth {
            access: "old-access".into(),
            refresh: "old-refresh".into(),
            expires,
            account_id: Some("acct".into()),
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
        assert_eq!(credentials.account_id.as_deref(), Some("acct"));
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
        assert_eq!(credentials.account_id.as_deref(), Some("acct"));

        let requests = issuer.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/oauth/token");
        assert_eq!(requests[0].form["grant_type"], "refresh_token");
        assert_eq!(requests[0].form["refresh_token"], "old-refresh");
        assert_eq!(requests[0].form["client_id"], CLIENT_ID);

        let saved = store.oauth(ChatGpt::ID).await.unwrap().unwrap();
        assert_eq!(saved.access, credentials.access);
        assert_eq!(saved.refresh, "new-refresh");
        assert_eq!(saved.account_id.as_deref(), Some("acct"));
        assert!(saved.expires > now_ms() + 7_000_000);

        // Now fresh: no second request.
        chatgpt.credentials().await.unwrap();
        assert_eq!(issuer.requests().len(), 1);
    }

    #[tokio::test]
    async fn refused_refresh_says_why() {
        let issuer = Issuer::start(401, json!({"error": "invalid_grant"})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        store.save(ChatGpt::ID, &saved(0)).await.unwrap();
        let chatgpt = ChatGpt::with_issuer(store, &issuer.url, 0).unwrap();

        let error = chatgpt.credentials().await.unwrap_err();
        assert!(
            matches!(&error, Error::Status { status: 401, body } if body.contains("invalid_grant")),
            "{error}"
        );
    }
}
