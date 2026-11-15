//! The headless `nth run` end to end: the real binary, a real config and a
//! real session save, against a mock provider on 127.0.0.1. `nth-llm`'s
//! test server is `pub(crate)`, so this is a small standalone one. It serves
//! the OpenAI-style `/v1/models` listing the run asks for when the config
//! names no models, and chat completions SSE in the shape `nth-llm` parses.
//!
//! Everything the run writes goes under the runner's temp dir — HOME,
//! XDG_DATA_HOME (where the session lands) and the working directory — so
//! the test pins the config-to-session wiring without touching any real
//! file, and the models.dev fetch the binary makes on its own can fail
//! offline without the run noticing.

mod support;

use std::{
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Output,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use serde_json::Value;
use support::Env;

/// The answer a plain run gets; distinctive enough to assert on.
const ANSWER: &str = "Hello from the mock provider.";

/// What the overflow script's replay gets, so the two answers cannot be
/// told apart by accident.
const FINAL: &str = "The final answer.";

/// A multi-line summary, so the test also pins that only its first line
/// reaches stderr; the rest must stay inside the loop's own request.
const SUMMARY: &str = "The conversation so far\n\n## Objective\ncarry on";

/// Refused with this body, as an endpoint that cannot fit the request does;
/// the phrasing is the one `nth-llm::http::context_overflow` looks for.
const OVERFLOW: &str = r#"{"error":{"message":"This model's maximum context length is 8192 tokens. However, you requested 8896 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;

/// How the mock answers chat completions requests.
#[derive(Clone, Copy)]
enum Script {
    /// Every request gets [`ANSWER`].
    Answer,
    /// The first request is refused as too long, the second is the turn's
    /// summary request and everything after it the replay: the overflow
    /// recovery end to end, with the model's window unknown.
    Overflow,
}

/// The mock provider: one accept loop, one thread per connection, which
/// answers one request and closes. What it was asked is kept for the
/// test's assertions.
struct Mock {
    port: u16,
    state: Arc<State>,
}

/// What the mock's connections share.
struct State {
    script: Script,
    /// Chat completions requests so far, which pick the scripted reply.
    chats: AtomicUsize,
    listings: AtomicUsize,
    /// Every chat completions request body, in order.
    requests: Mutex<Vec<String>>,
}

impl Mock {
    fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let port = listener.local_addr().expect("bound").port();
        let state = Arc::new(State {
            script,
            chats: AtomicUsize::new(0),
            listings: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let accepting = state.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let state = accepting.clone();
                thread::spawn(move || serve(stream, &state));
            }
        });
        Self { port, state }
    }

    fn port(&self) -> u16 {
        self.port
    }

    /// Every chat completions request body, in order.
    fn requests(&self) -> Vec<String> {
        self.state.requests.lock().expect("not poisoned").clone()
    }

    /// How often the endpoint's `/v1/models` was asked.
    fn listings(&self) -> usize {
        self.state.listings.load(Ordering::SeqCst)
    }
}

/// Serves one request: the head tells the route, the body decides the reply.
fn serve(mut stream: TcpStream, state: &State) {
    let Some((head, body)) = read_request(&mut stream) else {
        return;
    };
    let request_line = head.lines().next().unwrap_or_default().to_string();
    let (status, content_type, reply) = match request_line.as_str() {
        "GET /v1/models HTTP/1.1" => {
            state.listings.fetch_add(1, Ordering::SeqCst);
            (
                200,
                "application/json",
                r#"{"object":"list","data":[{"id":"mock-model","object":"model"}]}"#.to_string(),
            )
        }
        "POST /v1/chat/completions HTTP/1.1" => chat_reply(&body, state),
        other => (404, "text/plain", format!("unexpected request {other}")),
    };
    write_response(&mut stream, status, content_type, &reply);
}

/// The chat completions reply the script gives the `n`-th request.
fn chat_reply(body: &str, state: &State) -> (u16, &'static str, String) {
    let n = state.chats.fetch_add(1, Ordering::SeqCst) + 1;
    state
        .requests
        .lock()
        .expect("not poisoned")
        .push(body.to_string());
    match (state.script, n) {
        (Script::Answer, _) => (200, "text/event-stream", completion(ANSWER, 12, 4)),
        (Script::Overflow, 1) => (400, "application/json", OVERFLOW.to_string()),
        (Script::Overflow, 2) => (200, "text/event-stream", completion(SUMMARY, 100, 20)),
        (Script::Overflow, _) => (200, "text/event-stream", completion(FINAL, 12, 4)),
    }
}

