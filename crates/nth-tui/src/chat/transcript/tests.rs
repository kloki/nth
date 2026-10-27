use nth_protocol::Message;
use nth_session::SHELL_PROMPT;

use super::*;

pub(in crate::chat) fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "read".into(),
        arguments: r#"{"filePath":"/repo/src/a.rs"}"#.into(),
    }
}

pub(in crate::chat) fn transcript() -> Transcript {
    Transcript::new("/repo".into())
}

pub(in crate::chat) fn text(lines: &[Line]) -> Vec<String> {
    lines.iter().map(|l| l.to_string()).collect()
}

#[test]
fn a_word_is_only_under_its_own_columns() {
    assert_eq!(word_at("a  bb c", 0), Some((0, "a")));
    assert_eq!(word_at("a  bb c", 1), None);
    assert_eq!(word_at("a  bb c", 2), None);
    assert_eq!(word_at("a  bb c", 3), Some((3, "bb")));
    assert_eq!(word_at("a  bb c", 6), Some((6, "c")));
    assert_eq!(word_at("a  bb c", 7), None);
    assert_eq!(word_at("日本 x", 1), Some((0, "日本")));
    assert_eq!(word_at("日本 x", 5), Some((5, "x")));
}

#[test]
fn entries_are_found_by_line_with_their_blank_separators() {
    let mut t = transcript();
    t.push_user("one".into());
    t.apply(&Event::TextDelta("two".into()));
    t.push_error("three".into());
    t.layout(40, ChatSettings::default());

    assert_eq!(t.entry_at(0), Some(&Entry::User("one".into())));
    // The blank line above an entry counts as its own.
    assert_eq!(t.entry_at(1), Some(&Entry::Answer("two".into())));
    assert_eq!(t.entry_at(2), Some(&Entry::Answer("two".into())));
    assert_eq!(t.entry_at(4), Some(&Entry::TurnError("three".into())));
    assert_eq!(t.entry_at(5), None);

    assert_eq!(t.entry_lines(0), Some(0..1));
    assert_eq!(t.entry_lines(2), Some(1..3));
    assert_eq!(t.entry_lines(5), None);
}

fn url_at(t: &Transcript, line: usize, column: usize) -> Option<String> {
    t.link_at(line, column).map(|(_, url)| url)
}

#[test]
fn links_are_found_by_their_drawn_columns() {
    let mut t = transcript();
    t.apply(&Event::TextDelta(
        "see [docs](https://x.y) or https://a.b/c. and [no](file:///etc/passwd)".into(),
    ));
    t.layout(60, ChatSettings::default());
    assert_eq!(
        text(&t.visible(0, 1)),
        ["▎ see docs or https://a.b/c. and no"]
    );

    // `docs` sits after the bar and `see `.
    assert_eq!(url_at(&t, 0, 6).as_deref(), Some("https://x.y"));
    assert_eq!(url_at(&t, 0, 9).as_deref(), Some("https://x.y"));
    assert_eq!(url_at(&t, 0, 10), None);
    assert_eq!(t.link_at(0, 6).map(|(columns, _)| columns), Some(6..10));
    // A bare address, without the full stop after it.
    assert_eq!(url_at(&t, 0, 14).as_deref(), Some("https://a.b/c"));
    assert_eq!(t.link_at(0, 14).map(|(columns, _)| columns), Some(14..27));
    // The gap before it opens nothing, nor does a file link.
    assert_eq!(url_at(&t, 0, 13), None);
    assert_eq!(url_at(&t, 0, 35), None);
    assert_eq!(url_at(&t, 1, 6), None);
}

#[test]
fn a_link_on_a_wrapped_row_is_found_there() {
    let mut t = transcript();
    t.apply(&Event::TextDelta(
        "alpha beta gamma [delta](https://d) epsilon".into(),
    ));
    t.layout(20, ChatSettings::default());
    assert_eq!(
        text(&t.visible(0, 3)),
        ["▎ alpha beta gamma", "▎ delta epsilon"]
    );

    assert_eq!(url_at(&t, 1, 2).as_deref(), Some("https://d"));
    assert_eq!(url_at(&t, 1, 6).as_deref(), Some("https://d"));
    assert_eq!(url_at(&t, 1, 7), None);
    assert_eq!(url_at(&t, 0, 2), None);
}

