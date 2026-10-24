//! Native, owner-scoped Curios operations. Presentation never owns mutations.
use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec, nth_curios as core};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

pub struct CurioPublish;
pub struct CurioRead;
pub struct CurioReply;

const MAX_OUTPUT: usize = 32 * 1024;

pub(crate) fn failure(error: core::Error) -> String {
    let recovery = match error {
        core::Error::Conflict(_) => {
            "Read current versions; retry with fresh expected version and request ID. Reuse a request ID only for identical input."
        }
        core::Error::NotFound(_) => "List this session's curios and use an existing ID/revision.",
        core::Error::RepoRequired => {
            "Open a Git repository and prepare this session's Curios workspace."
        }
        _ => {
            "Check arguments and this session's source workspace; retry after correcting the cause."
        }
    };
    format!(
        "curios.{}: {} Recovery: {recovery}",
        error.code(),
        clip(&error.to_string(), 1024)
    )
}

pub(crate) fn store(ctx: &ToolContext, name: &str) -> Result<core::Store, String> {
    if ctx.is_subagent
        || ctx.agent.as_ref().is_some_and(|agent| {
            !agent.allows(name) || (name == "show_user" && !agent.allows("curio_read"))
        })
    {
        return Err("curios.permission: this agent cannot use this operation. Recovery: ask the owning primary session to perform it.".into());
    }
    let store = ctx.curios.store().map_err(failure)?;
    if ctx.session.is_empty() || store.session_id() != ctx.session {
        return Err("curios.permission: trusted scope does not match the active session. Recovery: prepare Curios for the active session.".into());
    }
    Ok(store)
}

fn args<T: DeserializeOwned>(value: Value) -> Result<T, String> {
    nth_protocol::parse_args(value)
        .map_err(|e| format!("curios.invalid: {e}. Recovery: follow the tool schema."))
}

