# nth-notify

Notes for working in this crate. The root `CLAUDE.md` has the commands, the crate map and the rules every crate keeps.

desktop notifications. `Event::notification` fills the texts in `templates/` from a `Context` (session title, elapsed, last reply); a `Backend` shows them (`notify_send.rs` on Linux, `osascript.rs` on macOS, the platform's by default; `[notify]` picks one via `BackendKind`; each runs its command through `command::run`, killed after `SEND_TIMEOUT`, 5 s, so a daemon that stopped answering holds up nothing), and `Notifier` is the cheap-to-clone handle a front-end sends through without waiting. Depends on nothing else in the workspace.
