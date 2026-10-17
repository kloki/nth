# Browser debugging and testing

|                |                                                                     |
| -------------- | ------------------------------------------------------------------- |
| **Researched** | 2026-10-17                                                          |
| **Question**   | How Claude Code drives Chrome, and how nth gets the same without MCP |
| **Decision**   | The internal `browser` skill over the agent-browser CLI             |

## Summary

Claude Code's browser is a Chrome extension talking to the CLI, exposed to
the model as an MCP server and taught by a built-in skill. nth rules MCP
out ("integrations are skills that call REST APIs or CLIs through bash",
design.md), so its browser is a CLI the model runs through bash, taught by
an internal skill. Three CLIs do the job the same way (open a page, take an
accessibility-tree snapshot with element refs, act by ref, read console and
network, run JavaScript, screenshot); agent-browser is the one picked.

## How Claude Code does it

Three parts, per the [Chrome page](https://code.claude.com/docs/en/chrome)
of its docs and the `chrome-browser` skill it ships:

- **The extension.** _Claude in Chrome_ runs in the user's real Chrome with
  their sign-ins. Claude Code reaches it through a native messaging host
  (`~/.config/google-chrome/NativeMessagingHosts/com.anthropic.claude_code_browser_extension.json`)
  and Anthropic's bridge, and it only works signed in to claude.ai, so a
  third-party harness cannot reuse it. `claude --chrome` or `/chrome` turns
  it on; the tabs it opens go in a tab group tied to the session.
- **The tools.** An MCP server named `claude-in-chrome`: `tabs_context_mcp`,
  `tabs_create_mcp`, `tabs_close_mcp`, `navigate`, `computer`, `read_page`,
  `form_input`, `read_console_messages`, `read_network_requests`,
  `javascript_tool`, `gif_creator`, `screenshot` (with `save_to_disk`),
  `upload`. They are deferred and loaded through ToolSearch when needed,
  since always loading them costs context.
- **The skill.** Guidance only: load every tool you expect to need in one
  ToolSearch call, read the tab context first and work in a new tab, respect
  per-site permissions and let the user handle logins and CAPTCHAs, filter
  the console with a pattern, never trigger `alert`/`confirm`/`prompt`
  (they block the extension), and stop after two or three failed attempts.

The capability is the extension; the skill makes the model use it well.

## The CLIs

| CLI                                                                              | Runtime                                                                        | The user's own Chrome                                                                                   | Skill it ships                    |
| -------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------- | --------------------------------- |
| [agent-browser](https://github.com/vercel-labs/agent-browser) (Vercel Labs)      | Rust binary, `cargo install agent-browser`; a daemon per session, 1 h idle     | `--cdp <port>` or `--auto-connect` to a Chrome with remote debugging on; `--profile <path>` keeps one     | `--help` per command (0.19); `skills get core` on main |
| [playwright-cli](https://github.com/microsoft/playwright-cli) (Microsoft)        | Node 18+, `npm i -g @playwright/cli`                                           | `attach --extension=chrome` through the Playwright MCP Bridge extension; `attach --cdp`                 | `playwright-cli install --skills` |
| [chrome-devtools-mcp](https://github.com/ChromeDevTools/chrome-devtools-mcp) (Google) | Node, MCP only                                                                 | launches or attaches                                                                                    | none                              |

All three take the same shape. agent-browser's `snapshot -i` is the
compact form (interactive elements only, `@e3` refs), and it has `console`,
`errors`, `network requests`, `eval`, `wait` on text/url/function,
`dialog`, `screenshot --annotate`, state, frames, routes and tracing. It is
a binary-only crate (no library target), so nth cannot link it in: it is
installed once per machine, and it finds an installed Chrome or Chromium
itself (`agent-browser install` downloads Chrome for Testing only when
there is none). The crates.io release (0.19.0 on 2026-10-17) is behind the
repository: `doctor` and `skills get core` from the README are not in it,
so the skill points at `--help`. On Ubuntu, a Chrome it launches can die
with "No usable sandbox" (the AppArmor rule against unprivileged user
namespaces); `--args "--no-sandbox"` gets past it for local work.

## What nth does

- The internal `browser` skill
  (`crates/nth-context/src/skills/internal/browser/SKILL.md`) teaches the
  workflow and nth's own rules, and points at `agent-browser --help` for
  the full reference, so it does not go stale with the CLI's releases. It
  has the model check for the binary and, with the user's okay, install it
  through the monitor tool.
- `process::shell` sets `AGENT_BROWSER_SESSION=nth-<session id>` on every
  bash and monitor command, so a session has one browser, the user's `!`
  commands reach it, and parallel sessions and subagents never share one.
- The diagnostics tab's tools section says whether `agent-browser` is on
  PATH, beside `rtk`.

## Not done

- **Images to the model.** Tool results are text (`ToolResult` in
  nth-protocol), so the model cannot see a screenshot; it reads the page
  from the snapshot and the DOM, and screenshots are files for the user.
  Carrying images would touch the tool result and all three wire protocols.
- **The real Chrome by default.** Claude Code works in the user's signed-in
  browser; nth's default is a headless one, with `--headed`, `--cdp`,
  `--auto-connect` and `--profile` when the user wants their own.
