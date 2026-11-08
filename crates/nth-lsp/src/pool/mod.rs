//! The process-wide set of language server clients, started lazily: one per
//! (server id, project root), the first time a file there is touched, and
//! stopped once none has been touched there for the configured idle time.

mod idle;
mod usage;

#[cfg(test)]
mod fake;

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Duration,
};

use futures::{
    FutureExt,
    future::{BoxFuture, Shared, join_all},
};
use idle::{Busy, stop_when_idle};
use tokio::{sync::watch, task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;
use usage::snapshot;
pub use usage::{Caller, CallerUsage, ProcessUsage};

use crate::{
    client::{Client, Error, Stderr},
    config::LspConfig,
    server::{self, Scope, Server},
    types::Diagnostic,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub id: String,
    pub root: PathBuf,
    pub state: ServerState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    Starting,
    Connected,
    /// It failed to start, and is not tried again; died, and is tried
    /// again once stopped for idling, when `[lsp] idle_secs` is set; or
    /// stopped reading what it is sent, and was killed, to be started again
    /// on the next touch.
    Broken(String),
}

/// What [`Lsp::servers_for`] reports about one server.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerInfo {
    pub id: String,
    pub extensions: Vec<String>,
    /// The program that would run, when one is on PATH.
    pub program: Option<PathBuf>,
    /// The root it would run in for a file in the directory asked about.
    pub root: Option<PathBuf>,
}

type Key = (String, PathBuf);

/// One client's start, shared by everyone who touches its (id, root). The
/// first runs it in a task of its own, since a cancelled turn drops its tool
/// futures and must not take a server that is still starting down with it;
/// the rest wait on the same outcome. `None` means it could not start, for
/// good, which is opencode's `broken` set.
type Slot = Shared<BoxFuture<'static, Option<Arc<Client>>>>;

struct Entry {
    slot: Slot,
    /// When the last touch of a file this client serves ended, which its
    /// idle time counts from.
    touched: Instant,
    /// Touches using it right now: never idle while there are any.
    busy: usize,
    usage: ProcessUsage,
}

/// Cheap to clone; every clone shares the same clients. Dropping the last
/// one stops them all; [`Lsp::shutdown`] first asks them to leave. A client
/// untouched for `idle` is stopped on its own, so a server a subagent
/// started in some other root does not outlive its use.
#[derive(Clone)]
pub struct Lsp {
    inner: Arc<Inner>,
}

struct Inner {
    servers: Arc<[Server]>,
    clients: Mutex<HashMap<Key, Entry>>,
    status: watch::Sender<Vec<ServerStatus>>,
    usage: watch::Sender<Vec<ProcessUsage>>,
    /// A usage sampler runs: `usage()` starts one when it is not set, and
    /// the sampler clears it when it stops for want of watchers.
    sampling: AtomicBool,
    /// The sampler's task, so the pool going stops it at once rather than
    /// at its next tick.
    sampler: Mutex<Option<JoinHandle<()>>>,
    cancel: CancellationToken,
    idle: Option<Duration>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Ok(sampler) = self.sampler.get_mut()
            && let Some(sampler) = sampler.take()
        {
            sampler.abort();
        }
    }
}

impl Lsp {
    pub fn new(config: &LspConfig) -> Self {
        Self::with_servers(server::registry(config), config.idle())
    }

    fn with_servers(servers: Vec<Server>, idle: Option<Duration>) -> Self {
        Self {
            inner: Arc::new(Inner {
                servers: servers.into(),
                clients: Mutex::new(HashMap::new()),
                status: watch::Sender::new(Vec::new()),
                usage: watch::Sender::new(Vec::new()),
                sampling: AtomicBool::new(false),
                sampler: Mutex::new(None),
                cancel: CancellationToken::new(),
                idle,
            }),
        }
    }

    /// Every client nth has tried to start, as it changes.
    pub fn status(&self) -> watch::Receiver<Vec<ServerStatus>> {
        self.inner.status.subscribe()
    }

    /// The servers that would handle files in `cwd`. Blocking.
    pub fn servers_for(&self, cwd: &Path) -> Vec<ServerInfo> {
        // Any file name will do: roots depend only on the directory.
        let probe = cwd.join("_");
        let scope = Scope::of(&probe);
        self.inner
            .servers
            .iter()
            .map(|server| ServerInfo {
                id: server.id.clone(),
                extensions: server.extensions.clone(),
                program: server.program().map(|(program, _)| program),
                root: server.root(&probe, &scope),
            })
            .collect()
    }

    /// Tells every server for `path` about its current text, starting them
    /// if needed. With `wait`, gives them a moment to report on it. Returns
    /// all the diagnostics those servers know of, for this file and others.
    pub async fn touch(&self, path: &Path, wait: bool) -> BTreeMap<PathBuf, Vec<Diagnostic>> {
        self.touch_inner(path, wait, None).await
    }

