//! `nth connect`: signs in to ChatGPT in the browser, so its models run on
//! your subscription; `--status` says whether it is connected, `--logout`
//! signs out. Needs no provider configured: it never builds `Providers`.

use std::io::{IsTerminal, Write};

use anyhow::Result;
use nth_auth::ChatGpt;
use nth_icons::icons;
use owo_colors::OwoColorize;

pub async fn run(status: bool, logout: bool, json: bool) -> Result<()> {
    let chatgpt = crate::chatgpt(&nth_context::Paths::from_env())?;
    if status {
        let mut out = std::io::stdout().lock();
        let json = json || !out.is_terminal();
        return write_status(&mut out, &chatgpt, json);
    }
    if logout {
        chatgpt.logout().await?;
        println!("{} disconnected from ChatGPT", icons().ok.green().bold());
        return Ok(());
    }
    let account = chatgpt
        .login(|url| {
            println!("{} {url}", icons().to.cyan().bold());
            nth_auth::open_browser(url);
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

/// One line per provider that is signed in to rather than given a key:
/// whether it is connected. ChatGPT only, today.
fn write_status(out: &mut impl Write, chatgpt: &ChatGpt, json: bool) -> Result<()> {
    let providers = [(ChatGpt::ID, "ChatGPT", chatgpt.connected())];
    let width = providers
        .iter()
        .map(|(_, name, _)| name.len())
        .max()
        .unwrap_or(0);
    for (id, name, connected) in providers {
        if json {
            let line = serde_json::json!({ "id": id, "name": name, "connected": connected });
            writeln!(out, "{line}")?;
        } else if connected {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn status(chatgpt: &ChatGpt, json: bool) -> String {
        let mut out = Vec::new();
        write_status(&mut out, chatgpt, json).expect("written");
        String::from_utf8(out).expect("utf-8")
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