#[test]
fn monitor_notices_show_as_their_own_rows() {
    let text = "<monitor id=\"2\" description=\"ci\" log=\"/l/2.log\">\nfailed\n</monitor>\n\
                    <monitor id=\"2\" description=\"ci\" log=\"/l/2.log\" ended=\"exited with code 1\" events=\"1\"/>\n\nfix it";
    let t = Transcript::replay("/repo".into(), &[Message::User(text.into())]);

    let entries: Vec<_> = t.entries().collect();
    let notice = |lines, ended: Option<&str>| {
        Entry::Notice(NoticeSummary::Monitor {
            id: 2,
            description: "ci".into(),
            lines,
            ended: ended.map(Into::into),
        })
    };
    assert_eq!(
        entries,
        [
            &notice(1, None),
            &notice(0, Some("exited with code 1")),
            &Entry::User("fix it".into()),
        ]
    );
}

#[test]
fn streams_reasoning_tools_and_answer_into_entries() {
    let mut t = transcript();
    t.push_user("go".into());
    t.apply(&Event::ReasoningDelta("hm".into()));
    t.apply(&Event::ReasoningDelta("mm".into()));
    t.apply(&Event::ToolStarted(call("1")));
    t.apply(&Event::ToolFinished {
        call: call("1"),
        result: Err("no such file\nat line 2".into()),
    });
    t.apply(&Event::TextDelta("\n".into()));
    t.apply(&Event::TextDelta("do".into()));
    t.apply(&Event::TextDelta("ne".into()));
    t.finish_turn(Ok(()), "glm", Duration::from_secs(2));

    let entries: Vec<_> = t.entries().collect();
    assert_eq!(entries.len(), 5);
    assert!(matches!(
        entries[1],
        Entry::Reasoning { took: Some(_), text, .. } if text == "hmmm"
    ));
    assert_eq!(
        entries[2],
        &Entry::Tool {
            call: call("1"),
            state: ToolState::Failed("no such file".into()),
            output: Vec::new(),
            notes: Vec::new(),
        }
    );
    assert_eq!(entries[3], &Entry::Answer("done".into()));
    assert!(matches!(entries[4], Entry::TurnDone { tool_calls: 1, .. }));
}

#[test]
fn a_retry_row_sits_between_the_partial_and_the_retried_answer() {
    let mut t = transcript();
    t.push_user("go".into());
    t.apply(&Event::TextDelta("partial".into()));
    t.apply(&Event::Retry {
        attempt: 1,
        delay: Duration::from_secs(2),
    });
    t.apply(&Event::TextDelta("full answer".into()));

    let entries: Vec<_> = t.entries().collect();
    assert_eq!(
        entries,
        [
            &Entry::User("go".into()),
            &Entry::Answer("partial".into()),
            &Entry::Retry {
                attempt: 1,
                delay: Duration::from_secs(2),
            },
            &Entry::Answer("full answer".into()),
        ],
        "the retry's text starts a fresh answer block"
    );
}

#[test]
fn a_turn_counts_only_its_own_tool_calls() {
    let mut t = transcript();
    t.push_user("go".into());
    t.apply(&Event::ToolStarted(call("1")));
    t.apply(&Event::ToolStarted(call("2")));
    t.finish_turn(Ok(()), "glm", Duration::from_secs(1));
    // A turn that monitors' notices started has no user row.
    t.push_user(
        "<monitor id=\"2\" description=\"ci\" log=\"/l/2.log\">\nfailed\n</monitor>".into(),
    );
    t.apply(&Event::ToolStarted(call("3")));
    t.finish_turn(Ok(()), "glm", Duration::from_secs(1));
    // One that answered without tools, after a stopped one.
    t.interrupt(Duration::from_secs(1));
    t.finish_turn(Ok(()), "glm", Duration::from_secs(1));

    let counts: Vec<_> = t
        .entries()
        .filter_map(|entry| match entry {
            Entry::TurnDone { tool_calls, .. } => Some(*tool_calls),
            _ => None,
        })
        .collect();
    assert_eq!(counts, [2, 1, 0]);
}

