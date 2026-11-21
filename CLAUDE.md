# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

nth ("N-th time") is a personal, opinionated coding harness in Rust: a cargo workspace that builds one `nth` binary. `docs/design.md` is the design doc, `docs/ui.md` the TUI layout spec, and `docs/todo.md` lists what opencode has that nth does not yet (a reference, not a backlog).

## Commands

CI runs these three, and all must pass:

```sh
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

- Single test: `cargo test -p nth-tui prompt_and_status_rows_never_move` (any substring of the test path works).
- Run the TUI: `cargo run -p nth`. It needs at least one usable provider: ChatGPT is built in and runs once `nth connect` signs in to your subscription; otherwise run `nth init` and uncomment one, with its API key set (OpenCode Go and Zen read `OPENCODE_API_KEY`). `NTH_MODEL` (or `--model`) picks the model as `provider/model`; with none set, a random one the providers list is used.
- Headless: `cargo run -p nth -- run "<prompt>"` (act mode; `--mode plan` to plan, `--agent <name>` to run as a primary agent; ctrl+c cancels the turn and saves the session, so `nth -c` continues it, and a second ctrl+c exits 130). Every other subcommand (`models`, `skills`, `agents`, `formatters`, `lsp`, `notify`, `memory`, `usage`, `work`, `session`, `connect`) is listed by `cargo run -p nth -- --help`.
- Preview a saved session's next logical request: `cargo run -p nth -- context [<id>]` (`--json`; JSON is automatic when stdout is redirected). It reconstructs from saved state, so another process's unsaved state and inbox are unavailable. Manage repository Curios with `cargo run -p nth -- curio --help` (no provider needed).
- Config: optional `~/.config/nth/config.toml` (or `--config`, `NTH_CONFIG`); `cargo run -p nth -- init` writes one with every key and its default (`--force` to overwrite; the template, `crates/nth-config/src/config.toml`, is the reference for every key, and has OpenCode Go and Zen commented out), and `cargo run -p nth -- config` prints the resolved one. Flags and env vars win over it. Another provider is a `[provider.<id>]` block with `base_url` and `api_key_env`; the id `chatgpt` is reserved for the built-in one.
- A new config key goes into the `nth init` template (`crates/nth-config/src/config.toml`, with its default and a comment) and into the internal `config` skill (`crates/nth-context/src/skills/internal/config/SKILL.md`) in the same change, so both keep listing every key; `every_key_is_in_the_template_and_the_skill` in nth-config fails on one missing from either.

## Architecture

Crates in `crates/`, from the bottom up. Each crate's details (its modules, invariants and gotchas) are in its own `crates/<crate>/CLAUDE.md`, which loads when you work in that crate; read it before changing the crate, and update it in the same change when a line there goes stale.

- **nth-directories**: checkout and main-checkout discovery, the main checkout's `.nth` layout as `Directories` accessors, `expand_home` and the directories `/add-dir` adds. Depends on nothing else in the workspace.
- **nth-context**: what nth reads about a project before the first prompt: instruction files and their `@path` imports, skills (nth's own in `skills/internal/`), agents (`agents/`) and the project memory (`.nth/memory/`). `Paths` keeps tests off the real home.
- **nth-protocol**: types every crate shares: `Message`, `Event`, `Mode`, the `Provider` and `Tool` traits, `ToolContext` and the handles a tool reaches the front-end through, and the next-request `Preview`.
- **nth-llm**: the `Provider` impls: the chat completions, messages and responses wire protocols, `providers/` (an endpoint per `[provider.<id>]` and the merged listing), the built-in ChatGPT endpoint and the models.dev catalogue.
- **nth-curios**: repository-local static-document review under `.nth/curios/`: the store, HTML validation and the browser viewer.
- **nth-format**: the formatters run after a tool writes a file.
- **nth-lsp**: language server diagnostics, LSP written by hand, one process-wide `Lsp` pool.
- **nth-notify**: desktop notifications. Depends on nothing else in the workspace.
- **nth-icons**: every icon nth draws, read through `icons()`. Depends on nothing else in the workspace.
- **nth-work**: the work plan on disk (`.nth/work/<session>/`) and nothing that runs it.
- **nth-worktree**: git worktrees under `.nth/worktrees` in the main checkout.
- **nth-auth**: signing in to providers that take an account (ChatGPT), kept in `auth.json`. Depends on nothing else in the workspace.
- **nth-tools**: one module per tool, listed by `nth_tools::all()`; `PostWrite` and `write_atomic` for every file write, `process` for commands.
- **nth-session**: `Session`, its `Store`, the agent loop (`Turn::run`), compaction, the `Inbox`, subagents, the work tools, plan mode, usage and the session's side of Curios.
- **nth-config**: `Config`, the global and repository layers, and `Selection`, what every kind of turn runs on.
- **nth-tui**: the interactive chat (ratatui + crossterm).
- **nth**: the clap CLI, `nth run`, the `Report` pattern every printing command follows, and `runtime/`, the one way a session starts.

Rules every crate keeps:

- Every path to a main checkout's `.nth` state is a `Directories` accessor (nth-directories), never a literal at the call site; tests may spell the layout out to pin it. The exception is nth-context's walk tables, `.nth/skills` and `.nth/agents`, looked for in every directory of the walk.
- A path the user or the model types reads a leading `~` through `nth_directories::expand_home`, the one `~` rule.
- A new icon is a field on `Icons` (nth-icons), never a literal at the call site.
- Anything swappable sits behind a trait in nth-protocol.
- Front-ends only consume `Event`s; they never reach into the loop.

Style: no borders or divider lines; bands differ by background and spacing. Use only the 16 ANSI colours plus the terminal's default foreground and background, never hex or RGB (see `docs/ui.md`). The one exception is syntax-highlighted code, which hoodrich draws in Dracula colours.

## opencode reference

`refs/opencode` is a gitignored local clone of opencode pinned to the commit named in `docs/research/opencode/README.md`. The `research-opencode` skill in `.claude/skills/` studies it and writes notes under `docs/research/opencode/`; the `opencode-changes` skill reviews what opencode changed since the pin (each feature linked to its commit, with a verdict on adopting it) under `docs/research/opencode/changes/`, then moves the pin.
