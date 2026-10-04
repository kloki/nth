//! The client against a scripted server on an in-memory pipe.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{BufReader, DuplexStream, ReadHalf, WriteHalf},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use super::{
    Client,
    wait::{DEBOUNCE, DOCUMENT_WAIT},
};
use crate::{
    transport::{read_message, write_message},
    uri,
};

/// The server's end of the pipe.
struct Fake {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
}

impl Fake {
    async fn recv(&mut self) -> Value {
        let body = read_message(&mut self.reader).await.unwrap().unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    /// The next message, which must be `method`.
    async fn expect(&mut self, method: &str) -> Value {
        let message = self.recv().await;
        assert_eq!(message["method"], method, "{message}");
        message
    }

    async fn send(&mut self, message: Value) {
        write_message(&mut self.writer, &message).await.unwrap();
    }

    async fn reply(&mut self, request: &Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": request["id"], "result": result }))
            .await;
    }

    async fn publish(&mut self, path: &Path, version: Option<i32>, messages: &[&str]) {
        let mut params = json!({
            "uri": uri::from_path(path),
            "diagnostics": messages.iter().map(|m| error(m)).collect::<Vec<_>>(),
        });
        if let Some(version) = version {
            params["version"] = json!(version);
        }
        self.send(json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": params }))
            .await;
    }

    /// Reads a touch: the watched-files notification, then didOpen or
    /// didChange, which is returned.
    async fn touched(&mut self) -> Value {
        self.expect("workspace/didChangeWatchedFiles").await;
        let message = self.recv().await;
        let method = message["method"].as_str().unwrap();
        assert!(method == "textDocument/didOpen" || method == "textDocument/didChange");
        message
    }
}

fn error(message: &str) -> Value {
    json!({
        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
        "severity": 1,
        "message": message,
    })
}

const ROOT: &str = "/project";

async fn connect(capabilities: Value, initialization: Option<Value>) -> (Client, Fake) {
    let (client_end, server_end) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_end);
    let (server_read, server_write) = tokio::io::split(server_end);
    let mut fake = Fake {
        reader: BufReader::new(server_read),
        writer: server_write,
    };
    let start = Client::start(
        "fake",
        Path::new(ROOT),
        client_read,
        client_write,
        None,
        initialization.clone(),
        CancellationToken::new(),
    );
    let handshake = async {
        let init = fake.expect("initialize").await;
        assert_eq!(init["params"]["rootUri"], "file:///project");
        assert_eq!(
            init["params"]["capabilities"]["textDocument"]["diagnostic"]["dynamicRegistration"],
            true
        );
        fake.reply(&init, json!({ "capabilities": capabilities }))
            .await;
        fake.expect("initialized").await;
        if let Some(settings) = &initialization {
            let config = fake.expect("workspace/didChangeConfiguration").await;
            assert_eq!(&config["params"]["settings"], settings);
        }
    };
    let (client, ()) = tokio::join!(start, handshake);
    (client.unwrap(), fake)
}

fn source_file(text: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rs");
    std::fs::write(&path, text).unwrap();
    (dir, path)
}

fn messages(client: &Client, path: &Path) -> Vec<String> {
    client
        .diagnostics()
        .get(path)
        .map(|d| d.iter().map(|d| d.message.clone()).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn initialize_sends_options_and_configuration() {
    let (_client, _fake) =
        connect(json!({}), Some(json!({ "check": { "command": "clippy" } }))).await;
}

#[tokio::test]
async fn answers_the_server_requests() {
    let settings = json!({ "a": { "b": 2 } });
    let (_client, mut fake) = connect(json!({}), Some(settings)).await;

    let config = json!({ "jsonrpc": "2.0", "id": 7, "method": "workspace/configuration",
        "params": { "items": [{ "section": "a.b" }, {}, { "section": "nope" }] } });
    fake.send(config).await;
    let reply = fake.recv().await;
    assert_eq!(reply["id"], 7);
    assert_eq!(reply["result"], json!([2, { "a": { "b": 2 } }, null]));

    fake.send(json!({ "jsonrpc": "2.0", "id": "s1", "method": "workspace/workspaceFolders" }))
        .await;
    let reply = fake.recv().await;
    assert_eq!(reply["id"], "s1");
    assert_eq!(reply["result"][0]["uri"], "file:///project");

    for method in [
        "window/workDoneProgress/create",
        "workspace/diagnostic/refresh",
    ] {
        fake.send(json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": {} }))
            .await;
        assert_eq!(fake.recv().await["result"], Value::Null);
    }

    fake.send(json!({ "jsonrpc": "2.0", "id": 9, "method": "made/up" }))
        .await;
    assert_eq!(fake.recv().await["error"]["code"], -32601);
}

#[tokio::test]
async fn open_then_change_with_rising_versions() {
    let (client, mut fake) = connect(json!({ "textDocumentSync": 1 }), None).await;
    let (_dir, path) = source_file("fn main() {}\n");

    let (version, open) = tokio::join!(client.open_or_change(&path), fake.touched());
    assert_eq!(version.unwrap(), 0);
    assert_eq!(open["method"], "textDocument/didOpen");
    let doc = &open["params"]["textDocument"];
    assert_eq!(doc["version"], 0);
    assert_eq!(doc["languageId"], "rust");
    assert_eq!(doc["text"], "fn main() {}\n");
    assert_eq!(doc["uri"], uri::from_path(&path));

    std::fs::write(&path, "fn main() { 1 }\n").unwrap();
    for expected in [1, 2] {
        let (version, change) = tokio::join!(client.open_or_change(&path), fake.touched());
        assert_eq!(version.unwrap(), expected);
        assert_eq!(change["method"], "textDocument/didChange");
        assert_eq!(change["params"]["textDocument"]["version"], expected);
        let changes = &change["params"]["contentChanges"];
        assert_eq!(changes, &json!([{ "text": "fn main() { 1 }\n" }]));
    }
}

#[tokio::test]
async fn incremental_servers_get_a_whole_document_range() {
    let sync = json!({ "textDocumentSync": { "openClose": true, "change": 2 } });
    let (client, mut fake) = connect(sync, None).await;
    let (_dir, path) = source_file("ab\ncd");

    let _ = tokio::join!(client.open_or_change(&path), fake.touched());
    std::fs::write(&path, "new").unwrap();
    let (_, change) = tokio::join!(client.open_or_change(&path), fake.touched());
    let range = &change["params"]["contentChanges"][0]["range"];
    assert_eq!(range["start"], json!({ "line": 0, "character": 0 }));
    assert_eq!(range["end"], json!({ "line": 1, "character": 2 }));
    assert_eq!(change["params"]["contentChanges"][0]["text"], "new");
}

#[tokio::test(start_paused = true)]
async fn waits_for_pushes_to_settle() {
    let (client, mut fake) = connect(json!({}), None).await;
    let (_dir, path) = source_file("x");
    let after = Instant::now();
    let (version, _) = tokio::join!(client.open_or_change(&path), fake.touched());
    let version = version.unwrap();

    let server = async {
        fake.publish(&path, None, &["first"]).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        fake.publish(&path, None, &["second"]).await;
        fake
    };
    let (_, _fake) = tokio::join!(client.wait_for_diagnostics(&path, version, after), server);

    // The second push restarted the debounce.
    assert_eq!(after.elapsed(), Duration::from_millis(100) + DEBOUNCE);
    assert_eq!(messages(&client, &path), ["second"]);
}

#[tokio::test(start_paused = true)]
async fn gives_up_after_the_document_wait() {
    let (client, mut fake) = connect(json!({}), None).await;
    let (_dir, path) = source_file("x");
    let after = Instant::now();
    let (version, _) = tokio::join!(client.open_or_change(&path), fake.touched());

    client
        .wait_for_diagnostics(&path, version.unwrap(), after)
        .await;
    assert_eq!(after.elapsed(), DOCUMENT_WAIT);
    assert!(client.diagnostics().is_empty());
}

#[tokio::test(start_paused = true)]
async fn ignores_pushes_for_another_version() {
    let (client, mut fake) = connect(json!({}), None).await;
    let (_dir, path) = source_file("x");
    let _ = tokio::join!(client.open_or_change(&path), fake.touched());
    let after = Instant::now();
    let (version, _) = tokio::join!(client.open_or_change(&path), fake.touched());
    assert_eq!(version.as_ref().unwrap(), &1);

    let server = async {
        fake.publish(&path, Some(0), &["stale"]).await;
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        fake.publish(&path, Some(1), &["fresh"]).await;
        fake
    };
    let _ = tokio::join!(client.wait_for_diagnostics(&path, 1, after), server);
    assert_eq!(after.elapsed(), Duration::from_millis(1_000) + DEBOUNCE);
    assert_eq!(messages(&client, &path), ["fresh"]);
}

#[tokio::test(start_paused = true)]
async fn pulls_when_the_server_offers_it_and_dedups() {
    let caps = json!({ "diagnosticProvider": { "interFileDependencies": true, "workspaceDiagnostics": false } });
    let (client, mut fake) = connect(caps, None).await;
    let (dir, path) = source_file("x");
    let other = dir.path().join("lib.rs");
    let after = Instant::now();
    let (version, _) = tokio::join!(client.open_or_change(&path), fake.touched());

    let server = async {
        // A push arrives too, carrying the same error the pull will.
        fake.publish(&path, None, &["same"]).await;
        let request = fake.expect("textDocument/diagnostic").await;
        assert_eq!(
            request["params"]["textDocument"]["uri"],
            uri::from_path(&path)
        );
        let report = json!({
            "kind": "full",
            "items": [error("same"), error("pulled")],
            "relatedDocuments": { uri::from_path(&other): { "kind": "full", "items": [error("elsewhere")] } },
        });
        fake.reply(&request, report).await;
        fake
    };
    let _ = tokio::join!(
        client.wait_for_diagnostics(&path, version.unwrap(), after),
        server
    );

    // Answered by the pull, without waiting out the push debounce.
    assert!(after.elapsed() < DEBOUNCE);
    assert_eq!(messages(&client, &path), ["same", "pulled"]);
    assert_eq!(messages(&client, &other), ["elsewhere"]);
}

#[tokio::test(start_paused = true)]
async fn a_registration_enables_pulling_with_its_identifier() {
    let (client, mut fake) = connect(json!({}), None).await;
    let (_dir, path) = source_file("x");
    let after = Instant::now();
    let (version, _) = tokio::join!(client.open_or_change(&path), fake.touched());

    let server = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        fake.send(
            json!({ "jsonrpc": "2.0", "id": 1, "method": "client/registerCapability", "params": {
            "registrations": [{ "id": "r1", "method": "textDocument/diagnostic",
                "registerOptions": { "identifier": "check", "workspaceDiagnostics": false } }]
        } }),
        )
        .await;
        assert_eq!(fake.recv().await["id"], 1);
        // One pull without an identifier, one with it, in any order.
        let mut identifiers = Vec::new();
        for _ in 0..2 {
            let request = fake.expect("textDocument/diagnostic").await;
            identifiers.push(request["params"]["identifier"].clone());
            let items = if request["params"]["identifier"] == "check" {
                json!([error("checked")])
            } else {
                json!([])
            };
            fake.reply(&request, json!({ "kind": "full", "items": items }))
                .await;
        }
        identifiers.sort_by_key(|i| i.to_string());
        assert_eq!(identifiers, [json!("check"), Value::Null]);
        fake
    };
    let _ = tokio::join!(
        client.wait_for_diagnostics(&path, version.unwrap(), after),
        server
    );
    assert_eq!(after.elapsed(), Duration::from_millis(50));
    assert_eq!(messages(&client, &path), ["checked"]);
}

#[tokio::test]
async fn a_closed_connection_fails_requests() {
    let (client, fake) = connect(json!({}), None).await;
    drop(fake);
    let (_dir, path) = source_file("x");
    // The task notices the closed pipe on its next read or write.
    let result = client.open_or_change(&path).await;
    let result = match result {
        Ok(_) => client.open_or_change(&path).await,
        err => err,
    };
    assert!(result.is_err());
    tokio::time::timeout(Duration::from_secs(1), async {
        while !client.is_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
