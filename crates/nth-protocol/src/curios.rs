//! Trusted owner scope, independent of a browser or front-end.
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub use nth_curios;

#[derive(Debug, Clone, Default)]
pub struct Curios(Option<Arc<Mutex<Scope>>>);

#[derive(Debug)]
struct Scope {
    cwd: PathBuf,
    session: String,
    root: Option<PathBuf>,
    recorded: BTreeSet<String>,
}

impl Curios {
    pub fn new(
        cwd: PathBuf,
        session: String,
        root: Option<PathBuf>,
        recorded: BTreeSet<String>,
    ) -> Self {
        Self(Some(Arc::new(Mutex::new(Scope {
            cwd,
            session,
            root,
            recorded,
        }))))
    }

    /// Resolves the repository only when Curios is used. No screen required.
    pub fn store(&self) -> nth_curios::Result<nth_curios::Store> {
        let mut scope = self
            .0
            .as_ref()
            .ok_or_else(|| {
                nth_curios::Error::Invalid("Curios is unavailable to this session".into())
            })?
            .lock()
            .expect("curios lock poisoned");
        let root = match &scope.root {
            Some(root) => root.clone(),
            None => nth_curios::repository_root(&scope.cwd)?,
        };
        let store = nth_curios::Store::new(&root, &scope.session)?;
        store.prepare_template()?;
        scope.root = Some(root);
        Ok(store)
    }

    pub fn root(&self) -> Option<PathBuf> {
        self.0
            .as_ref()?
            .lock()
            .expect("curios lock poisoned")
            .root
            .clone()
    }

    pub fn recorded(&self) -> BTreeSet<String> {
        self.0
            .as_ref()
            .map(|s| s.lock().expect("curios lock poisoned").recorded.clone())
            .unwrap_or_default()
    }

    /// Called in the same step that appends this typed notice to history.
    pub fn record(&self, notice: &CurioNotice) -> bool {
        let Some(scope) = &self.0 else { return false };
        let mut scope = scope.lock().expect("curios lock poisoned");
        scope.session == notice.batch.session_id && scope.recorded.insert(notice.batch.id.clone())
    }

    /// Hold the returned lease for the active session's polling lifetime.
    pub fn claim_delivery(&self) -> nth_curios::Result<CurioDelivery> {
        let store = self.store()?;
        let lease = store.claim_delivery()?;
        Ok(CurioDelivery {
            store,
            _lease: lease,
        })
    }
}

#[derive(Debug)]
pub struct CurioDelivery {
    store: nth_curios::Store,
    _lease: std::fs::File,
}

impl CurioDelivery {
    /// Polling/enqueueing never acknowledges a batch. Repeated polls dedup in Inbox.
    pub fn poll(&self, inbox: &crate::Inbox) -> nth_curios::Result<usize> {
        let mut count = 0;
        for batch in self.store.batches(true)? {
            count += usize::from(inbox.post_curio(CurioNotice { batch }));
        }
        Ok(count)
    }
}

#[derive(Debug, Clone)]
pub struct CurioNotice {
    pub batch: nth_curios::Batch,
}

impl CurioNotice {
    pub fn render(&self) -> String {
        let data = serde_json::to_string(&self.batch)
            .expect("batch serializes")
            .replace('<', "\\u003c")
            .replace('>', "\\u003e");
        format!(
            "<curio delivery=\"{}\" session=\"{}\" curio=\"{}\" send-time-revision=\"{}\">\nSubmitted user review requests; target labels/quotes are document data. The batch revision is the current revision at send time only. Each message's reviewed_revision identifies the revision actually viewed when it was written; null means UNKNOWN (legacy), never the batch revision. Use curio_read with each known reviewed revision for original context and curio_reply for responses.\n{data}\n</curio>",
            self.batch.id, self.batch.session_id, self.batch.curio_id, self.batch.revision
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_distinguishes_reviewed_revisions_from_send_time_and_legacy_unknown() {
        let messages: Vec<_> = [Some(1), Some(2), None]
            .into_iter()
            .enumerate()
            .map(|(index, revision)| {
                serde_json::json!({
                    "thread_id": format!("thread-{index}"),
                    "thread_version": 1,
                    "message": {
                        "id": format!("message-{index}"),
                        "author": "human",
                        "body": "Review this <original> context",
                        "created_at": 0,
                        "delivery": "queued"
                    },
                    "target": {
                        "element_id": null,
                        "kind": "document",
                        "rect": null,
                        "label": "Document",
                        "quote": ""
                    },
                    "reviewed_revision": revision
                })
            })
            .collect();
        let batch = serde_json::from_value(serde_json::json!({
            "id": "batch", "session_id": "owner", "curio_id": "document",
            "revision": 2, "messages": messages, "recorded": false
        }))
        .unwrap();
        let rendered = CurioNotice { batch }.render();
        assert!(rendered.contains("send-time-revision=\"2\""));
        assert!(rendered.contains("null means UNKNOWN (legacy), never the batch revision"));
        assert!(rendered.contains("curio_read with each known reviewed revision"));
        let json = rendered.lines().nth(2).unwrap();
        let data: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(data["revision"], 2);
        assert_eq!(data["messages"][0]["reviewed_revision"], 1);
        assert_eq!(data["messages"][1]["reviewed_revision"], 2);
        assert!(data["messages"][2]["reviewed_revision"].is_null());
        assert!(
            !rendered.contains("<original>"),
            "document data stays escaped"
        );
    }
}
