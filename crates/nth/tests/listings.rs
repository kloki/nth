//! Smoke tests of the listing subcommands that read only the filesystem:
//! the real binary, from clap through the gather to the JSON it prints, in
//! an isolated environment with a small project. stdout is a pipe here, so
//! every command prints JSON; what a person reads is tested beside each
//! printer.

mod support;

use std::{collections::BTreeMap, path::PathBuf, time::SystemTime};

use nth_protocol::{Message, Usage};
use nth_session::{Session, Spend, ToolUse};
use serde_json::{Value, json};
use support::Env;

#[tokio::test]
async fn session_rename_cli_updates_the_saved_title_without_overwriting_history() {
    let env = Env::new();
    let work = env.dir("work");
    let mut session = Session::new("model", work.clone());
    session.messages.push(Message::User("original".into()));
    env.save(&session).await;
    let id = session.id.to_string();
    let output = env.run(&work, &["session", "rename", &id[..8], "New title"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["id"], id);
    assert_eq!(result["title"], "New title");
    assert_eq!(result["outcome"], "renamed");
    let store = nth_session::Store::at(env.sessions());
    let saved = store.load(session.id).await.unwrap();
    assert_eq!(saved.messages, session.messages);
    assert_eq!(saved.updated_at, session.updated_at);
    assert_eq!(store.list().await.unwrap()[0].title, "New title");
}

/// A project with one of each thing the listings read: a skill, an agent
/// and a memory entry with its index.
fn project(env: &Env) -> PathBuf {
    let work = env.dir("work");
    let write = |path: &str, text: &str| {
        let path = work.join(path);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("creates the folder");
        std::fs::write(path, text).expect("writes the fixture");
    };
    write(
        ".claude/skills/x/SKILL.md",
        "---\nname: x\ndescription: The x skill\n---\nDo x.\n",
    );
    write(
        ".claude/agents/y.md",
        "---\nname: y\ndescription: The y agent\n---\nYou are y.\n",
    );
    write(
        ".nth/memory/colours.md",
        "---\nname: colours\ndescription: Colour names are terminal colours\ntype: feedback\n---\nUse the 16.\n",
    );
    write(
        ".nth/memory/MEMORY.md",
        "- [Colours](colours.md) — terminal colours\n",
    );
    work
}

/// The line whose `key` is `value`, failing the test when there is none.
fn find<'a>(lines: &'a [Value], key: &str, value: &str) -> &'a Value {
    lines
        .iter()
        .find(|line| line[key] == value)
        .unwrap_or_else(|| panic!("no line with {key} {value}: {lines:?}"))
}

/// `value` with the temp root replaced, for a snapshot.
fn redacted(env: &Env, value: &Value) -> Value {
    serde_json::from_str(&env.redact(&value.to_string())).expect("still JSON")
}

#[test]
fn skills_lists_the_project_skill() {
    let env = Env::new();
    let work = project(&env);
    let lines = env.json_lines(&work, &["skills"]);
    let x = find(&lines, "name", "x");
    assert_eq!(x["source"], "project");
    insta::assert_json_snapshot!(redacted(&env, x));
}

#[test]
fn agents_lists_the_project_agent_beside_the_builtins() {
    let env = Env::new();
    let work = project(&env);
    let lines = env.json_lines(&work, &["agents"]);
    for builtin in ["general", "explore"] {
        find(&lines, "name", builtin);
    }
    let y = find(&lines, "name", "y");
    assert_eq!(y["source"], "project");
    insta::assert_json_snapshot!(redacted(&env, y));
}

#[test]
fn memory_lists_the_saved_entry() {
    let env = Env::new();
    let work = project(&env);
    let lines = env.json_lines(&work, &["memory"]);
    assert_eq!(lines.len(), 1, "the index is not an entry: {lines:?}");
    let entry = &lines[0];
    assert_eq!(entry["name"], "colours");
    assert_eq!(entry["type"], "feedback");
    assert_eq!(entry["description"], "Colour names are terminal colours");
    insta::assert_json_snapshot!(redacted(&env, entry));
}

