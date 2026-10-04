//! The process-wide set of language server clients, started lazily: one per
//! (server id, project root), the first time a file there is touched.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use futures::future::join_all;
use tokio::{
    sync::{OnceCell, watch},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use crate::{
    client::{Client, Error},
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
    /// It failed to start or died, and is not tried again.
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
/// Set once by whoever touches its (id, root) first, while the rest wait
/// on it. `None` inside means it could not start, for good, which is
/// opencode's `broken` set.
type Slot = Arc<OnceCell<Option<Arc<Client>>>>;

/// Cheap to clone; every clone shares the same clients. Dropping the last
/// one stops them all.
#[derive(Clone)]
pub struct Lsp {
    inner: Arc<Inner>,
}

struct Inner {
    servers: Arc<[Server]>,
    clients: Mutex<HashMap<Key, Slot>>,
    status: watch::Sender<Vec<ServerStatus>>,
    cancel: CancellationToken,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Lsp {
    pub fn new(config: &LspConfig) -> Self {
        Self::with_servers(server::registry(config))
    }

    fn with_servers(servers: Vec<Server>) -> Self {
        Self {
            inner: Arc::new(Inner {
                servers: servers.into(),
                clients: Mutex::new(HashMap::new()),
                status: watch::Sender::new(Vec::new()),
                cancel: CancellationToken::new(),
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
        let path = match std::path::absolute(path) {
            Ok(path) => path,
            Err(_) => return BTreeMap::new(),
        };
        let clients = self.clients_for(&path).await;
        join_all(clients.iter().map(|client| async {
            let after = Instant::now();
            match client.open_or_change(&path).await {
                Ok(version) if wait => client.wait_for_diagnostics(&path, version, after).await,
                Ok(_) => {}
                Err(Error::Closed) => self.set_state(
                    client.id(),
                    client.root(),
                    ServerState::Broken("exited".into()),
                ),
                // The file vanished or is not text: nothing to report.
                Err(_) => {}
            }
        }))
        .await;

        let mut all: BTreeMap<PathBuf, Vec<Diagnostic>> = BTreeMap::new();
        for client in &clients {
            for (file, diagnostics) in client.diagnostics() {
                all.entry(file).or_default().extend(diagnostics);
            }
        }
        all
    }

    async fn clients_for(&self, path: &Path) -> Vec<Arc<Client>> {
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
                .map(|(server, root)| self.client(server, root)),
        )
        .await;
        started.into_iter().flatten().collect()
    }

    async fn client(&self, server: Server, root: PathBuf) -> Option<Arc<Client>> {
        let cell = {
            let mut clients = self
                .inner
                .clients
                .lock()
                .expect("lsp client map lock poisoned");
            clients
                .entry((server.id.clone(), root.clone()))
                .or_default()
                .clone()
        };
        cell.get_or_init(|| self.start(server, root)).await.clone()
    }

    async fn start(&self, server: Server, root: PathBuf) -> Option<Arc<Client>> {
        let launch = {
            let (server, root) = (server.clone(), root.clone());
            tokio::task::spawn_blocking(move || server.launch(&root))
                .await
                .ok()??
        };
        self.set_state(&server.id, &root, ServerState::Starting);
        match Client::spawn(&server.id, launch, self.inner.cancel.child_token()).await {
            Ok(client) => {
                self.set_state(&server.id, &root, ServerState::Connected);
                Some(Arc::new(client))
            }
            Err(e) => {
                self.set_state(&server.id, &root, ServerState::Broken(e.to_string()));
                None
            }
        }
    }

    fn set_state(&self, id: &str, root: &Path, state: ServerState) {
        self.inner.status.send_modify(|all| {
            match all.iter_mut().find(|s| s.id == id && s.root == root) {
                Some(status) => status.state = state,
                None => all.push(ServerStatus {
                    id: id.to_string(),
                    root: root.to_path_buf(),
                    state,
                }),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;

    fn config_with(id: &str, command: &[&str], extensions: &[&str]) -> LspConfig {
        let mut config = LspConfig {
            enabled: true,
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
