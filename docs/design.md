# nth — Design Doc

Sep 28, 2026 · Koen Klinkers · updated Oct 10, 2026

nth ("N-th time") is a personal, opinionated coding harness in Rust: a tabbed TUI over an agent loop, with git worktrees for working on branches in isolation. It copies opencode's defaults wherever there is no strong opinion. Its novelty budget goes to four things: worktree-native multi-agent work, the TUI, long-running plan and review flows, and plans you review with inline comments. The TUI and the worktree tools are built; the flows and comment threads are direction.

## Goals

1. Daily-drivable for real work on nth itself, so it is dogfooded from week one.
2. Learn the internals of a harness: provider streaming, tool calling, context management, LSP.
3. Every subsystem sits behind a trait in its own crate, so a provider, tool set or view can be swapped without touching the core.
4. **Multi-agent by default.** Several agents work on one machine at once without stepping on each other: parallel sessions, subagents, and flows that coordinate them. The UI makes it obvious at a glance which agents exist, what each is doing, and which one is waiting for you.
5. **Plans are reviewed with comments.** Inline comment threads on a plan or design; the agent answers or revises each one, and approval means every thread is resolved.
6. **Clean git history.** Base only ever gets one well-described commit per task. Checkpoint commits and agent noise never reach it.
7. **Agent-native.** Every action is one command that the TUI, the CLI and agent tools all issue, and every view has a text form an agent can read. Nothing is TUI-only.

## Non-goals

- Configurability for other users. Opinions are hardcoded; `~/.config/nth/config.toml` only tunes defaults such as the provider, model, tool limits and extra skill folders, and secrets stay in environment variables.
- A client/server split, web UI, desktop app or IDE plugin. opencode has these; nth does not need them. The one exception is a local Unix socket on a running nth carrying the same commands and events as JSON lines — still direction.
- Plugin loading at runtime. Swapping happens at compile time through crates and cargo features.
- Multi-user or team features, telemetry, sharing sessions by link.
- MCP. Integrations are skills that call REST APIs or CLIs through bash.
- Process sandboxing. Worktrees are for parallel agents, not security.

## What is built

CLAUDE.md holds the crate map and the internals; this is the design-level shape.

**The loop and sessions.** One turn at a time: stream a reply, run its tool calls in parallel, feed the results back, repeat until the model answers without tools. Esc cancels a turn and always leaves the history valid to continue from. A session is one JSON file under `$XDG_DATA_HOME/nth/sessions`, saved after every turn; `nth -c` and `/resume` rebuild the chat from it. A prompt sent mid-turn pre-empts the tool calls the model was about to run — they are answered as interrupted, and the model hears the message at its next step. Everything else the model should know, such as monitor output and subagent answers, reaches it between steps as inbox notices.

**Tools.** read, write, edit, apply_patch, bash, glob, grep, webfetch, websearch, skill, question, panel, monitor and the worktree pair. read warms the language server without waiting; a write runs the formatters and language servers before the model hears back. A long command runs as a monitor whose output reaches the model between steps; the question tool asks you 1–4 questions mid-turn.