fn clip(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn shorten(value: &mut Value) -> bool {
    match value {
        Value::String(s) if s.len() > 2048 => {
            *s = clip(s, 2048);
            true
        }
        Value::Array(items) => items.iter_mut().fold(false, |cut, v| shorten(v) | cut),
        Value::Object(fields) => fields.values_mut().fold(false, |cut, v| shorten(v) | cut),
        _ => false,
    }
}

// Every paged item has bounded strings and no unbounded nested collections.
fn page(
    items: Vec<Value>,
    cursor: Option<&str>,
    limit: Option<usize>,
    budget: usize,
) -> Result<Value, String> {
    let start = cursor.unwrap_or("0").parse::<usize>().map_err(|_| "curios.invalid: cursor must be a nonnegative decimal offset. Recovery: use next_cursor from the previous result.".to_string())?;
    let limit = limit.unwrap_or(20);
    if !(1..=100).contains(&limit) || start > items.len() {
        return Err("curios.invalid: limit must be 1..100 and cursor within the collection. Recovery: restart with no cursor.".into());
    }
    let total = items.len();
    let mut result = Vec::new();
    let mut bytes = 0;
    let mut truncated = false;
    for mut item in items.into_iter().skip(start).take(limit) {
        let cut = shorten(&mut item);
        let size = serde_json::to_vec(&item).expect("JSON value").len();
        if bytes + size > budget {
            break;
        }
        truncated |= cut;
        bytes += size;
        result.push(item);
    }
    let next = start + result.len();
    Ok(
        json!({"items": result, "total": total, "next_cursor": (next < total).then(|| next.to_string()), "text_truncated": truncated, "text_limit_bytes": 2048}),
    )
}

fn revision(rev: &core::Revision) -> Value {
    json!({"number":rev.number,"title":rev.title,"hash":rev.hash,"anchor_count":rev.anchors.len()})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishArgs {
    source_path: String,
    request_id: String,
    curio_id: Option<String>,
    expected_revision: Option<u64>,
    title: Option<String>,
}

impl Tool for CurioPublish {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: "curio_publish", description: "Publish one static HTML source file from this session's Curios source workspace. Does not open a browser. Updates require curio_id and expected_revision; request_id makes identical retries safe. Use curio_read for actual source/template paths.".into(), parameters: json!({"type":"object","additionalProperties":false,"properties":{"source_path":{"type":"string"},"request_id":{"type":"string"},"curio_id":{"type":"string"},"expected_revision":{"type":"integer","minimum":1},"title":{"type":"string"}},"required":["source_path","request_id"]}) }
    }
    fn call<'a>(&'a self, value: Value, ctx: &'a ToolContext) -> BoxFuture<'a, ToolResult> {
        async move {
            let a: PublishArgs = args(value)?;
            let store = store(ctx, "curio_publish")?;
            if a.curio_id.is_some() != a.expected_revision.is_some() {
                return Err("curios.invalid: updates require both curio_id and expected_revision. Recovery: read the current revision first.".into());
            }
            let source = crate::path::resolve(&ctx.cwd, &a.source_path);
            let published = store.publish_file(&source, core::Publish { request_id:a.request_id, curio_id:a.curio_id, expected_revision:a.expected_revision, title:a.title, html:String::new() }).map_err(failure)?;
            let detached_count = published.detached_thread_ids.len();
            Ok(json!({"curio":published.curio,"revision":revision(&published.revision),"source_path":source,"detached_thread_ids":published.detached_thread_ids.into_iter().take(100).collect::<Vec<_>>(),"detached_thread_count":detached_count,"thread_details":"Use curio_read threads for paginated attachment/status details."}).to_string())
        }.boxed()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    #[serde(default = "list")]
    action: String,
    curio_id: Option<String>,
    revision: Option<u64>,
    thread_id: Option<String>,
    batch_id: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}
fn list() -> String {
    "list".into()
}
fn required(value: Option<&str>, name: &str) -> Result<String, String> {
    value.map(str::to_owned).ok_or_else(|| {
        format!("curios.invalid: {name} required. Recovery: follow the selected action's schema.")
    })
}

impl Tool for CurioRead {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name:"curio_read", description:"Read only this session's curios. list returns source/template paths; get returns current metadata; revisions lists metadata; revision pages anchors; threads lists review/attachment state without messages; thread pages messages and original target context; batches lists immutable sent-batch metadata; batch pages submitted messages. Use curio_id for get/revisions/revision/threads/thread, thread_id for thread, batch_id for batch. Numeric opaque next_cursor paginates the selected collection. Text fields are clipped to 2048 bytes with text_truncated indication; output is bounded to 32 KiB. No HTML or authentication secrets returned.".into(), parameters:json!({"type":"object","additionalProperties":false,"properties":{"action":{"type":"string","enum":["list","get","revisions","revision","threads","thread","batches","batch"]},"curio_id":{"type":"string"},"revision":{"type":"integer","minimum":1},"thread_id":{"type":"string"},"batch_id":{"type":"string"},"cursor":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":100}}}) }
    }
    fn call<'a>(&'a self, value: Value, ctx: &'a ToolContext) -> BoxFuture<'a, ToolResult> {
        async move {
            let a: ReadArgs = args(value)?;
            let store = store(ctx, "curio_read")?;
            let mut meta = json!({"source_path":store.source_dir(),"template_path":store.prepare_template().map_err(failure)?});
            let items = match a.action.as_str() {
                "list" => store.list().map_err(failure)?.into_iter().map(|c| json!(c)).collect(),
                "batches" | "batch" => {
                    let batches = store.batches(false).map_err(failure)?;
                    if a.action == "batch" {
                        let id = required(a.batch_id.as_deref(), "batch_id")?;
                        let batch = batches.into_iter().find(|b| b.id == id).ok_or_else(|| failure(core::Error::NotFound("Batch not found".into())))?;
                        meta["batch"] = json!({"id":batch.id,"curio_id":batch.curio_id,"revision":batch.revision,"recorded":batch.recorded});
                        batch.messages.into_iter().map(|m| json!(m)).collect()
                    } else { batches.into_iter().filter(|b| a.curio_id.as_ref().is_none_or(|id| id == &b.curio_id)).map(|b| json!({"id":b.id,"curio_id":b.curio_id,"revision":b.revision,"recorded":b.recorded,"message_count":b.messages.len()})).collect() }
                }
                "get" | "revisions" | "revision" | "threads" | "thread" => {
                    let id = required(a.curio_id.as_deref(), "curio_id")?;
                    let snapshot = store.snapshot(&id, a.revision).map_err(failure)?;
                    meta["curio"] = json!(snapshot.curio);
                    meta["state_version"] = json!(snapshot.state_version);
                    meta["revision"] = revision(&snapshot.revision);
                    match a.action.as_str() {
                        "get" => vec![],
                        "revisions" => snapshot.revisions.iter().map(revision).collect(),
                        "revision" => snapshot.revision.anchors.into_iter().map(|a| json!(a)).collect(),
                        "threads" => snapshot.threads.into_iter().map(|t| json!({"id":t.id,"number":t.number,"version":t.version,"created_revision":t.created_revision,"target":t.target,"attachment":t.attachment,"status":t.status,"message_count":t.messages.len()})).collect(),
                        _ => {
                            let id = required(a.thread_id.as_deref(), "thread_id")?;
                            let t = snapshot.threads.into_iter().find(|t| t.id == id).ok_or_else(|| failure(core::Error::NotFound("Thread not found".into())))?;
                            meta["thread"] = json!({"id":t.id,"version":t.version,"created_revision":t.created_revision,"target":t.target,"attachment":t.attachment,"status":t.status});
                            t.messages.into_iter().map(|m| json!(m)).collect()
                        }
                    }
                }
                _ => return Err("curios.invalid: unknown action. Recovery: use an action from the tool schema.".into()),
            };
            let metadata_truncated = shorten(&mut meta);
            meta["metadata_truncated"] = json!(metadata_truncated);
            let budget = MAX_OUTPUT.saturating_sub(meta.to_string().len() + 512);
            meta["page"] = page(items, a.cursor.as_deref(), a.limit, budget)?;
            Ok(meta.to_string())
        }.boxed()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyArgs {
    curio_id: String,
    thread_id: String,
    request_id: String,
    expected_version: u64,
    body: String,
    revision: Option<u64>,
    #[serde(default)]
    mark_addressed: bool,
}
impl Tool for CurioReply {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name:"curio_reply", description:"Append a trusted agent reply to this session's thread. Requires expected_version and idempotent request_id. Optionally reference a published revision or mark_addressed. Never resolves/reopens threads or submits user feedback.".into(), parameters:json!({"type":"object","additionalProperties":false,"properties":{"curio_id":{"type":"string"},"thread_id":{"type":"string"},"request_id":{"type":"string"},"expected_version":{"type":"integer","minimum":1},"body":{"type":"string","minLength":1,"maxLength":16384},"revision":{"type":"integer","minimum":1},"mark_addressed":{"type":"boolean"}},"required":["curio_id","thread_id","request_id","expected_version","body"]}) }
    }
    fn call<'a>(&'a self, value: Value, ctx: &'a ToolContext) -> BoxFuture<'a, ToolResult> {
        async move {
            let a: ReplyArgs = args(value)?;
            let store = store(ctx, "curio_reply")?;
            let t = store.reply(&a.curio_id, &a.thread_id, core::Reply { request_id:a.request_id, expected_version:a.expected_version, body:a.body, mark_addressed:a.mark_addressed, revision:a.revision }, core::Author::Agent).map_err(failure)?;
            Ok(json!({"curio_id":a.curio_id,"thread_id":t.id,"version":t.version,"status":t.status,"message_id":t.messages.last().map(|m| &m.id)}).to_string())
        }.boxed()
    }
}

#[cfg(test)]
mod tests;
