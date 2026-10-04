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
- Run the TUI: `cargo run -p nth`. It needs `OPENCODE_GO_API_KEY`; `NTH_MODEL` and `NTH_BASE_URL` (or `--model`, `--base-url`) pick the endpoint.
- Headless: `cargo run -p nth -- run "<prompt>"`. List models: `cargo run -p nth -- models`.

## Architecture

Crates in `crates/`, from the bottom up:

- **nth-protocol**: types every crate shares: `Message`, `Event`, and the `Provider` and `Tool` traits. Anything swappable sits behind one of these traits.
- **nth-context**: what nth reads about a project before the first prompt: instruction files (`AGENTS.md`, or `CLAUDE.md` where a project has none), found by `Context::discover`. `Paths` carries the home and config directories so tests never touch the real ones.
- **nth-llm**: `Provider` impls, one module per wire protocol. Only `chat_completions` exists today (OpenCode Go).
- **nth-tools**: one module per tool (`read`, `write`, `bash`); `nth_tools::all()` lists them.
- **nth-session**: `Session` (serializable history, model, effort, cwd, and an unsaved `Context` that goes into the system prompt), `Store` (one JSON file per session under `$XDG_DATA_HOME/nth/sessions`, saved after every turn, behind `/resume` and `nth -c`) and `run_turn`, the agent loop: stream a reply, run its tool calls in parallel, feed results back, repeat until the model answers without tools (capped at `MAX_STEPS`). Progress goes out as `Event`s over an `mpsc` channel; cancellation is a `CancellationToken` and always leaves `messages` valid to continue from.
- **nth-tui**: the interactive chat (ratatui + crossterm).
- **nth**: the clap CLI; with no subcommand it opens the TUI, `run` prints events with `render.rs`.

Front-ends only consume `Event`s; they never reach into the loop. The TUI moves the `Session` into a task for each turn and takes it back when the turn ends (`app/turn.rs`), so Esc cancels cooperatively rather than aborting the task.

### TUI

`App` (`crates/nth-tui/src/app/mod.rs`) runs one `tokio::select!` loop over terminal input, session events, the running turn and background jobs, redrawing on every step.

The screen is three bands, named as in `docs/ui.md`. Use these names in code and comments:

- **Content panel**: what you look at. The `Content` enum (`app/content.rs`) picks the view; only `Content::Chat` exists, and later tabs (Plan, Diff, Monitor) become new variants.
- **Input panel**: what you type into. The `Input` enum (`app/input.rs`) picks it: `Input::Prompt` by default, or a widget such as the model picker that swaps in and hands back to the prompt.
- **Status bar**: always `status::ROWS` (2) lines at the bottom.

Heights are decided bottom-up: the status bar, then the input panel's own `Input::rows()`, then the content panel takes the rest. A view's state lives on `App` (`App.chat`, `App.prompt`), not in the enum variant, so it keeps up with the session and keeps its text while hidden.

TUI tests render `App` into a `TestBackend` and assert on rows (`rows()` in `app/mod.rs` tests), so layout changes mean updating row indexes there.

Style: no borders or divider lines; bands differ by background and spacing. Use only the 16 ANSI colours plus the terminal's default foreground and background, never hex or RGB (see `docs/design.md#tui`).

## opencode reference

`refs/opencode` is a gitignored local clone of opencode pinned to the commit named in `docs/todo.md`. The `research-opencode` skill in `.claude/skills/` studies it and writes notes under `docs/research/opencode/`.