/// One chat completions stream: the text, a finish, its usage, then DONE.
fn completion(text: &str, input: u64, output: u64) -> String {
    let text = serde_json::to_string(text).expect("a JSON string");
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{text}}}}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":{input},\"completion_tokens\":{output},\"total_tokens\":{}}}}}\n\n\
         data: [DONE]\n\n",
        input + output
    )
}

/// The whole request, so the reply never lands on a connection reset by
/// unread bytes; `None` when it ended before head and body had arrived.
fn read_request(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut request = Vec::new();
    let mut buf = [0; 4096];
    let head_end = loop {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        request.extend_from_slice(&buf[..n]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
    };
    let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|value| value.trim().to_string())
        })
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    while request.len() < head_end + 4 + length {
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        request.extend_from_slice(&buf[..n]);
    }
    Some((
        head,
        String::from_utf8_lossy(&request[head_end + 4..head_end + 4 + length]).into_owned(),
    ))
}

fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
}

/// The runner's environment with the mock's key set, the working
/// directory the child starts in and the config file it is pointed at with
/// `--config`.
struct Dirs {
    env: Env,
    work: PathBuf,
    file: PathBuf,
}

impl Dirs {
    fn new(config: &str) -> Self {
        let mut env = Env::new();
        env.set("MOCK_KEY", "mock-key");
        let work = env.dir("work");
        let file = env.root().join("nth.toml");
        std::fs::write(&file, config).expect("writes the config");
        Self { env, work, file }
    }

    /// The one session the run saved, parsed.
    fn session(&self) -> Value {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.env.sessions())
            .expect("the run saved a session")
            .map(|entry| entry.expect("readable").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        files.sort();
        assert_eq!(files.len(), 1, "exactly one session: {files:?}");
        let json = std::fs::read_to_string(&files[0]).expect("reads the session");
        serde_json::from_str(&json).expect("the session is valid JSON")
    }

    /// `nth --config <file> args` in the working directory.
    fn nth(&self, args: &[&str]) -> Output {
        let file: &Path = &self.file;
        let mut all = vec![std::ffi::OsStr::new("--config"), file.as_os_str()];
        all.extend(args.iter().map(std::ffi::OsStr::new));
        self.env.run(&self.work, &all)
    }
}

/// A config that talks to the mock: `[provider.mock]` with the endpoint,
/// the key's variable and the models to offer (empty asks the endpoint's
/// listing), the mock model as the one to run, and the compaction policy
/// under test.
fn config_for(port: u16, models: &[&str], auto: bool) -> String {
    let models = models
        .iter()
        .map(|id| format!("\"{id}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"model = "mock/mock-model"

[provider.mock]
base_url = "http://127.0.0.1:{port}/v1"
api_key_env = "MOCK_KEY"
models = [{models}]

[context]
auto = {auto}
"#
    )
}

/// What one child left behind, as text.
struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// Runs the real binary on `prompt` with `dirs`' config and environment.
fn run_nth(dirs: &Dirs, prompt: &str) -> Run {
    let output = dirs.nth(&["run", prompt]);
    let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).into_owned();
    Run {
        status: output.status,
        stdout: text(output.stdout),
        stderr: text(output.stderr),
    }
}

/// A headless run answers on stdout, prints its summary line on stderr and
/// saves a session that carries the config: the model it ran, the
/// `[context]` policy, the prompt and the answer.
#[test]
fn a_run_answers_and_saves_the_session_the_config_asked_for() {
    let mock = Mock::start(Script::Answer);
    let dirs = Dirs::new(&config_for(mock.port(), &["mock-model"], false));

    let run = run_nth(&dirs, "say hello");

    assert!(
        run.status.success(),
        "nth run failed: {:?}\nstderr: {}",
        run.status,
        run.stderr
    );
    assert_eq!(
        run.stdout,
        format!("{ANSWER}\n"),
        "stdout is the answer and nothing else"
    );
    assert!(
        run.stderr
            .contains("mock/mock-model · 0 tool calls · 1 step"),
        "the summary line names the model and what it spent: {:?}",
        run.stderr
    );

    let session = dirs.session();
    assert_eq!(session["model"], "mock/mock-model");
    assert!(
        session.get("compaction").is_none(),
        "[context] is the config's, applied on every run, never saved: {session}"
    );
    let messages = session["messages"].as_array().expect("an array");
    assert!(
        messages.iter().any(|m| m["user"] == "say hello"),
        "the prompt is saved: {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m["assistant"]["text"] == ANSWER),
        "the answer is saved: {messages:?}"
    );
    assert_eq!(
        session["last_usage"]["input"], 12,
        "the usage the mock reported is counted and saved"
    );

    // The configured models went on the wire as the endpoint's, and the
    // endpoint's own listing was never needed for them.
    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "one step, one request");
    assert!(
        requests[0].contains(r#""model":"mock-model""#),
        "the endpoint hears its own model id: {}",
        requests[0]
    );
    assert_eq!(mock.listings(), 0, "models = [...] skips /v1/models");
}

