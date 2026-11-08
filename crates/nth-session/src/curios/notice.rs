//! Review feedback as the model reads it: one submitted batch, rendered
//! with the document text escaped so it can never close the element.

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
