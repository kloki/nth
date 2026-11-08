//! Fake language servers and the helpers the pool's tests share.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use super::{Lsp, ServerState, ServerStatus};
use crate::{
    config::{LspConfig, ServerConfig},
    server,
};

pub fn config_with(id: &str, command: &[&str], extensions: &[&str]) -> LspConfig {
    let mut config = LspConfig {
        enabled: true,
        idle_secs: 0,
        servers: BTreeMap::new(),
    };
    config.servers.insert(
        id.into(),
        ServerConfig {
            command: command.iter().map(|s| s.to_string()).collect(),
            extensions: extensions.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        },
    );
    config
}

/// A language server as a shell script: it answers `initialize`, and
/// on every `didOpen` publishes one error for that file, versioned.
/// With `die` as its argument it exits right after publishing; with
/// `deaf` it reads nothing more once it has answered `initialize`. What
/// it writes to stderr on start shows up in the broken-state text.
const SCRIPTED_SERVER: &str = r##"#!/bin/sh
echo "scripted server here" >&2
message() {
    length=0
    while IFS= read -r line; do
        line=$(printf '%s' "$line" | tr -d '\r')
        case "$line" in
            "") break ;;
            Content-Length:*) length=${line#Content-Length: } ;;
        esac
    done
    [ "$length" -gt 0 ] || return 1
    # One byte at a time: BSD head buffers and would eat the next message.
    body=$(dd bs=1 count="$length" 2>/dev/null)
}
send() {
    printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"
}
while message; do
    case "$body" in
        *'"method":"initialize"'*)
            id=$(printf '%s' "$body" | sed 's/.*"id":\([0-9]*\).*/\1/')
            send "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{}}}"
            [ "$1" = deaf ] && exec sleep 600
            ;;
        *'"method":"textDocument/didOpen"'*)
            uri=$(printf '%s' "$body" | sed 's/.*"uri":"\([^"]*\)".*/\1/')
            range='{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}'
            item="{\"range\":$range,\"severity\":1,\"message\":\"boom\"}"
            params="{\"uri\":\"$uri\",\"version\":0,\"diagnostics\":[$item]}"
            send "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/publishDiagnostics\",\"params\":$params}"
            [ "$1" = die ] && exit 0
            ;;
    esac
done
"##;

/// A checkout with one `.nthtest` file, served by the scripted server
/// started with `args`.
pub fn scripted(args: &[&str]) -> (tempfile::TempDir, PathBuf, Lsp) {
    scripted_idling(args, None)
}

/// [`scripted`], stopping the server after `idle` untouched.
pub fn scripted_idling(args: &[&str], idle: Option<Duration>) -> (tempfile::TempDir, PathBuf, Lsp) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    let file = tmp.path().join("a.nthtest");
    std::fs::write(&file, "x").unwrap();
    let script = tmp.path().join("server.sh");
    std::fs::write(&script, SCRIPTED_SERVER).unwrap();
    // Run by `sh` rather than exec'd: another test thread forking while
    // the script was open for writing holds that fd until its own exec,
    // and exec'ing the script meanwhile fails with ETXTBSY.
    let mut command = vec!["sh", script.to_str().unwrap()];
    command.extend(args);
    let config = config_with("fake", &command, &[".nthtest"]);
    let lsp = Lsp::with_servers(server::registry(&config), idle);
    (tmp, file, lsp)
}

pub fn state_of(status: &[ServerStatus]) -> Option<ServerState> {
    status.first().map(|s| s.state.clone())
}

/// The pid of the one client the pool holds.
pub fn pid_of(lsp: &Lsp) -> Option<u32> {
    let clients = lsp.inner.clients.lock().unwrap();
    clients.values().next().and_then(|entry| entry.usage.pid)
}

/// Whether `pid` is gone or only waits to be reaped.
pub fn exited(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    stat.trim().is_empty() || stat.trim_start().starts_with('Z')
}
