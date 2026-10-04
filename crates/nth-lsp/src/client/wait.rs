//! Waiting for a file's diagnostics after a touch, as opencode's
//! `waitForDocumentDiagnostics` does: pull them where the server allows it,
//! otherwise wait for a fresh push to settle.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use futures::{StreamExt, stream::FuturesUnordered};
use serde_json::json;
use tokio::{
    sync::{oneshot, watch},
    time::{Instant, sleep_until},
};

use super::{Client, Command, Store, dedup};
use crate::{
    types::{Diagnostic, DocumentDiagnosticReport},
    uri,
};

/// How long pushes must stay quiet before they count as settled.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(150);
/// The most a touch waits for one file's diagnostics.
pub(crate) const DOCUMENT_WAIT: Duration = Duration::from_millis(5_000);
/// The most a touch waits when the server says it is still loading the
/// project. A cold rust-analyzer answers nothing useful until it has run
/// `cargo metadata` and indexed the crate graph, which takes about as long
/// as it may take to start, so it gets the initialize budget.
pub(crate) const COLD_WAIT: Duration = super::INITIALIZE_TIMEOUT;
const REQUEST_TIMEOUT: Duration = Duration::from_millis(3_000);

/// One `textDocument/diagnostic` answer, by file.
#[derive(Default)]
struct Pulled {
    /// The server gave a full report.
    handled: bool,
    /// ... and it covered the file asked about.
    matched: bool,
    by_file: BTreeMap<PathBuf, Vec<Diagnostic>>,
}

impl Client {
    /// Returns once diagnostics for `version` of `path` are in, or after
    /// [`DOCUMENT_WAIT`] from `after` (the moment of the touch) with
    /// whatever there is; [`COLD_WAIT`] if the server is still loading the
    /// project. Read them with [`Client::diagnostics`].
    pub async fn wait_for_diagnostics(&self, path: &Path, version: i32, after: Instant) {
        let mut push = std::pin::pin!(fresh_push(
            self.store.clone(),
            path.to_path_buf(),
            version,
            after,
        ));
        let mut changes = self.store.clone();
        let mut deadline = after + DOCUMENT_WAIT;
        let mut seen = Wake::of(&changes.borrow_and_update());
        let mut pull = true;
        loop {
            if seen.loading {
                deadline = deadline.max(after + COLD_WAIT);
            }
            if pull {
                if Instant::now() >= deadline {
                    return;
                }
                if self.pull_document(path).await && self.pulled_an_answer(path) {
                    return;
                }
            }
            tokio::select! {
                () = &mut push => return,
                () = sleep_until(deadline) => return,
                now = woken(&mut changes, seen) => {
                    let Some(now) = now else {
                        return;
                    };
                    // A server that starts loading only extends the wait;
                    // it is asked again once it has finished.
                    pull = now.registrations != seen.registrations
                        || (seen.loading && !now.loading);
                    seen = now;
                }
            }
        }
    }

    /// Whether a pull that covered `path` really answered: an empty report
    /// from a server still loading the project only means it has not looked
    /// yet, so it is pulled again once loading ends.
    fn pulled_an_answer(&self, path: &Path) -> bool {
        let store = self.store.borrow();
        !store.loading() || store.pull.get(path).is_some_and(|d| !d.is_empty())
    }

    /// Pulls the file's diagnostics from every source the server offers,
    /// and stores them. True when one of the answers covered the file.
    async fn pull_document(&self, path: &Path) -> bool {
        let identifiers = {
            let store = self.store.borrow();
            let document: Vec<_> = store
                .registrations
                .values()
                .filter(|r| {
                    !r.register_options
                        .as_ref()
                        .is_some_and(|o| o.workspace_diagnostics)
                })
                .collect();
            if !self.pull && document.is_empty() {
                return false;
            }
            let mut identifiers: Vec<String> = document
                .iter()
                .filter_map(|r| r.register_options.as_ref()?.identifier.clone())
                .collect();
            identifiers.sort();
            identifiers.dedup();
            identifiers
        };

        let mut requests: FuturesUnordered<_> = std::iter::once(None)
            .chain(identifiers.into_iter().map(Some))
            .map(|identifier| self.pull_report(path, identifier))
            .collect();
        let mut results = Vec::new();
        let mut matched = false;
        // Done as soon as one answer has errors for the file; the slower
        // ones are not worth waiting for.
        while let Some(result) = requests.next().await {
            results.push(result);
            matched = self.store_pulled(path, &results).await;
            if results
                .iter()
                .any(|r| r.by_file.get(path).is_some_and(|d| !d.is_empty()))
            {
                break;
            }
        }
        matched
    }