/// What is on PATH decides which formatters are enabled, so only the keys
/// every line carries are pinned.
#[test]
fn formatters_lines_carry_their_keys() {
    let env = Env::new();
    let work = project(&env);
    let lines = env.json_lines(&work, &["formatters"]);
    assert!(!lines.is_empty(), "the built-in table is listed");
    for line in &lines {
        assert!(line["name"].is_string(), "{line}");
        assert!(line["extensions"].is_array(), "{line}");
        assert!(line["enabled"].is_boolean(), "{line}");
    }
}

/// As for formatters, PATH decides `program` and the project decides
/// `root`, either of which may be null.
#[test]
fn lsp_lines_carry_their_keys() {
    let env = Env::new();
    let work = project(&env);
    let lines = env.json_lines(&work, &["lsp"]);
    assert!(!lines.is_empty(), "the built-in table is listed");
    for line in &lines {
        assert!(line["id"].is_string(), "{line}");
        assert!(line["extensions"].is_array(), "{line}");
        for key in ["program", "root"] {
            assert!(line.get(key).is_some(), "no {key}: {line}");
        }
    }
}

#[test]
fn config_prints_a_config_that_parses_as_toml() {
    let env = Env::new();
    let work = project(&env);
    let output = env.success(&work, &["config"]);
    let config: toml::Table = std::str::from_utf8(&output.stdout)
        .expect("UTF-8")
        .parse()
        .expect("the config is TOML");
    assert!(
        config.contains_key("session"),
        "defaults filled in: {config}"
    );
}

#[test]
fn session_list_all_is_empty_on_an_empty_data_dir() {
    let env = Env::new();
    let work = project(&env);
    let sessions = env.json(&work, &["session", "list", "--all"]);
    assert_eq!(sessions, json!([]));
}

/// The gather reads the env's data dir: one saved session's spend comes
/// back as its model's line for the day and its tool's line. The spend
/// stores no rates and no provider is configured to list them, so its
/// price is null.
#[tokio::test]
async fn usage_all_reads_the_saved_spend() {
    let env = Env::new();
    let work = project(&env);
    let mut session = Session::new("seed/model", work.clone());
    session.messages.push(Message::User("spend".into()));
    session.usage.record(Spend {
        model: "seed/model".into(),
        source: None,
        agent: None,
        steps: 2,
        tokens: Usage {
            input: 100,
            output: 20,
            ..Usage::default()
        },
        at: SystemTime::now(),
        cost: None,
        tools: BTreeMap::from([(
            "read".to_string(),
            ToolUse {
                calls: 3,
                ..ToolUse::default()
            },
        )]),
    });
    env.save(&session).await;

    let lines = env.json_lines(&work, &["usage", "--all"]);
    let model = find(&lines, "model", "seed/model");
    assert_eq!(model["steps"], 2);
    assert_eq!(model["input"], 100);
    assert_eq!(model["output"], 20);
    assert!(model["day"].is_string(), "{model}");
    assert!(
        model["price"].is_null(),
        "no stored rates and no listing, so no price: {model}"
    );
    assert_eq!(find(&lines, "tool", "read")["calls"], 3);
}

#[test]
fn notify_refuses_when_notifications_are_off() {
    let env = Env::new();
    let work = project(&env);
    let file = env.config().join("nth/config.toml");
    std::fs::create_dir_all(file.parent().expect("a parent")).expect("creates the folder");
    std::fs::write(&file, "[notify]\nenabled = false\n").expect("writes the config");
    let output = env.run(&work, &["notify"]);
    assert!(!output.status.success(), "nothing to send through");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("notifications are off"), "{stderr}");
}

/// A pipe is never a terminal, so `--json` changes nothing there.
#[test]
fn json_flag_changes_nothing_on_a_pipe() {
    let env = Env::new();
    let work = project(&env);
    let plain = env.success(&work, &["skills"]);
    let json = env.success(&work, &["skills", "--json"]);
    assert_eq!(
        String::from_utf8_lossy(&plain.stdout),
        String::from_utf8_lossy(&json.stdout)
    );
}
