# nth-icons

Notes for working in this crate. The root `CLAUDE.md` has the commands, the crate map and the rules every crate keeps.

every icon nth draws, as one set (`PLAIN`: Unicode any font has, each icon one column wide) that the TUI and CLI read through `icons()`. A new icon is a field on `Icons`, never a literal at the call site. Depends on nothing else in the workspace.