**Context.** nth-context discovers what goes in before the first prompt: instruction files (`AGENTS.md`, or `CLAUDE.md` where a project has none), skills (`SKILL.md` folders in the Claude Code, opencode, open-standard and nth places) and agents (opencode's built-in `general` and `explore`, plus one markdown file per agent). Instructions from the repo root down to the cwd go in, and deeper files ride on the first read below them. Skills load on demand through the `skill` tool and run as `/name args`.

**Modes.** Plan and Act, Tab apart. Plan may write only its plan file, `.nth/plans/<session>.md`; Act has every tool. Each mode keeps its own model and effort. `/approve` tells the model the plan is approved and switches to Act.

**Providers.** Nothing is built in: each `[provider.<id>]` config block names a `base_url` and the environment variable its key lives in, and nth refuses to chat without a provider and a model. A model id is `provider/model`, so several endpoints serve one list. Requests stream over chat completions, Anthropic messages or OpenAI responses, chosen by the models.dev catalogue; a model the catalogue does not know goes over chat completions, and one whose protocol nth does not speak is not listed. 429s, 5xx and dropped streams retry with backoff.

**Formatting and LSP.** Formatters from opencode's table plus `[format]`, run after every write: each matching formatter whose program is installed runs, and the model gets a one-line note, never the formatted file. Language servers from opencode's table plus `[lsp]`, one lazily started client per server and project root; after a write the model gets the errors, warnings left out.

**Worktrees.** nth-worktree keeps git worktrees under `.nth/worktrees` in the main checkout, one per name and on the branch of that name. `enter_worktree` opens one and moves the session there; `exit_worktree` moves it back. Both refuse in plan mode, since the plan file lives under the working directory.

**TUI.** Three fixed bands — content panel, input panel, status bar — with a tab per thing you watch: Chat, Diagnostics, Usage, Plan, one per monitor and one per subagent. The full layout spec is [ui.md](ui.md). Desktop notifications go through nth-notify while the terminal is not focused.

**Subagents.** The task tool runs a named agent on its own session in the background; the call returns at once and the answer reaches the model as an inbox notice, or as the tool's result headless. Each gets a tab, and the prompt on that tab talks to it. A model-started turn is capped at `[task] max_steps` (50) or `[task] timeout_secs` (600, 0 for no clock), and can be stopped and continued by `task_id`.

**Usage.** Every turn's reported tokens go into a ledger saved with the session; the Usage tab and `nth usage` report what it spent, per model and per turn, at models.dev's list prices. The one output compression is [rtk](https://github.com/rtk-ai/rtk): when it is on PATH, bash runs the model's commands as `rtk rewrite` rewrites them, so git, cargo and test output reach the model already filtered. Beyond that there is no context management so far.

## Direction

Not a schedule: the shape of what comes next, decided before work starts.

**Worktree-native multi-agent.** Every session gets its own worktree and branch, created before the first prompt; a checkpoint commit after each turn, so undoing a turn is a reset to the previous one. Landing rebases the branch onto base, squashes it and fast-forwards; discarding deletes branch and worktree. Worker subagents get a worktree branched from the parent's and land there, never on base. An agents sidebar lists every agent as a tree with status and current action.

**Plan review with comments.** In the Plan view, select a line or range and press `c` for a thread. Threads live beside the file in `<file>.comments.jsonl`, anchored by quoted text so they survive edits. Sending the review gives the agent every open thread; it replies, or edits the file and replies with what changed. Approve is disabled while any thread is open, and resolved threads stay for history.

**Long-running flows.** Markdown files plus agents with fixed prompts; no workflow engine. Planning: brief → design → plan, each step reading the previous file and approved before the next, the files committed under `.nth/projects/<name>/`. Build: one session per approved task, its own worktree. Review: parallel reviewers per dimension (correctness, simplicity, tests, guidelines), a verify pass that tries to disprove each finding, findings in `review.md`, and a gate before landing.

**Context management.** opencode caps every tool's output (the rest is spilled to disk with a grep-or-delegate hint), prunes old tool results, and compacts a session into a structured summary plus a verbatim tail once it reaches the usable window. nth caps some tools but never shrinks a session, so every step re-sends every result. The researched plan is in [docs/research/opencode/03-context-management.md](research/opencode/03-context-management.md).

**Agent-native control.** Every action is a command the TUI, the CLI and agent tools share, and every view has a text or JSON form. Built so far: `nth models`, `agents`, `skills`, `formatters`, `lsp` and `usage`. Still to come: the local socket on a running TUI, and a CLI form for the rest.

[docs/todo.md](todo.md) tracks these against opencode's code.

## Rules

Opinions that are hardcoded, not configurable.

- **No tiling.** Three fixed bands ([ui.md](ui.md)), no layout engine.
- **16 ANSI colours**, so the terminal theme drives the look. The one exception is syntax-highlighted code in Dracula, through hoodrich; [ui.md](ui.md#colours) has the table.
- **Worktrees are parallelism, not sandboxing.** nth never limits what a process can do. The only write gate is plan mode.
- **Prompts live in template files**, not inline strings.
- **One task owns a session's state.** Events are the front-end boundary; front-ends never reach into the loop.
- **Clean history.** Base gets one well-described commit per task; checkpoint and agent noise never reach it.