#[test]
fn interrupting_stops_running_tools_and_adds_the_footer() {
    let mut t = transcript();
    t.push_user("go".into());
    t.apply(&Event::ReasoningDelta("hm".into()));
    t.apply(&Event::ToolStarted(call("1")));
    t.apply(&Event::ToolStarted(call("2")));
    t.apply(&Event::ToolFinished {
        call: call("1"),
        result: Ok("x".into()),
    });
    t.interrupt(Duration::from_secs(3));

    let entries: Vec<_> = t.entries().collect();
    assert!(matches!(entries[1], Entry::Reasoning { took: Some(_), .. }));
    assert!(matches!(
        entries[2],
        Entry::Tool {
            state: ToolState::Done,
            ..
        }
    ));
    assert_eq!(
        entries[3],
        &Entry::Tool {
            call: call("2"),
            state: ToolState::Failed("interrupted".into()),
            output: Vec::new(),
            notes: Vec::new(),
        }
    );
    assert!(matches!(entries[4], Entry::Interrupted { .. }));
    let total = t.layout(40, ChatSettings::default());
    assert_eq!(text(&t.visible(total - 1, 1)), ["  ⏹ interrupted · 3.0s"]);
}

fn tool(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

fn output(id: &str, text: &str) -> Event {
    Event::ToolOutput {
        call_id: id.into(),
        text: text.into(),
    }
}

fn numbered(range: std::ops::RangeInclusive<usize>) -> Vec<String> {
    range.map(|i| i.to_string()).collect()
}

fn outputs(t: &Transcript) -> Vec<Vec<String>> {
    t.entries()
        .filter_map(|entry| match entry {
            Entry::Tool { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn output_stays_with_its_call_after_it_finishes() {
    let mut t = transcript();
    let bash = tool("1", "bash", r#"{"command":"cargo test"}"#);
    t.apply(&Event::ToolStarted(bash.clone()));
    t.apply(&output("1", "compiling\n"));
    t.apply(&output("2", "not this call\n"));
    t.apply(&Event::ToolFinished {
        call: bash,
        result: Ok(String::new()),
    });

    assert_eq!(outputs(&t), [["compiling"]]);
}

#[test]
fn bash_keeps_the_last_lines_and_read_the_first() {
    let mut t = transcript();
    let text = |range| numbered(range).join("\n") + "\n";
    t.apply(&Event::ToolStarted(tool("1", "bash", "{}")));
    t.apply(&Event::ToolStarted(tool("2", "read", "{}")));
    t.apply(&output("1", &text(1..=8)));
    t.apply(&output("1", &text(9..=14)));
    t.apply(&output("2", &text(1..=14)));

    assert_eq!(outputs(&t), [numbered(5..=14), numbered(1..=10)]);
}

#[test]
fn write_shows_the_start_of_its_content() {
    let mut t = transcript();
    let content = numbered(1..=12).join("\\n");
    let arguments = format!(r#"{{"filePath":"a.rs","content":"{content}"}}"#);
    t.apply(&Event::ToolStarted(tool("1", "write", &arguments)));

    assert_eq!(outputs(&t), [numbered(1..=10)]);
}

#[test]
fn plan_edits_show_as_one_row() {
    let text = edits::render("/repo/plan.md".as_ref(), "a\n", "a\n<!-- b? -->\n");
    let mut t = Transcript::new("/repo".into());
    t.push_user(text);

    assert_eq!(
        t.entries().collect::<Vec<_>>(),
        [&Entry::PlanEdits(PlanEdits {
            added: 1,
            removed: 0
        })]
    );
}

#[test]
fn mode_reminders_are_left_out() {
    let mut t = Transcript::new("/repo".into());
    t.push_user("plan it\n\n<system-reminder>\nPlan mode is active.\n</system-reminder>".into());
    t.push_user("go\n\n<system-reminder>\nYour operational mode has changed\n</system-reminder>\n\nA plan file exists".into());

    let users: Vec<_> = t.entries().collect();
    assert_eq!(
        users,
        [&Entry::User("plan it".into()), &Entry::User("go".into())]
    );
}

#[test]
fn replays_a_command_you_ran_as_its_bash_row() {
    let bash = tool("1", "bash", r#"{"command":"ls"}"#);
    let messages = [
        Message::User(SHELL_PROMPT.into()),
        Message::Assistant(nth_protocol::AssistantMessage {
            tool_calls: vec![bash],
            ..Default::default()
        }),
        Message::ToolResult {
            call_id: "1".into(),
            content: "a.rs".into(),
            images: Vec::new(),
        },
    ];

    let t = Transcript::replay(PathBuf::from("/repo"), &messages);

    let entries: Vec<_> = t.entries().collect();
    assert!(
        matches!(&entries[..], [Entry::Tool { call, state: ToolState::Done, .. }] if call.name == "bash"),
        "{entries:?}"
    );
}

#[test]
fn replay_shows_a_compaction_as_a_divider() {
    let messages = [
        Message::User("go".into()),
        Message::Assistant(nth_protocol::AssistantMessage {
            text: "done".into(),
            ..Default::default()
        }),
        Message::Compaction {
            summary: "Objective: ship".into(),
            recent: "[Assistant]: done".into(),
        },
        Message::User("next".into()),
    ];

    let t = Transcript::replay("/repo".into(), &messages);

    assert_eq!(
        t.entries().collect::<Vec<_>>(),
        [
            &Entry::User("go".into()),
            &Entry::Answer("done".into()),
            &Entry::Compacted("Objective: ship".into()),
            &Entry::User("next".into()),
        ],
        "the divider follows what it covers"
    );
}

#[tokio::test]
async fn replaying_a_saved_compacted_session_shows_the_divider_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = nth_session::Store::at(dir.path());
    let mut session = nth_session::Session::new("glm", "/repo".into());
    session.messages.push(Message::User("go".into()));
    session
        .messages
        .push(Message::Assistant(nth_protocol::AssistantMessage {
            text: "done".into(),
            ..Default::default()
        }));
    session.messages.push(Message::Compaction {
        // The model may open with blanks and answer in markdown.
        summary: "\n\n## Findings\n\n- one\n- two".into(),
        recent: String::new(),
    });
    session.messages.push(Message::User("next".into()));
    store.save(&session).await.expect("saves");
    let saved = store.load(session.id).await.expect("loads");

    let mut t = Transcript::replay("/repo".into(), &saved.messages);

    assert_eq!(
        t.entries().nth(2),
        Some(&Entry::Compacted("\n\n## Findings\n\n- one\n- two".into())),
        "the save round-trips the marker and the replay shows it"
    );
    let total = t.layout(60, ChatSettings::default());
    assert_eq!(
        text(&t.visible(0, total)),
        [
            "▎ go",
            "",
            "▎ done",
            "",
            "  ◘ compacted · ## Findings",
            "",
            "▎ next",
        ]
    );
    assert_eq!(total, 7);
}

#[test]
fn a_markdown_summary_with_leading_blanks_is_still_one_row() {
    let mut t = transcript();
    t.apply(&Event::Compacted {
        summary: "\n\n## Findings\n\n- one\n- two".into(),
    });

    // Whatever the pane width, the divider never wraps.
    for width in [10, 60] {
        let total = t.layout(width, ChatSettings::default());
        assert_eq!(total, 1, "one row at {width} columns");
        assert_eq!(
            text(&t.visible(0, total)),
            ["  ◘ compacted · ## Findings"],
            "the first line with something on it"
        );
    }
}

#[test]
fn a_compaction_copies_the_whole_summary() {
    let entry = Entry::Compacted("\n\n## Findings\n\n- one\n- two".into());

    assert_eq!(
        entry.clipboard().as_deref(),
        Some("\n\n## Findings\n\n- one\n- two"),
        "the model's whole summary, not only the divider's line"
    );
}

#[test]
fn replays_a_saved_session() {
    let bash = tool("1", "bash", r#"{"command":"ls"}"#);
    let messages = [
        Message::System("you are nth".into()),
        Message::User("go".into()),
        Message::Assistant(nth_protocol::AssistantMessage {
            text: "looking".into(),
            reasoning: "hm".into(),
            tool_calls: vec![bash.clone(), call("2")],
        }),
        Message::ToolResult {
            call_id: "1".into(),
            content: "a.rs\nb.rs".into(),
            images: Vec::new(),
        },
        Message::ToolResult {
            call_id: "2".into(),
            content: "Error: no such file".into(),
            images: Vec::new(),
        },
        Message::Assistant(nth_protocol::AssistantMessage {
            text: "done".into(),
            ..Default::default()
        }),
    ];

    let t = Transcript::replay("/repo".into(), &messages);

    let entries: Vec<_> = t.entries().collect();
    assert_eq!(entries[0], &Entry::User("go".into()));
    assert!(matches!(
        entries[1],
        Entry::Reasoning { took: Some(_), text, .. } if text == "hm"
    ));
    assert_eq!(entries[2], &Entry::Answer("looking".into()));
    assert_eq!(
        entries[3],
        &Entry::Tool {
            call: bash,
            state: ToolState::Done,
            output: vec!["a.rs".into(), "b.rs".into()],
            notes: Vec::new(),
        }
    );
    assert_eq!(
        entries[4],
        &Entry::Tool {
            call: call("2"),
            state: ToolState::Failed("no such file".into()),
            output: Vec::new(),
            notes: Vec::new(),
        }
    );
    assert_eq!(entries[5], &Entry::Answer("done".into()));
    assert_eq!(entries.len(), 6);
}

#[test]
fn replay_keeps_what_a_write_reported() {
    let write = tool("1", "write", r#"{"filePath":"/repo/a.rs","content":"x"}"#);
    let messages = [
        Message::User("go".into()),
        Message::Assistant(nth_protocol::AssistantMessage {
            tool_calls: vec![write],
            ..Default::default()
        }),
        Message::ToolResult {
            call_id: "1".into(),
            content: "Wrote file: /repo/a.rs\n\nFormatted with rustfmt.".into(),
            images: Vec::new(),
        },
    ];

    let t = Transcript::replay("/repo".into(), &messages);

    let Some(Entry::Tool { output, notes, .. }) = t.entries().nth(1) else {
        panic!("no write entry");
    };
    assert_eq!(output, &["x"]);
    assert_eq!(notes, &[Note::Format("Formatted with rustfmt.".into())]);
}

#[test]
fn replay_keeps_what_an_edit_reported_as_notes() {
    let edit = tool("1", "edit", r#"{"filePath":"/repo/a.rs"}"#);
    let messages = [
        Message::User("go".into()),
        Message::Assistant(nth_protocol::AssistantMessage {
            tool_calls: vec![edit],
            ..Default::default()
        }),
        Message::ToolResult {
            call_id: "1".into(),
            content: "Edited file: /repo/a.rs\n\nFormatted with rustfmt.".into(),
            images: Vec::new(),
        },
    ];

    let t = Transcript::replay("/repo".into(), &messages);

    let Some(Entry::Tool { output, notes, .. }) = t.entries().nth(1) else {
        panic!("no edit entry");
    };
    assert!(output.is_empty(), "its diff comes from its arguments");
    assert_eq!(notes, &[Note::Format("Formatted with rustfmt.".into())]);
}

#[test]
fn replay_shows_a_skill_command_as_typed() {
    let messages = [Message::User(
        "/fix the build\n\n<skill_content name=\"fix\">\n# Skill: fix\n</skill_content>".into(),
    )];

    let t = Transcript::replay("/repo".into(), &messages);

    assert_eq!(
        t.entries().collect::<Vec<_>>(),
        [&Entry::User("/fix the build".into())]
    );
}

#[test]
fn replay_shows_a_skill_without_its_body() {
    let skill = tool("1", "skill", r#"{"name":"deploy"}"#);
    let messages = [
        Message::User("go".into()),
        Message::Assistant(nth_protocol::AssistantMessage {
            tool_calls: vec![skill.clone()],
            ..Default::default()
        }),
        Message::ToolResult {
            call_id: "1".into(),
            content: "<skill_content name=\"deploy\">\n# Skill: deploy\n</skill_content>".into(),
            images: Vec::new(),
        },
    ];

    let t = Transcript::replay("/repo".into(), &messages);

    assert_eq!(
        t.entries().nth(1),
        Some(&Entry::Tool {
            call: skill,
            state: ToolState::Done,
            output: Vec::new(),
            notes: Vec::new(),
        })
    );
}

#[test]
fn replay_hides_instructions_attached_to_a_read() {
    let messages = [
            Message::User("go".into()),
            Message::Assistant(nth_protocol::AssistantMessage {
                tool_calls: vec![call("1")],
                ..Default::default()
            }),
            Message::ToolResult {
                call_id: "1".into(),
                content: "1: fn main() {}\n\n<system-reminder>\nInstructions from: /repo/src/AGENTS.md\nBe brief.\n</system-reminder>\n".into(),
                images: Vec::new(),
            },
        ];

    let t = Transcript::replay("/repo".into(), &messages);

    assert_eq!(outputs(&t), [vec!["1: fn main() {}".to_string()]]);
}
