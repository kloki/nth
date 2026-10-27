//! `/connect`: the panel's login job. Enter starts the provider's browser
//! login in the background; its URL comes back first, to show and open,
//! then the account, and the model picker opens on the models it brought.

use nth_auth::{Account, ChatGpt};
use tokio::sync::oneshot;

use super::{App, Job, input::Input, keys::Action};
use crate::{
    connect::{ConnectPanel, Connectable},
    terminal,
};

/// The login running; dropping it stops the login, freeing its port.
pub(super) type Login = Job<Result<Account, nth_auth::Error>>;

impl App {
    /// A key while the connect panel is open.
    pub(super) fn key_connect(&mut self, action: Action) {
        let Input::Connect(panel) = &mut self.input else {
            return;
        };
        let waiting = panel.waiting().is_some();
        match action {
            Action::SelectNext if !waiting => panel.next(),
            Action::SelectPrev if !waiting => panel.prev(),
            Action::Submit if !waiting => self.start_login(),
            Action::Insert('c') if waiting => self.copy_login_url(),
            // Stops the login too, never a running turn.
            Action::Interrupt | Action::ClearOrQuit => self.close_connect(),
            _ => {}
        }
    }

    /// The account handle sign-ins go through, from the config's store.
    pub fn with_chatgpt(mut self, chatgpt: ChatGpt) -> Self {
        self.chatgpt = Some(chatgpt);
        self
    }

    pub(super) fn open_connect(&mut self) {
        let connected = Connectable::ALL.map(|to| match to {
            Connectable::ChatGpt => self.chatgpt.as_ref().is_some_and(ChatGpt::connected),
        });
        self.completion = None;
        self.input = Input::Connect(ConnectPanel::new(connected));
    }

    /// Starts the highlighted provider's login, whose URL lands in
    /// `connect_url`.
    pub(super) fn start_login(&mut self) {
        let Input::Connect(panel) = &mut self.input else {
            return;
        };
        if panel.waiting().is_some() {
            return;
        }
        let Some(chatgpt) = self.chatgpt.clone() else {
            self.hint = Some("nowhere to keep credentials".into());
            return;
        };
        match panel.start() {
            Connectable::ChatGpt => {
                let (url_tx, url_rx) = oneshot::channel();
                self.connect_url = Some(url_rx);
                self.login.start(|_| {
                    tokio::spawn(async move {
                        chatgpt
                            .login(move |url| {
                                // Gone only once the login is cancelled.
                                let _ = url_tx.send(url.to_string());
                            })
                            .await
                    })
                });
            }
        }
    }

    /// Shows where to sign in and sends the browser there.
    pub(super) fn login_url(&mut self, url: String) {
        // Shown either way, so a browser that did not open is no dead end.
        let _ = (self.opener)(&url);
        if let Input::Connect(panel) = &mut self.input {
            panel.set_url(url);
        }
    }

    pub(super) fn copy_login_url(&mut self) {
        let Input::Connect(panel) = &self.input else {
            return;
        };
        let Some(url) = panel.url() else {
            return;
        };
        if url.len() > terminal::COPY_LIMIT {
            self.hint = Some("too large to copy".into());
            return;
        }
        self.hint = Some(match (self.clipboard)(url) {
            Ok(()) => "copied".into(),
            Err(e) => format!("could not copy: {e}").into(),
        });
    }

    /// Esc: stops a running login, and closes the panel.
    pub(super) fn close_connect(&mut self) {
        self.login = Login::default();
        self.connect_url = None;
        self.input = Input::Prompt;
    }

