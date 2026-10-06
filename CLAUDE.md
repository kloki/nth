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
- Headless: `cargo run -p nth -- run "<prompt>"` (act mode; `--mode plan` to plan). List models: `cargo run -p nth -- models`. List skills: `cargo run -p nth -- skills`. List agents: `cargo run -p nth -- agents`. List formatters: `cargo run -p nth -- formatters`. List language servers: `cargo run -p nth -- lsp`; what they say about a file: `cargo run -p nth -- lsp diagnostics <file>`.
- Config: optional `~/.config/nth/config.toml` (or `--config`, `NTH_CONFIG`); every key is in `docs/config.example.toml`, and `cargo run -p nth -- config` prints the resolved one. Flags and env vars win over it.

## Architecture

Crates in `crates/`, from the bottom up:

- **nth-context**: what nth reads about a project before the first prompt: instruction files (`AGENTS.md`, or `CLAUDE.md` where a project has none), skills (`SKILL.md` folders in the Claude Code, opencode, open-standard and nth places) and agents (`agents/`: opencode's built-in `general` and `explore`, plus one markdown file per agent in the Claude Code, opencode and nth places, the body being its system prompt), found by `Context::discover`, and the nested ones the read tool attaches (`instructions::nested`). `frontmatter` reads the YAML block skills and agents share. `Paths` carries the home and config directories so tests never touch the real ones. Depends on nothing else in the workspace.
- **nth-protocol**: types every crate shares: `Message`, `Event`, `Mode` (plan or act), and the `Provider` and `Tool` traits. Anything swappable sits behind one of these traits. Also the handles a tool reaches the front-end through, bundled as `FrontEnd` on `ToolContext`: `Asker` (questions), `Screen` (which tab shows) and `Monitors` (`monitor/`: the wire types, the registry of running commands and the notice text, kept together with its parser).
- **nth-llm**: `Provider` impls, one module per wire protocol. Only `chat_completions` exists today (OpenCode Go).
- **nth-format**: the formatters run after a tool writes a file: opencode's built-in table (`registry.rs`), each with a probe for whether it applies to the project, plus custom ones from `[format]` in the config. `Formatters::format` runs every match; `nth formatters` prints `Formatters::status`.
- **nth-lsp**: language server diagnostics, LSP written by hand (`transport`, `types`, `client/`). `server/` is opencode's server table (rust-analyzer, typescript-language-server, gopls, pyright and more, used only when on PATH) plus `[lsp]` from the config, with the project-root rules. `Lsp` is one process-wide, cheap-to-clone pool: `touch(path, wait)` starts a client per (server, root) on first use and returns the diagnostics, `status()` is a `watch` of every client's state, and `report` writes opencode's text for the model. `nth lsp` prints `Lsp::servers_for`.
- **nth-tools**: one module per tool (`read`, `write`, `edit`, `apply_patch`, `bash`, `glob`, `grep`, `webfetch`, `websearch`, `skill`, `question`, `panel`, `monitor`); `nth_tools::all()` lists them. `question` reaches you through the `Asker` on `ToolContext`; a front-end without one (`nth run`) has nobody to ask, so the model decides. `monitor` leaves a command running in a detached task (`process.rs` is what it shares with bash) and reports through `Monitors` on `ToolContext`, which keeps its stdout lines as notices for the model; without a front-end it refuses. `PostWrite` is what every file-writing tool calls after writing: it formats the file, asks the language servers about it, and returns the format note and their errors for the model. Read shares its `Lsp` and touches each file it reads without waiting, so the server is warm by the time the model writes.
- **nth-session**: `Session` (serializable history, model, effort, mode, cwd, and an unsaved `Context` that goes into the system prompt), `Store` (one JSON file per session under `$XDG_DATA_HOME/nth/sessions`, saved after every turn, behind `/resume` and `nth -c`) and `run_turn`, the agent loop: stream a reply, run its tool calls in parallel, feed results back, repeat until the model answers without tools (capped at `MAX_STEPS`). Between steps it hands the model what monitors said (`Monitors::take_notices`) as a user message. `plan/` is plan mode: the plan file's path (`.nth/plans/<id>.md`) and opencode's reminders, appended to the prompt on entering plan mode and on switching to act; in plan mode the `Writable` on `ToolContext` lets write, edit and apply_patch touch only the plan file. Progress goes out as `Event`s over an `mpsc` channel; cancellation is a `CancellationToken` and always leaves `messages` valid to continue from. `subagent/` runs the agents nth-context found: `Subagents` is the registry (one actor task per subagent, owning its `Session` and taking prompts from an inbox one after another, reporting `SubagentEvent`s to the front-end), `Task` is the tool the model starts or continues one with (with a front-end it returns at once and the answer lands in the `Monitors` inbox as a `<task>` notice; headless it waits), and `mention::resolve` turns `@name` in a prompt into opencode's instruction to call it. A subagent's session has `subagent` set: its system prompt carries the agent's prompt instead of the model's persona and no agents list, and it never gets `task`, `question`, `panel` or `monitor`.
- **nth-tui**: the interactive chat (ratatui + crossterm).
- **nth**: the clap CLI; with no subcommand it opens the TUI, `run` prints events with `render.rs`.

Front-ends only consume `Event`s; they never reach into the loop. The TUI moves the `Session` into a task for each turn and takes it back when the turn ends (`app/turn.rs`), so Esc cancels cooperatively rather than aborting the task.

### TUI

`App` (`crates/nth-tui/src/app/mod.rs`) runs one `tokio::select!` loop over terminal input, session events, the running turn and background jobs, redrawing on every step.

The screen is three bands, named as in `docs/ui.md`. Use these names in code and comments:

- **Content panel**: what you look at. `Content` (`app/content.rs`) holds the open tabs, shown in the header, and which one shows; a `Tab` is `Chat` (always first, never closed), `Diagnostics` (`diagnostics.rs`), `Plan` (`plan/`: the plan file with the latest turn's changes marked, opened while the file exists; `app/plan.rs` reads it and runs `/approve`; `app/editor.rs` is ctrl+g, which hands the terminal to your editor on a copy of the plan while its tab shows — its diff goes to the model as review feedback, `nth_session::plan::edits` — or of the prompt, whose edits become its text again) or `Monitor(id)` (`monitor.rs`, one per running command, closable only once stopped; `app/monitor.rs` wires them), and later ones (Diff) become new variants. The `panel` tool switches tabs through the `Screen` on `ToolContext`.
- **Input panel**: what you type into. The `Input` enum (`app/input.rs`) picks it: `Input::Prompt` by default, or a widget such as the model picker that swaps in and hands back to the prompt. `/` completes nth's commands and then the skills (`command/`); a skill runs by filling its template (`nth_context::skills::parse` and `Skill::invoke`), the same way `nth run "/name args"` does.
- **Mode**: Tab at the prompt switches plan and act (`app/mode.rs`); each mode keeps its own model and effort (`ModeLlms`, from `[mode]` in the config), and `App.model`/`App.effort` are the current mode's.
- **Commands**: `!` at the start of the prompt makes it a command (`Prompt::shell`, label `cmd` in yellow). Enter runs it through `Session::shell` on the untimed `Bash` that `nth_tui::Tools` hands over, as a turn without the model (`App::start_shell`); the session records it as opencode does, `SHELL_PROMPT` then a bash call and its result.
- **Status bar**: always `status::ROWS` (2) lines at the bottom.

Heights are decided bottom-up: the status bar, then the input panel's own `Input::rows()`, then the content panel takes the rest. A view's state lives on `App` (`App.chat`, `App.prompt`), not in the enum variant, so it keeps up with the session and keeps its text while hidden.

Markdown and code go through `rich.rs`, the only module that uses [hoodrich](https://github.com/kloki/hoodrich). Chat messages are concealed markdown. read, write, edit (as a diff) and webfetch output are highlighted by tool (`output_lines` in `chat/render.rs`). The plan tab renders the plan as markdown. hoodrich gives one line per source line, and `rich::wrap` wraps them with their styles kept, because every pane draws pre-wrapped lines.

TUI tests render `App` into a `TestBackend` and assert on rows (`rows()` in `app/mod.rs` tests), so layout changes mean updating row indexes there.

Style: no borders or divider lines; bands differ by background and spacing. Use only the 16 ANSI colours plus the terminal's default foreground and background, never hex or RGB (see `docs/design.md#tui`). The one exception is syntax-highlighted code, which hoodrich draws in Dracula colours.

## opencode reference

`refs/opencode` is a gitignored local clone of opencode pinned to the commit named in `docs/research/opencode/README.md`. The `research-opencode` skill in `.claude/skills/` studies it and writes notes under `docs/research/opencode/`.