    pub async fn touch_attributed(
        &self,
        path: &Path,
        wait: bool,
        caller: Caller,
    ) -> BTreeMap<PathBuf, Vec<Diagnostic>> {
        self.touch_inner(path, wait, Some(caller)).await
    }

    async fn touch_inner(
        &self,
        path: &Path,
        wait: bool,
        caller: Option<Caller>,
    ) -> BTreeMap<PathBuf, Vec<Diagnostic>> {
        let path = match std::path::absolute(path) {
            Ok(path) => path,
            Err(_) => return BTreeMap::new(),
        };
        let (_busy, clients) = self.clients_for(&path, caller).await;
        let alive = join_all(clients.iter().map(|client| async {
            let after = Instant::now();
            match client.open_or_change(&path).await {
                Ok(version) if wait => client.wait_for_diagnostics(&path, version, after).await,
                Ok(_) => {}
                Err(Error::Closed) => {
                    self.broken(client, client.stderr().explain("exited"), false);
                    return false;
                }
                // Not reading what it is sent, so it never will: the next
                // touch starts it again rather than wait on it once more.
                Err(e @ Error::Timeout(_)) => {
                    self.broken(client, client.stderr().explain(&e.to_string()), true);
                    return false;
                }
                // The file vanished or is not text: nothing to report.
                Err(_) => {}
            }
            true
        }))
        .await;

        let mut all: BTreeMap<PathBuf, Vec<Diagnostic>> = BTreeMap::new();
        // A dead server's last word is not worth repeating on every write.
        for (client, _) in clients.iter().zip(alive).filter(|(_, alive)| *alive) {
            for (file, diagnostics) in client.diagnostics() {
                all.entry(file).or_default().extend(diagnostics);
            }
        }
        all
    }

    /// Asks every running server to shut down and exit, then stops their
    /// tasks, which kills any still around. For the end of a run: dropping
    /// the last `Lsp` only does the killing.
    pub async fn shutdown(&self) {
        let clients: Vec<Arc<Client>> = {
            let clients = self
                .inner
                .clients
                .lock()
                .expect("lsp client map lock poisoned");
            clients
                .values()
                .filter_map(|entry| entry.slot.peek().cloned().flatten())
                .collect()
        };
        join_all(clients.iter().map(|client| client.shutdown())).await;
        self.inner.cancel.cancel();
        let usage = {
            let mut clients = self
                .inner
                .clients
                .lock()
                .expect("lsp client map lock poisoned");
            clients.clear();
            snapshot(&clients)
        };
        self.inner.publish(usage);
        self.inner.status.send_replace(Vec::new());
    }