    /// Lists the models again, since a new provider brings its own, and
    /// opens the picker on them, as opencode does.
    pub(super) fn connected(&mut self, account: Result<Account, nth_auth::Error>) {
        self.connect_url = None;
        let showing = matches!(self.input, Input::Connect(_));
        match account {
            Ok(account) => {
                let to = Connectable::ChatGpt.name();
                self.hint = Some(match account.email {
                    Some(email) => format!("connected to {to} as {email}").into(),
                    None => format!("connected to {to}").into(),
                });
                // A listing from before the login would land without the
                // new provider's models.
                self.llm_listing = Job::default();
                self.llms = None;
                if showing {
                    self.open_llm_picker();
                } else {
                    self.list_llms();
                }
            }
            Err(e) => {
                self.chat.transcript.push_error(format!("connect: {e}"));
                if showing {
                    self.input = Input::Prompt;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{
        Event as TermEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use nth_auth::Store;

    use super::*;
    use crate::{
        app::{
            keys::Action,
            tests::{app, rows},
        },
        command::Command,
    };

    /// An app signing in to a store in `dir`, connected when `connected`.
    async fn connect_app(dir: &std::path::Path, connected: bool) -> App {
        let store = Store::at(dir.join("auth.json"));
        if connected {
            let oauth = nth_auth::OAuth {
                access: "access".into(),
                refresh: "refresh".into(),
                expires: u64::MAX,
                account_id: None,
            };
            store.save(ChatGpt::ID, &oauth).await.expect("saves");
        }
        let mut app = app().with_chatgpt(ChatGpt::new(store).expect("client"));
        app.clipboard = |_| Ok(());
        app.opener = |_| Ok(());
        app
    }

    #[tokio::test]
    async fn connect_completes_in_the_popup() {
        let mut app = app();
        for c in "/con".chars() {
            app.apply(Action::Insert(c));
        }
        app.apply(Action::Submit);
        assert!(matches!(app.input, Input::Connect(_)));
        assert!(app.prompt.is_empty());
    }

    #[tokio::test]
    async fn the_panel_marks_chatgpt_once_connected() {
        let dir = tempfile::tempdir().expect("tempdir");

        let mut app = connect_app(dir.path(), false).await;
        app.run_command(Command::Connect);
        let shown = rows(&mut app);
        assert_eq!(
            crate::app::tests::prompt_row(&shown, 2).trim_end(),
            " ▎ connect"
        );
        assert_eq!(
            crate::app::tests::prompt_row(&shown, 3).trim_end(),
            " ▎ → ChatGPT    a Plus or Pro subscript"
        );

        let mut app = connect_app(dir.path(), true).await;
        app.run_command(Command::Connect);
        assert_eq!(
            crate::app::tests::prompt_row(&rows(&mut app), 3).trim_end(),
            " ▎ → ChatGPT ✓  a Plus or Pro subscript"
        );
    }

    #[tokio::test]
    async fn waiting_shows_the_url_and_copies_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = connect_app(dir.path(), false).await;
        app.run_command(Command::Connect);
        // Not `start_login`, which would bind the real callback port.
        if let Input::Connect(panel) = &mut app.input {
            panel.start();
        }
        app.login_url("https://auth.example/authorize?state=1".into());

        let shown = rows(&mut app);
        let panel: Vec<&str> = shown[9..13].iter().map(|r| r.trim_end()).collect();
        assert_eq!(
            panel,
            [
                " ▎ connect      c copy url · esc cancel",
                " ▎ ChatGPT",
                " ▎ https://auth.example/authorize?stat…",
                " ▎ Waiting for authorization in your br",
            ]
        );
        assert_eq!(
            crate::app::tests::status_row(&shown, 0).trim_end(),
            " /repo 0m",
            "the status bar stays"
        );

        app.apply(Action::Insert('c'));
        assert_eq!(app.hint.as_deref(), Some("copied"));

        app.on_terminal(TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 11,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(
            app.hint.as_deref(),
            Some("opened https://auth.example/authorize?state=1"),
            "a click opens the whole URL"
        );
    }

    #[tokio::test]
    async fn esc_cancels_the_login_and_returns_to_the_prompt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = connect_app(dir.path(), false).await;
        app.prompt.insert_str("half typed");
        app.run_command(Command::Connect);
        if let Input::Connect(panel) = &mut app.input {
            panel.start();
        }
        app.login.start(|_| tokio::spawn(std::future::pending()));
        let login = app.login.abort_handle().expect("running");

        app.apply(Action::Interrupt);
        tokio::task::yield_now().await;

        assert!(matches!(app.input, Input::Prompt));
        assert_eq!(app.prompt.text(), "half typed", "the prompt kept its text");
        assert!(login.is_finished(), "the login stopped");
        assert!(!app.is_busy(), "and no turn was interrupted");
    }

    #[tokio::test]
    async fn a_login_opens_the_picker_and_a_failure_says_why() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = connect_app(dir.path(), false).await;
        app.llms = Some(Default::default());

        app.run_command(Command::Connect);
        app.connected(Ok(Account {
            id: None,
            email: Some("me@example.com".into()),
        }));
        assert!(matches!(app.input, Input::LlmPicker(_)));
        assert!(
            app.llms.is_none() && app.llm_listing.is_running(),
            "listed again"
        );
        assert_eq!(
            app.hint.as_deref(),
            Some("connected to ChatGPT as me@example.com")
        );

        app.run_command(Command::Connect);
        app.connected(Err(nth_auth::Error::Timeout));
        assert!(matches!(app.input, Input::Prompt));
        assert!(
            rows(&mut app)
                .concat()
                .contains("connect: authorization took"),
            "the error is in the chat"
        );
    }
}
