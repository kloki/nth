use nth_protocol::{AssistantMessage, ToolCall};

use super::*;

fn user(text: &str) -> Message {
    Message::User(text.into())
}

fn answer(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        text: text.into(),
        ..AssistantMessage::default()
    })
}

fn calls(name: &str) -> Message {
    Message::Assistant(AssistantMessage {
        tool_calls: vec![ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments: "{}".into(),
        }],
        ..AssistantMessage::default()
    })
}

fn result(content: &str) -> Message {
    Message::ToolResult {
        call_id: "1".into(),
        content: content.into(),
        images: Vec::new(),
    }
}

#[test]
fn the_kept_part_is_the_newest_that_fits() {
    let messages = vec![
        Message::System("prompt".into()),
        user("old"),
        answer("a"),
        user("new"),
        answer("b"),
    ];
    // "[User]: new" and "[Assistant]: b" fit 5 tokens; the older turn
    // does not.
    let split = split(&messages, 5, false).expect("a conversation");
    assert_eq!(split.older, 3);
    assert_eq!(split.recent, "[User]: new\n\n[Assistant]: b");
}

#[test]
fn when_everything_fits_only_the_latest_exchange_is_kept() {
    let messages = vec![user("old"), answer("a"), user("new"), answer("b")];
    let split = split(&messages, 1_000, false).expect("a conversation");
    assert_eq!(split.older, 3, "the latest reply alone");
    assert_eq!(split.recent, "[Assistant]: b");
}

#[test]
fn a_single_message_is_summarized_whole_unless_an_earlier_checkpoint_kept_text() {
    let messages = vec![user("only")];
    assert_eq!(
        split(&messages, 1_000, false),
        Some(Split {
            older: 1,
            recent: String::new()
        }),
        "nothing kept"
    );
    assert_eq!(
        split(&messages, 1_000, true),
        Some(Split {
            older: 0,
            recent: "[User]: only".into()
        }),
        "everything kept; the earlier checkpoint is what gets summarized"
    );
    assert_eq!(split(&[Message::System("p".into())], 1_000, false), None);
}

#[test]
fn a_huge_tool_result_is_kept_cut_and_with_its_call() {
    // One turn: the prompt, a huge tool exchange, then more calls.
    let messages = vec![
        user("go"),
        calls("read"),
        result(&"x".repeat(40_000)),
        calls("grep"),
        result("found"),
    ];
    let split = split(&messages, 10, false).expect("a conversation");
    assert_eq!(split.older, 3, "starts at the call, never on a result");
    assert_eq!(
        split.recent,
        "[Assistant tool call]: grep {}\n\n[Tool result]: found"
    );

    // With room for the cut result but not for everything, the read
    // call that made it comes along: the grep exchange, the cut result
    // and the call are 5 + 7 + 319 + 7 tokens.
    let split = super::split(&messages, 335, false).expect("a conversation");
    assert_eq!(split.older, 1);
    assert!(split.recent.contains("[truncated]"), "{}", split.recent);
    assert!(split.recent.len() < 1_400, "the result is cut");
}

#[test]
fn exchanges_start_at_each_user_message_after_the_kept_text() {
    let messages = vec![
        Message::System("p".into()),
        user("one"),
        calls("read"),
        result("r"),
        user("two"),
        answer("done"),
    ];
    assert_eq!(
        exchanges("[User]: before", &messages),
        [
            "[User]: before",
            "[User]: one\n[Assistant tool call]: read {}\n[Tool result]: r",
            "[User]: two\n[Assistant]: done",
        ]
    );
    assert_eq!(exchanges("", &messages[..1]), Vec::<String>::new());
}

fn read_an_image() -> Message {
    Message::ToolResult {
        call_id: "1".into(),
        content: "Image read: shot.png, 1920×1080 png".into(),
        images: vec![nth_protocol::Image {
            mime: "image/png".into(),
            sha256: "ab".into(),
            width: 1920,
            height: 1080,
            path: "/images/ab.png".into(),
            data: Some("A".repeat(400_000).into()),
        }],
    }
}

#[test]
fn an_image_is_a_line_in_the_transcript() {
    assert_eq!(
        entry(&read_an_image()),
        Some(format!(
            "[Tool result]: Image read: shot.png, 1920×1080 png\n[Read image/png: /images/ab.png]\n{IMAGE_REMOVED}"
        ))
    );
}

