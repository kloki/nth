# nth

What opencode has that nth does not yet, plus nth's own review backlog. Each item names the opencode code to copy from, where there is one.

### Subagents and orchestrations

- [ ] Save subagent sessions under the parent's, so `/resume` brings them back
- [ ] `[agents] paths` in the config, like `[skills] paths`
- [ ] A global cap on concurrent model requests across subagents
- [ ] Skills typed on a subagent's tab: expand them as the chat does, rather than send `/name args` as text
- [ ] Name nested agent files by path (`group/name`) as opencode does, rather than by file alone

### Worktrees

An opinionated worktree flow baked into nth requires design

### Scratchpad

A more flexible todo that uses the tab view

### TUI

- [ ] Paste summary: collapse a large paste to `[pasted N lines]`. `tui/` `app.toggle.paste_summary`
- [ ] Tool details toggle: expand a collapsed tool call in place. `tui/` `session.toggle.actions`
- [ ] Thinking toggle: show or hide reasoning. `tui/` `session.toggle.thinking`

## Review backlog

Design debt and test gaps from the 2026-10-05 project review. Bugs from that review were fixed in their own PRs; these are the larger reshapes that were left.

### Design

- [ ] One table of per-tool presentation in the TUI (icon, streams, keeps first or last, writes files, shows result) instead of name-matched arms in `Transcript::replay`, `apply`, `output_lines` and `App::on_session`. Adding a tool should touch one place, and `replay` becomes `apply` fed by the saved messages.
- [ ] Streaming render cost: every `TextDelta` invalidates the whole answer and hoodrich re-renders it, and `visible()` clones every cached line each frame. Render only the tail paragraph live, or cache per-item line offsets so `visible` can seek.
- [ ] Shared picker pieces: `llm_picker/view.rs` and `session_picker/view.rs` duplicate the header, the keys-fit check and the Loading/Failed rows and their `State` enums.
- [ ] Per-path edit lock in nth-tools: formatters run under the global `EDITS` lock, so a cold prettier serialises every parallel edit of every file. A `Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>` keeps the race fix and the parallelism.
- [ ] Bounded bash output: bash buffers the whole stdout and tails at the end; keep a ring of `max_output_chars` while streaming. Same note for read and grep, which read whole files before counting lines.
- [ ] One timeout parameter convention across tools: `timeout` is ms in bash, seconds in webfetch, and `timeout_ms` in monitor next to camelCase `filePath` and `numResults`.
- [ ] Decide on an SSRF guard for webfetch (`169.254.169.254`, `localhost:<port>`); opencode has the same gap.
- [ ] Monitor flood guard counts stdout only, so a stderr flood still sends one event per line to the front-end and the log.

## Deliberately skipped

These are non-goals in [design.md](design.md):

- MCP client (`mcp/`), plugins (`plugin/`, `packages/plugin`)
- Client/server split, web, desktop, IDE and ACP (`server/`, `packages/app`, `packages/desktop`, `acp/`, `ide/`)
- Session sharing, accounts and orgs (`share/`, `account/`, `dialog-console-org.tsx`)
- GitHub app and PR commands (`cli/cmd/github.ts`, `cli/cmd/pr.ts`)
- Themes (nth uses the 16 ANSI colors), self-upgrade and uninstall