/// The commands that read a saved session find the one a run saved, in
/// the environment it saved it in: the `run` glue of `session list`,
/// `usage` and `context`, which the listings' smoke tests cannot reach
/// without a session. The mock still serves, so `usage` may ask it for
/// prices.
#[test]
fn the_saved_session_commands_name_the_run_s_session() {
    let mock = Mock::start(Script::Answer);
    let dirs = Dirs::new(&config_for(mock.port(), &["mock-model"], false));
    let run = run_nth(&dirs, "say hello");
    assert!(run.status.success(), "nth run failed: {}", run.stderr);
    let id = dirs.session()["id"].clone();
    assert!(id.is_string(), "the session has an id: {id}");

    let json = |args: &[&str]| -> Value {
        let output = dirs.nth(args);
        assert!(
            output.status.success(),
            "nth {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("one JSON document")
    };
    let sessions = json(&["session", "list", "--json"]);
    let sessions = sessions.as_array().expect("an array");
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    assert_eq!(sessions[0]["id"], id);
    assert_eq!(json(&["usage", "--json"])["id"], id);
    assert_eq!(json(&["context", "--json"])["session_id"], id);
}

/// An overflowed step is compacted and replayed: the summary request is the
/// next thing the endpoint sees, its answer reaches stderr as the
/// `compacted` row, and the replay ends the turn. The mock model is unknown
/// to the catalogue, so its window is unknown and only the refusal can
/// trigger this.
#[test]
fn an_overflowed_run_compacts_replays_and_prints_the_compaction() {
    let mock = Mock::start(Script::Overflow);
    // No models listed: nth asks the endpoint's `/v1/models`, which the
    // mock serves.
    let dirs = Dirs::new(&config_for(mock.port(), &[], true));

    let run = run_nth(&dirs, "the overflowing prompt");

    assert!(
        run.status.success(),
        "nth run failed: {:?}\nstderr: {}",
        run.status,
        run.stderr
    );
    assert_eq!(
        run.stdout,
        format!("{FINAL}\n"),
        "only the replayed answer reaches stdout; the summary does not"
    );
    assert!(
        run.stderr.contains("compacted · The conversation so far"),
        "the compaction shows as one row with the summary's first line: {:?}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("## Objective"),
        "later summary lines do not print: {:?}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("mock/mock-model · 0 tool calls · 2 steps"),
        "the summary and the replay are counted: {:?}",
        run.stderr
    );
    assert!(
        mock.listings() >= 1,
        "an empty models list asks the endpoint's /v1/models"
    );

    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        3,
        "the refused step, the summary, the replay"
    );
    assert!(
        requests[0].contains(r#""content":"the overflowing prompt""#)
            && requests[0].contains(r#""tools""#),
        "the first request is the turn's own, tools and all: {}",
        requests[0]
    );
    assert!(
        requests[1].contains("Summarize the conversation")
            && requests[1].contains("[User]: the overflowing prompt"),
        "the summary request carries the template and the serialized prompt: {}",
        requests[1]
    );
    assert!(
        !requests[1].contains(r#""tools""#),
        "the summary request offers no tools: {}",
        requests[1]
    );
    assert!(
        requests[2].contains("<conversation-checkpoint>")
            && requests[2].contains("The conversation so far"),
        "the step again starts at the checkpoint: {}",
        requests[2]
    );

    let session = dirs.session();
    let messages = session["messages"].as_array().expect("an array");
    assert!(
        messages
            .iter()
            .any(|m| m["compaction"]["summary"] == SUMMARY),
        "the marker with the summary is saved: {messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m["user"] == "the overflowing prompt"),
        "the original prompt stays in the history: {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m["assistant"]["text"] == FINAL),
        "the replayed answer is saved: {messages:?}"
    );
}
