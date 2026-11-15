//! `nth connect`: signs in to ChatGPT in the browser, so its models run on
//! your subscription; `--status` says whether it is connected, `--logout`
//! signs out. Needs no provider configured: it never builds `Providers`.

use std::io::Write;

use anyhow::Result;
use nth_auth::ChatGpt;
use nth_icons::icons;
use owo_colors::OwoColorize;

use crate::output::{self, Report};

pub async fn run(status: bool, logout: bool, json: bool, chatgpt: ChatGpt) -> Result<()> {
    if status {
        let report = ConnectStatusReport {
            chatgpt: chatgpt.connected(),
        };
        return output::print(&report, json);
    }
    if logout {
        let signed_out = chatgpt.logout().await?;
        println!("{} disconnected from ChatGPT", icons().ok.green().bold());
        if signed_out == nth_auth::SignedOut::Unconfirmed {
            println!(
                "{}",
                "OpenAI did not confirm the session ended; disconnect nth in ChatGPT's settings to be sure."
                    .yellow()
            );
        }
        return Ok(());
    }
    let account = chatgpt
        .login(|url| {
            println!("{} {url}", icons().to.cyan().bold());
            // The URL above is the way in when no browser opens.
            let _ = nth_auth::open_browser(url);
            println!("{}", "waiting for authorization…".dimmed());
        })
        .await?;
    let who = match account.email {
        Some(email) => format!(" ({email})"),
        None => String::new(),
    };
    println!("{} connected to ChatGPT{who}", icons().ok.green().bold());
    Ok(())
}

/// Whether each provider that is signed in to rather than given a key is
/// connected. ChatGPT only, today.
struct ConnectStatusReport {
    chatgpt: bool,
}

impl ConnectStatusReport {
    /// Both halves list the same providers, in the same order.
    fn providers(&self) -> [(&'static str, &'static str, bool); 1] {
        [(ChatGpt::ID, "ChatGPT", self.chatgpt)]
    }
}

impl Report for ConnectStatusReport {
    fn text(&self, out: &mut dyn Write) -> Result<()> {
        let providers = self.providers();
        let width = providers
            .iter()
            .map(|(_, name, _)| name.len())
            .max()
            .unwrap_or(0);
        for (_, name, connected) in providers {
            if connected {
                writeln!(out, "{} {}", icons().ok.green().bold(), name.cyan())?;
            } else {
                writeln!(
                    out,
                    "{} {:width$}  {}",
                    icons().fail.red(),
                    name.dimmed(),
                    "not connected: run `nth connect`".dimmed()
                )?;
            }
        }
        Ok(())
    }

    fn json(&self, out: &mut dyn Write) -> Result<()> {
        for (id, name, connected) in self.providers() {
            let line = serde_json::json!({ "id": id, "name": name, "connected": connected });
            writeln!(out, "{line}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::testing;

    fn status(chatgpt: &ChatGpt, json: bool) -> String {
        let report = ConnectStatusReport {
            chatgpt: chatgpt.connected(),
        };
        match json {
            true => testing::json(&report),
            false => testing::text(&report),
        }
    }

    #[test]
    fn a_connected_provider_is_named() {
        let text = testing::text(&ConnectStatusReport { chatgpt: true });

        insta::assert_snapshot!(text);
    }

    #[test]
    fn a_provider_not_connected_says_how_to_connect() {
        let text = testing::text(&ConnectStatusReport { chatgpt: false });

        insta::assert_snapshot!(text);
    }

    #[test]
    fn the_status_is_a_json_line_per_provider() {
        let report = ConnectStatusReport { chatgpt: true };

        let value: serde_json::Value =
            serde_json::from_str(&testing::json(&report)).expect("one JSON line");

        insta::assert_json_snapshot!(value);
    }

    #[tokio::test]
    async fn status_says_whether_chatgpt_is_connected() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = nth_auth::Store::at(dir.path().join("auth.json"));
        let chatgpt = ChatGpt::new(store.clone()).expect("a client");

        assert_eq!(
            status(&chatgpt, true),
            "{\"connected\":false,\"id\":\"chatgpt\",\"name\":\"ChatGPT\"}\n"
        );
        let text = status(&chatgpt, false);
        assert!(text.contains(icons().fail), "{text}");
        assert!(text.contains("not connected"), "{text}");

        let auth = nth_auth::OAuth {
            access: "access".into(),
            refresh: "refresh".into(),
            expires: u64::MAX,
            account_id: None,
            client_id: Some("oaiapp_issued".into()),
            id_token: None,
            scopes: Vec::new(),
        };
        store.save(ChatGpt::ID, &auth).await.expect("saved");
        assert_eq!(
            status(&chatgpt, true),
            "{\"connected\":true,\"id\":\"chatgpt\",\"name\":\"ChatGPT\"}\n"
        );
        let text = status(&chatgpt, false);
        assert!(text.contains(icons().ok), "{text}");
        assert!(!text.contains("not connected"), "{text}");
    }
}
