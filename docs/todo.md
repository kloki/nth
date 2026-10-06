# nth

Items are grouped by the milestones in [design.md](design.md). Each item names the opencode code to copy from.

### Commands

- [ ] /init Set agent.md
- [ ] /compact compact long lines

### Agent loop hardening

- [x] Retry with backoff on 429, 5xx and dropped streams: start at 2 s, factor 2, and honor `retry-after`. Show "retrying in Ns" in the TUI. `session/retry.ts:26`
- [x] Doom-loop guard: when the same tool is called with the same input 3 times in a row, ask the user before continuing. `session/processor.ts:29`
- [x] Max-steps prompt: on the last allowed step, tell the model to stop calling tools and summarize, instead of cutting it off. `session/prompt.ts:1281`

### Context and instructions

- [x] Environment block: add today's date and the workspace root. `session/system.ts:80`
- [x] Per-model system prompts: only when a model misbehaves (`kimi.txt`, `gpt.txt`, `gemini.txt`). `session/prompt/`

### Subagents and orchestrations

- [x] Subagent: the task tool, async, answers as notices. `tool/task.ts`
- [x] Subagent tabs, with the prompt talking to the showing one
- [ ] Orchestrations view
- [ ] Save subagent sessions under the parent's, so `/resume` brings them back
- [ ] `[agents] paths` in the config, like `[skills] paths`
- [ ] A global cap on concurrent model requests across subagents
- [ ] Rename `Monitors` to the model's inbox it has become

### Worktrees

An opinionated worktree flow baked into nth requires design

### Scratchpad

A more flexible todo that uses the tab view

### Mouse control

Allow mouse control

- [ ] Switching tabs
- [ ] Clicking to copy

### TUI

- [x] Shell mode: `!` at the start of the prompt runs the line as a shell command and adds its output to the chat. `tui/component/prompt/index.tsx:836`
- [ ] Paste summary: collapse a large paste to `[pasted N lines]`. `tui/` `app.toggle.paste_summary`
- [ ] Tool details toggle: expand a collapsed tool call in place. `tui/` `session.toggle.actions`
- [ ] Thinking toggle: show or hide reasoning. `tui/` `session.toggle.thinking`

## Review backlog

Design debt and test gaps from the 2026-10-05 project review. Bugs from that review were fixed in their own PRs; these are the larger reshapes that were left.

### Design

- [ ] One table of per-tool presentation in the TUI (icon, streams, keeps first or last, writes files, shows result) instead of name-matched arms in `Transcript::replay`, `apply`, `output_lines` and `App::on_session`. Adding a tool should touch one place, and `replay` becomes `apply` fed by the saved messages.
- [ ] One `Viewport` struct for scroll bookkeeping (`top`, `max_top`, `height`, page and jump), embedded in the chat, diagnostics, monitor and plan views, instead of four copies plus the delegation in `app/content.rs`.
- [ ] Streaming render cost: every `TextDelta` invalidates the whole answer and hoodrich re-renders it, and `visible()` clones every cached line each frame. Render only the tail paragraph live, or cache per-item line offsets so `visible` can seek.
- [ ] Move the big test modules (`app/turn.rs`, `app/keys.rs`, `chat/transcript.rs`) into sibling `tests.rs` files; split `replay` out of `transcript.rs`.
- [ ] Shared picker pieces: `llm_picker/view.rs` and `session_picker/view.rs` duplicate the header, the keys-fit check and the Loading/Failed rows and their `State` enums.
- [ ] Per-path edit lock in nth-tools: formatters run under the global `EDITS` lock, so a cold prettier serialises every parallel edit of every file. A `Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>` keeps the race fix and the parallelism.
- [ ] Bounded bash output: bash buffers the whole stdout and tails at the end; keep a ring of `max_output_chars` while streaming. Same note for read and grep, which read whole files before counting lines.
- [ ] One timeout parameter convention across tools: `timeout` is ms in bash, seconds in webfetch, and `timeout_ms` in monitor next to camelCase `filePath` and `numResults`.
- [ ] Decide on an SSRF guard for webfetch (`169.254.169.254`, `localhost:<port>`); opencode has the same gap.
- [ ] Monitor flood guard counts stdout only, so a stderr flood still sends one event per line to the front-end and the log.
- [ ] `docs/design.md` describes a `Command` type, a `broadcast` event channel, an `nth-worktree` crate and a `views/` folder that do not exist. Either a "what shipped differently" section or a dated banner.

### Tests

- [ ] TUI layout tests hard-code row indexes 9, 10, 14 and 15 in eight files; derive `prompt_rows`/`status_rows` helpers from `header::ROWS`, `prompt::ROWS` and `status::ROWS` so one layout change does not break about 25 assertions.
- [ ] Bash timing tests (`sleep 0.2`, 300 ms drop) are load-sensitive; use a pipe-driven fixture or larger margins.
- [ ] `wire.rs` only tests the effort field; assert the message, `tool_calls`, `reasoning_content` and tool-result shape.
- [ ] Retry tests only fail at `stream()` open; add a provider that streams text and then errors, to prove a retried step starts a fresh reply. Also `max_steps: 0`.
- [ ] `store.rs`: save the same session twice and assert the list has one entry.
- [ ] Editor handoff error exits (`write_copy` failure, non-zero editor status leaving the terminal restored), the main loop's end-of-turn drain, and a tiny-terminal (40x4) case pinning "the status bar is the last to go".
- [ ] Probes `NodeDep`, `NodeMarker`, `Help` and `Ruff` have no unit tests; `run/render.rs` `Printer` line-breaking and `skills.rs` `shorten` are untested.
- [ ] nth-context: `@/abs/path` and `@../x` attachments; `nth-tools`: absolute glob patterns, `*** End of File` on a pure addition in apply_patch, webfetch redirects and timeouts.

## Deliberately skipped

These are non-goals in [design.md](design.md):

- MCP client (`mcp/`), plugins (`plugin/`, `packages/plugin`)
- Client/server split, web, desktop, IDE and ACP (`server/`, `packages/app`, `packages/desktop`, `acp/`, `ide/`)
- Session sharing, accounts and orgs (`share/`, `account/`, `dialog-console-org.tsx`)
- GitHub app and PR commands (`cli/cmd/github.ts`, `cli/cmd/pr.ts`)
- Themes (nth uses the 16 ANSI colors), self-upgrade and uninstall
