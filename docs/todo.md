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

- [ ] Environment block: add today's date and the workspace root. `session/system.ts:80`
- [ ] Per-model system prompts: only when a model misbehaves (`kimi.txt`, `gpt.txt`, `gemini.txt`). `session/prompt/`

### Subagents and orchestrations

- [ ] Subagent
- [ ] Subagent tabs
- [ ] Orchestrations view

### Worktrees

An opinionated worktree flow baked into nth requires design

### Scratchpad

A more flexible todo that uses the tab view

### Mouse control

Allow mouse control

- [ ] Switching tabs
- [ ] Clicking to copy

### TUI

- [ ] Shell mode: `!` at the start of the prompt runs the line as a shell command and adds its output to the chat. `tui/component/prompt/index.tsx:836`
- [ ] Paste summary: collapse a large paste to `[pasted N lines]`. `tui/` `app.toggle.paste_summary`
- [ ] Tool details toggle: expand a collapsed tool call in place. `tui/` `session.toggle.actions`
- [ ] Thinking toggle: show or hide reasoning. `tui/` `session.toggle.thinking`

## Deliberately skipped

These are non-goals in [design.md](design.md):

- MCP client (`mcp/`), plugins (`plugin/`, `packages/plugin`)
- Client/server split, web, desktop, IDE and ACP (`server/`, `packages/app`, `packages/desktop`, `acp/`, `ide/`)
- Session sharing, accounts and orgs (`share/`, `account/`, `dialog-console-org.tsx`)
- GitHub app and PR commands (`cli/cmd/github.ts`, `cli/cmd/pr.ts`)
- Themes (nth uses the 16 ANSI colors), self-upgrade and uninstall