    async fn pull_report(&self, path: &Path, identifier: Option<String>) -> Pulled {
        let mut params = json!({ "textDocument": { "uri": uri::from_path(path) } });
        if let Some(identifier) = identifier {
            params["identifier"] = json!(identifier);
        }
        let request = self.request("textDocument/diagnostic", params);
        let Ok(Ok(report)) = tokio::time::timeout(REQUEST_TIMEOUT, request).await else {
            return Pulled::default();
        };
        let Ok(report) = serde_json::from_value::<DocumentDiagnosticReport>(report) else {
            return Pulled::default();
        };
        let mut pulled = Pulled::default();
        if let Some(items) = report.items {
            pulled
                .by_file
                .entry(path.to_path_buf())
                .or_default()
                .extend(items);
            pulled.handled = true;
            pulled.matched = true;
        }
        for (uri, related) in report.related_documents {
            let (Some(related_path), Some(items)) = (uri::to_path(&uri), related.items) else {
                continue;
            };
            pulled.matched |= related_path == path;
            pulled.handled = true;
            pulled
                .by_file
                .entry(related_path)
                .or_default()
                .extend(items);
        }
        pulled
    }

    /// Hands what was pulled to the task; true when it covered `path`.
    async fn store_pulled(&self, path: &Path, results: &[Pulled]) -> bool {
        if !results.iter().any(|r| r.handled) {
            return false;
        }
        let matched = results.iter().any(|r| r.matched);
        let mut merged: BTreeMap<PathBuf, Vec<Diagnostic>> = BTreeMap::new();
        for result in results {
            for (file, items) in &result.by_file {
                merged
                    .entry(file.clone())
                    .or_default()
                    .extend(items.iter().cloned());
            }
        }
        if matched {
            merged.entry(path.to_path_buf()).or_default();
        }
        let merged = merged
            .into_iter()
            .map(|(file, items)| (file, dedup(items)))
            .collect();
        let (stored, done) = oneshot::channel();
        let command = Command::Pulled {
            by_file: merged,
            stored,
        };
        // A closed task has nothing left to store into.
        if self.send(command).await.is_ok() {
            let _ = done.await;
        }
        matched
    }
}

/// Resolves once a push for `path` that belongs to this touch has been
/// quiet for [`DEBOUNCE`], or when the client is gone.
async fn fresh_push(
    mut store: watch::Receiver<Store>,
    path: PathBuf,
    version: i32,
    after: Instant,
) {
    loop {
        let settled_at = {
            let store = store.borrow_and_update();
            store.published.get(&path).and_then(|&(at, published)| {
                // A push for another version, or one from before the touch
                // that does not name this version, is stale.
                let stale = published.is_some_and(|v| v != version)
                    || (at < after && published != Some(version));
                if stale {
                    return None;
                }
                if store.push.get(&path).is_some_and(|d| !d.is_empty()) {
                    return Some(at + DEBOUNCE);
                }
                // An empty push from a server still loading the project is
                // no answer yet. Once it has loaded it gets a debounce to
                // publish again before the empty one counts.
                if store.loading() {
                    return None;
                }
                let since = store.quiesced_at.map_or(at, |quiesced| quiesced.max(at));
                Some(since + DEBOUNCE)
            })
        };
        tokio::select! {
            () = sleep_until(settled_at.unwrap_or(after)), if settled_at.is_some() => return,
            changed = store.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
    }
}

/// What in the [`Store`] makes a waiting pull worth repeating.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Wake {
    registrations: u64,
    loading: bool,
}

impl Wake {
    fn of(store: &Store) -> Self {
        Self {
            registrations: store.registrations_changed,
            loading: store.loading(),
        }
    }
}

/// The new [`Wake`] once the diagnostic registrations or the server's
/// loading state differ from `seen`; `None` when the client is gone.
async fn woken(store: &mut watch::Receiver<Store>, seen: Wake) -> Option<Wake> {
    loop {
        let now = Wake::of(&store.borrow_and_update());
        if now != seen {
            return Some(now);
        }
        store.changed().await.ok()?;
    }
}