    async fn clients_for(
        &self,
        path: &Path,
        caller: Option<Caller>,
    ) -> (Vec<Busy>, Vec<Arc<Client>>) {
        let servers = Arc::clone(&self.inner.servers);
        let file = path.to_path_buf();
        let matches = tokio::task::spawn_blocking(move || {
            let scope = Scope::of(&file);
            servers
                .iter()
                .filter(|server| server.handles(&file))
                .filter_map(|server| {
                    let root = server.root(&file, &scope)?;
                    Some((server.clone(), root))
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();

        let started = join_all(
            matches
                .into_iter()
                .map(|(server, root)| self.client(server, root, caller.clone())),
        )
        .await;
        let (busy, clients): (Vec<_>, Vec<_>) = started.into_iter().unzip();
        (busy, clients.into_iter().flatten().collect())
    }

    async fn client(
        &self,
        server: Server,
        root: PathBuf,
        caller: Option<Caller>,
    ) -> (Busy, Option<Arc<Client>>) {
        let key = (server.id.clone(), root.clone());
        let (slot, usage) = {
            let mut clients = self
                .inner
                .clients
                .lock()
                .expect("lsp client map lock poisoned");
            let entry = clients
                .entry((server.id.clone(), root.clone()))
                .or_insert_with(|| {
                    let usage = ProcessUsage::starting(server.id.clone(), root.clone());
                    let lsp = self.clone();
                    let started = tokio::spawn(async move { lsp.start(server, root).await });
                    Entry {
                        // A start that panicked counts as a failed one.
                        slot: async move { started.await.unwrap_or(None) }
                            .boxed()
                            .shared(),
                        touched: Instant::now(),
                        busy: 0,
                        usage,
                    }
                });
            entry.enter(caller.as_ref());
            let slot = entry.slot.clone();
            (slot, snapshot(&clients))
        };
        self.inner.publish(usage);
        let busy = Busy {
            inner: Arc::clone(&self.inner),
            key,
            slot: slot.clone(),
            caller,
        };
        (busy, slot.await)
    }

    async fn start(&self, server: Server, root: PathBuf) -> Option<Arc<Client>> {
        let launch = {
            let (server, root) = (server.clone(), root.clone());
            tokio::task::spawn_blocking(move || server.launch(&root))
                .await
                .ok()??
        };
        self.set_state(&server.id, &root, ServerState::Starting);
        let stderr = Stderr::default();
        let cancel = self.inner.cancel.child_token();
        match Client::spawn(&server.id, launch, cancel.clone(), stderr.clone()).await {
            Ok(client) => {
                let usage = {
                    let mut clients = self
                        .inner
                        .clients
                        .lock()
                        .expect("lsp client map lock poisoned");
                    if let Some(entry) = clients.get_mut(&(server.id.clone(), root.clone())) {
                        entry.usage.pid = client.pid();
                    }
                    snapshot(&clients)
                };
                self.inner.publish(usage);
                self.set_state(&server.id, &root, ServerState::Connected);
                let client = Arc::new(client);
                if let Some(idle) = self.inner.idle {
                    let inner = Arc::downgrade(&self.inner);
                    let key = (server.id, root);
                    tokio::spawn(stop_when_idle(inner, key, client.clone(), idle, cancel));
                }
                Some(client)
            }
            Err(e) => {
                let reason = stderr.explain(&e.to_string());
                self.set_state(&server.id, &root, ServerState::Broken(reason));
                None
            }
        }
    }

    /// Marks `client`'s server broken for `reason`. With `evict`, also
    /// drops it from the pool and kills it, so the next touch starts the
    /// server again; the status says why until then. Nothing changes once
    /// another start has taken the client's place.
    fn broken(&self, client: &Arc<Client>, reason: String, evict: bool) {
        let key = (client.id().to_string(), client.root().to_path_buf());
        let usage = {
            let mut clients = self
                .inner
                .clients
                .lock()
                .expect("lsp client map lock poisoned");
            let Some(entry) = clients.get_mut(&key).filter(
                |entry| matches!(entry.slot.peek(), Some(Some(held)) if Arc::ptr_eq(held, client)),
            ) else {
                return;
            };
            if evict {
                clients.remove(&key);
            } else {
                entry.usage.state = ServerState::Broken(reason.clone());
            }
            // Under the lock, as when stopped for idling, so a start that
            // follows cannot have its status overwritten.
            self.set_status(&key.0, &key.1, ServerState::Broken(reason));
            // Still under the lock, so no touch can reach it in between.
            if evict {
                client.kill();
            }
            snapshot(&clients)
        };
        self.inner.publish(usage);
    }

    /// Only a change wakes the watchers: a broken server is reported again
    /// on every touch, and the front-end need not redraw for that.
    fn set_state(&self, id: &str, root: &Path, state: ServerState) {
        let usage = {
            let mut clients = self
                .inner
                .clients
                .lock()
                .expect("lsp client map lock poisoned");
            if let Some(entry) = clients.get_mut(&(id.to_string(), root.to_path_buf())) {
                entry.usage.state = state.clone();
            }
            snapshot(&clients)
        };
        self.inner.publish(usage);
        self.set_status(id, root, state);
    }

    fn set_status(&self, id: &str, root: &Path, state: ServerState) {
        self.inner.status.send_if_modified(|all| {
            match all.iter_mut().find(|s| s.id == id && s.root == root) {
                Some(status) if status.state == state => false,
                Some(status) => {
                    status.state = state;
                    true
                }
                None => {
                    all.push(ServerStatus {
                        id: id.to_string(),
                        root: root.to_path_buf(),
                        state,
                    });
                    true
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        fake::{config_with, exited, pid_of, scripted, state_of},
        *,
    };

    #[tokio::test]
    async fn a_server_that_fails_to_start_is_broken_for_good() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let file = tmp.path().join("a.nthtest");
        std::fs::write(&file, "x").unwrap();
        // `true` exits at once, so the handshake fails.
        let lsp = Lsp::new(&config_with("fake", &["true"], &[".nthtest"]));
        let status = lsp.status();

        assert!(lsp.touch(&file, true).await.is_empty());
        let after_first = status.borrow().clone();
        assert_eq!(after_first.len(), 1);
        assert_eq!(after_first[0].id, "fake");
        assert_eq!(after_first[0].root, tmp.path());
        assert!(matches!(after_first[0].state, ServerState::Broken(_)));

        // Not retried: the status does not go back to Starting.
        lsp.touch(&file, false).await;
        assert_eq!(*status.borrow(), after_first);
    }

    #[tokio::test]
    async fn a_touch_given_up_on_leaves_the_server_starting() {
        let (_tmp, file, lsp) = scripted(&[]);
        let mut status = lsp.status();

        // The turn is cancelled as soon as the server is being started.
        let touch = lsp.touch(&file, true);
        tokio::select! {
            _ = touch => panic!("the touch finished before the server was started"),
            _ = status.wait_for(|s| state_of(s) == Some(ServerState::Starting)) => {}
        }

        // The start goes on without it...
        tokio::time::timeout(
            Duration::from_secs(10),
            status.wait_for(|s| state_of(s) == Some(ServerState::Connected)),
        )
        .await
        .expect("the server came up")
        .unwrap();
        // ... and the next touch uses the client it made.
        let diagnostics = lsp.touch(&file, true).await;
        assert_eq!(diagnostics[&file][0].message, "boom");
    }

    #[tokio::test]
    async fn a_server_that_exits_takes_its_diagnostics_with_it() {
        let (_tmp, file, lsp) = scripted(&["die"]);

        let first = lsp.touch(&file, true).await;
        assert_eq!(first[&file][0].message, "boom");

        // It exited after publishing: the next touch finds it gone, and its
        // last report is not repeated.
        let again = lsp.touch(&file, true).await;
        assert!(again.is_empty(), "{again:?}");
        let status = lsp.status().borrow().clone();
        match &status[0].state {
            ServerState::Broken(reason) => {
                assert_eq!(reason, "exited: scripted server here", "{reason}")
            }
            other => panic!("{other:?}"),
        }
        assert!(lsp.touch(&file, true).await.is_empty());
    }

    #[tokio::test]
    async fn a_server_that_does_not_acknowledge_a_touch_is_broken() {
        let (tmp, file, lsp) = scripted(&["deaf"]);
        // Started on the real clock: the handshake talks to a real process.
        assert!(lsp.touch(&file, false).await.is_empty());
        assert_eq!(
            state_of(&lsp.status().borrow()),
            Some(ServerState::Connected)
        );
        let deaf = pid_of(&lsp).expect("the server runs");
        // More text than a pipe holds, so writing it blocks for good.
        let big = tmp.path().join("big.nthtest");
        std::fs::write(&big, "x".repeat(4 << 20)).unwrap();

        // Waits out the touch timeout without taking that long.
        tokio::time::pause();
        let touched =
            tokio::time::timeout(crate::client::TOUCH_TIMEOUT * 2, lsp.touch(&big, false))
                .await
                .expect("the touch gave up on the server");
        tokio::time::resume();

        assert!(touched.is_empty(), "{touched:?}");
        // What follows the reason is the server's stderr, which may not have
        // been read before the clock paused.
        match state_of(&lsp.status().borrow()) {
            Some(ServerState::Broken(why)) => {
                assert!(why.starts_with("touch timed out"), "{why}")
            }
            other => panic!("not broken: {other:?}"),
        }
        // Killed, though its task was stuck writing to it.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !exited(deaf) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the stuck server was killed");
        // The next touch starts it again rather than wait on it once more.
        tokio::time::timeout(crate::client::TOUCH_TIMEOUT / 2, lsp.touch(&file, false))
            .await
            .expect("the next touch did not wait on the stuck server");
        assert_eq!(
            state_of(&lsp.status().borrow()),
            Some(ServerState::Connected)
        );
        assert_ne!(pid_of(&lsp), Some(deaf));
    }

    #[tokio::test]
    async fn a_server_that_fails_to_start_says_why() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let file = tmp.path().join("a.nthtest");
        std::fs::write(&file, "x").unwrap();
        let command = ["sh", "-c", "echo 'no such option' >&2; exit 2"];
        let lsp = Lsp::new(&config_with("fake", &command, &[".nthtest"]));

        assert!(lsp.touch(&file, true).await.is_empty());
        let status = lsp.status().borrow().clone();
        match &status[0].state {
            ServerState::Broken(reason) => {
                assert!(reason.ends_with(": no such option"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn servers_not_on_path_are_skipped_quietly() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.nthtest");
        std::fs::write(&file, "x").unwrap();
        let lsp = Lsp::new(&config_with(
            "ghost",
            &["no-such-server-nth"],
            &[".nthtest"],
        ));
        assert!(lsp.touch(&file, true).await.is_empty());
        assert!(lsp.status().borrow().is_empty());
    }

    #[test]
    fn servers_for_lists_programs_and_roots() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let lsp = Lsp::new(&config_with("shell", &["sh"], &[".nthtest"]));
        let servers = lsp.servers_for(tmp.path());
        let shell = servers.iter().find(|s| s.id == "shell").unwrap();
        assert!(shell.program.is_some());
        assert_eq!(shell.root.as_deref(), Some(tmp.path()));
        assert!(servers.iter().any(|s| s.id == "rust"));
    }
}