#[test]
fn an_image_counts_as_a_screenshot_not_its_bytes() {
    let text_only = tokens(&result("Image read: shot.png, 1920×1080 png"));
    let with_image = tokens(&read_an_image());
    assert!(
        (IMAGE_TOKENS..IMAGE_TOKENS + 100).contains(&(with_image - text_only)),
        "{text_only} then {with_image}"
    );
}

#[test]
fn transcript_lines_name_who_spoke_and_cut_tool_results() {
    let long = "x".repeat(3_000);
    let reply = Message::Assistant(AssistantMessage {
        text: "reading".into(),
        reasoning: "hmm".into(),
        tool_calls: vec![ToolCall {
            id: "1".into(),
            name: "read".into(),
            arguments: r#"{"filePath":"a.rs"}"#.into(),
        }],
    });
    assert_eq!(
        entry(&reply).as_deref(),
        Some("[Assistant]: reading\n[Assistant tool call]: read {\"filePath\":\"a.rs\"}"),
        "reasoning is left out"
    );
    let cut = entry(&result(&long)).expect("a line");
    assert_eq!(
        cut,
        format!("[Tool result]: {}\n[truncated]", "x".repeat(1_250))
    );
    let shown = Message::ToolResult {
        call_id: "2".into(),
        content: "Image read".into(),
        images: vec![nth_protocol::Image {
            mime: "image/png".into(),
            sha256: "ab".into(),
            width: 1,
            height: 1,
            path: "/images/ab.png".into(),
            data: None,
        }],
    };
    assert_eq!(
        entry(&shown),
        Some(format!(
            "[Tool result]: Image read\n[Read image/png: /images/ab.png]\n{IMAGE_REMOVED}"
        )),
        "the image is named, and gone once it is text"
    );
    assert_eq!(entry(&Message::System("p".into())), None);
    assert_eq!(
        entry(&Message::Compaction {
            summary: "s".into(),
            recent: String::new()
        }),
        None
    );
}

#[test]
fn the_summary_request_folds_in_an_earlier_summary_and_notes_omissions() {
    let exchanges = ["[User]: a".to_string(), "[User]: b".to_string()];
    let fresh = summary_request(None, &exchanges, 0);
    assert!(fresh.starts_with(SUMMARY_PROMPT.trim_end()));
    assert!(!fresh.contains(UPDATE_PROMPT.trim_end()));
    assert!(fresh.ends_with("[User]: a\n\n[User]: b"));

    let update = summary_request(Some("S1"), &exchanges[1..], 1);
    assert!(update.contains(UPDATE_PROMPT.trim_end()));
    assert!(update.contains("<summary>\nS1\n</summary>"));
    assert!(update.ends_with("[1 older exchange omitted]\n\n[User]: b"));
}

#[test]
fn shrinking_drops_the_oldest_exchanges_but_keeps_the_newest() {
    let exchanges: Vec<String> = (0..4)
        .map(|n| format!("[User]: {}", "x".repeat(400 * (n + 1))))
        .collect();
    let full = text_tokens(&summary_request(None, &exchanges, 0));
    let without_first = text_tokens(&summary_request(None, &exchanges[1..], 1));

    assert_eq!(
        shrink(None, &exchanges, 0, full),
        Some(1),
        "at least one goes"
    );
    assert_eq!(shrink(None, &exchanges, 0, without_first - 1), Some(2));
    assert_eq!(
        shrink(None, &exchanges, 0, 1),
        None,
        "the newest never goes"
    );
    assert_eq!(
        shrink(None, &exchanges, 3, u64::MAX),
        None,
        "nothing left to drop"
    );
}

#[test]
fn a_checkpoint_wraps_the_summary_and_the_kept_text() {
    let both = checkpoint("S", "[User]: hi");
    assert!(both.starts_with("<conversation-checkpoint>"));
    assert!(both.contains("<summary>\nS\n</summary>"));
    assert!(both.contains("<recent-context>\n[User]: hi\n</recent-context>"));
    assert!(both.ends_with("</conversation-checkpoint>"));
    assert!(!checkpoint("S", "").contains("<recent-context>"));
}

#[test]
fn the_summary_prompt_keeps_the_sections_the_plan_promises() {
    for heading in [
        "## Objective",
        "## Important Details",
        "## Work State",
        "### Completed",
        "### Active",
        "### Blocked",
        "## Next Move",
        "## Relevant Files",
    ] {
        assert!(SUMMARY_PROMPT.contains(heading), "{heading} is missing");
    }
}
