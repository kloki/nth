use std::{process::Command, time::Duration};

use nth_curios::*;
use reqwest::{Client, StatusCode};
use serde_json::json;

async fn setup() -> (tempfile::TempDir, Store, Host) {
    let repo = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(repo.path())
            .status()
            .unwrap()
            .success()
    );
    let store = Store::new(repo.path(), "http-owner").unwrap();
    let published = store
        .publish(Publish {
            request_id: "publish".into(),
            curio_id: None,
            expected_revision: None,
            title: None,
            html: "<p id='target'>Target</p>".into(),
        })
        .unwrap();
    let host = Host::start(store.clone(), &published.curio.id)
        .await
        .unwrap();
    (repo, store, host)
}

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

#[tokio::test]
async fn host_origin_fetch_metadata_and_capability_are_all_enforced() {
    let (_repo, _store, host) = setup().await;
    assert!(host.address().ip().is_loopback());
    let url = host.url();
    let (base, token) = url.split_once('#').unwrap();
    let client = client();
    let api = format!("{base}api/state");
    for request in [
        client.get(&api),
        client.get(&api).header("x-curios-token", "wrong"),
        client
            .get(&api)
            .header("x-curios-token", token)
            .header("Host", "evil.invalid"),
        client
            .get(&api)
            .header("x-curios-token", token)
            .header("Origin", "null"),
        client
            .get(&api)
            .header("x-curios-token", token)
            .header("Origin", "https://evil.invalid"),
        client
            .get(&api)
            .header("x-curios-token", token)
            .header("Sec-Fetch-Site", "cross-site"),
        client.get(format!("{api}?token={token}")),
    ] {
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    let response = client
        .get(&api)
        .header("x-curios-token", token)
        .header("Origin", base.trim_end_matches('/'))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );
    for path in ["", "viewer.js", "viewer.css", "content/1"] {
        let response = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        let body = response.text().await.unwrap();
        assert!(!body.contains(token), "capability leaked in {path}");
    }
    host.stop().await.unwrap();
    assert!(client.get(base).send().await.is_err());
}

#[tokio::test]
async fn malformed_oversized_and_invalid_mutations_preserve_durable_state() {
    let (_repo, store, host) = setup().await;
    let url = host.url();
    let (base, token) = url.split_once('#').unwrap();
    let client = client();
    let endpoint = format!("{base}api/comments");
    let before = store.state_version().unwrap();
    let malformed = client
        .post(&endpoint)
        .header("x-curios-token", token)
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    let oversized = client
        .post(&endpoint)
        .header("x-curios-token", token)
        .header("content-type", "application/json")
        .body(" ".repeat(65537))
        .send()
        .await
        .unwrap();
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let missing = client
        .post(&endpoint)
        .header("x-curios-token", token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let invalid = client
        .post(&endpoint)
        .header("x-curios-token", token)
        .json(&json!({
            "request_id":"bad", "revision":1, "body":"", "target":{"kind":"document"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        invalid.json::<serde_json::Value>().await.unwrap()["code"],
        "invalid"
    );
    for query in [
        "revision=-1",
        "revision=banana",
        "revision=18446744073709551616",
    ] {
        assert_eq!(
            client
                .get(format!("{base}api/state?{query}"))
                .header("x-curios-token", token)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(store.state_version().unwrap(), before);
    host.stop().await.unwrap();
}

#[tokio::test]
async fn browser_mutations_persist_replay_and_cannot_impersonate_agent() {
    let (repo, store, host) = setup().await;
    let url = host.url();
    let (base, token) = url.split_once('#').unwrap();
    let client = client();
    let comment = json!({"request_id":"comment", "revision":1, "body":"<script>quoted text</script>",
        "author":"agent", "target":{"kind":"element", "element_id":"target"}});
    let first = client
        .post(format!("{base}api/comments"))
        .header("x-curios-token", token)
        .json(&comment)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let thread: Thread = first.json().await.unwrap();
    assert_eq!(thread.messages[0].author, Author::Human);
    let replay: Thread = client
        .post(format!("{base}api/comments"))
        .header("x-curios-token", token)
        .json(&comment)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay.id, thread.id);
    let reply = json!({"request_id":"reply", "expected_version":1, "body":"Impersonation", "mark_addressed":true, "author":"agent"});
    let response = client
        .post(format!("{base}api/threads/{}/messages", thread.id))
        .header("x-curios-token", token)
        .json(&reply)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let send = json!({"request_id":"send", "message_ids":[thread.messages[0].id]});
    let response = client
        .post(format!("{base}api/send"))
        .header("x-curios-token", token)
        .json(&send)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let batch: Batch = response.json().await.unwrap();
    assert_eq!(batch.session_id, "http-owner");
    host.stop().await.unwrap();
    let reopened = Store::new(repo.path(), "http-owner").unwrap();
    assert_eq!(reopened.batches(true).unwrap()[0].id, batch.id);
    let cid = store.list().unwrap()[0].id.clone();
    assert_eq!(
        reopened.snapshot(&cid, None).unwrap().threads[0].messages[0].delivery,
        Delivery::Queued
    );
}
