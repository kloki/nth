---
name: config
description: How to configure nth itself through ~/.config/nth/config.toml - add a provider and a model, set profiles for plan, act, subagents and the clerk, tune the context window, tool limits, extra skill folders, formatters, language servers and notifications. Load it when asked to set up, change or fix nth's own configuration, add a provider or model, or explain a config key.
---

Configure nth itself. nth's opinions are hardcoded; this file only tunes
defaults, and secrets never live in it.

User-provided focus or constraints (honor these):
$ARGUMENTS

## Where it lives

One file, for the user, not for a project:

- `$XDG_CONFIG_HOME/nth/config.toml`, else `~/.config/nth/config.toml`.
- `--config <path>` or `NTH_CONFIG` names another file instead.

There is no per-project config: the one file applies everywhere.

## How it loads

- Every key is optional. The file lists only what it changes; everything else
  keeps its default.
- Unknown keys are an error, so a typo stops nth with a message naming the key.
- Flags and environment variables win over the file: `--model` / `NTH_MODEL`
  over `model`, `--config` / `NTH_CONFIG` over the path.
- nth reads the file once at startup, so a change applies on the next launch,
  not to the session already running.
- `nth config` prints the resolved config with every default filled in, and
  says on stderr which file it used.
- `nth init` writes the commented template to the default path; `nth init
--force` replaces an existing file.
- MCP is not supported: there is no `mcp` key and no MCP client. Integrations
  are skills that call REST APIs or CLIs through bash.

## Editing rules

- Read the file first and edit in place. Keep its comments and the keys you are
  not changing; never run `nth init --force` over a config someone has edited,
  since it discards every change.
- Never put a secret in the file. A provider names the environment variable
  holding its key with `api_key_env`.
- If the file does not exist, run `nth init` first, then edit.
- After editing, run `nth config` to confirm it still parses, and `nth models`
  to see what the providers list.

## Recipes

### Add a provider

Each `[provider.<id>]` is one OpenAI-compatible chat completions endpoint, and
the id prefixes its models. nth refuses to chat without a provider whose key is
set, or ChatGPT connected.

```toml
[provider.opencode]
name = "OpenCode Go"
base_url = "https://opencode.ai/zen/go/v1"
api_key_env = "OPENCODE_API_KEY"
models = []          # empty: ask the endpoint's /models
```

`base_url` and `api_key_env` are required; `name` defaults to the id. Export the
variable (`export OPENCODE_API_KEY=...`); a provider whose variable is not set is
left out. Use `models = ["a", "b"]` to offer only those, needed for an endpoint
without `/models`. The id `chatgpt` is reserved for the built-in subscription
login (`nth connect`), so do not configure it.

### Choose the default model

```toml
model = "opencode/deepseek-v4.1-flash"
```

A model id is `provider/model`: the prefix picks the endpoint and the rest goes
on the wire; a bare id goes to the default provider. Empty means nth picks one
at random from `nth models`. `--model` / `NTH_MODEL` override it.

### Profiles for plan, act, subagents and the clerk

Profiles are presets a chat starts on and cycles through with ctrl+p; a profile
whose provider has no key is skipped.

```toml
[[profiles]]
name = "opencode go"

[profiles.plan]
model = "opencode/deepseek-v4.1-flash"
effort = "high"

[profiles.act]
model = "opencode/kimi-k3"
effort = "default"

[profiles.subagent]
model = "opencode/deepseek-v4.1-flash"
effort = "low"

[profiles.clerk]
model = "opencode/glm-5.3-flash"
effort = "minimal"
```

`name` is required. An empty plan or act model falls back to `model`; an empty
subagent model means subagents run on their parent's. `clerk` is what the
built-in `clerk` agent runs on, a cheap model for small one-off tasks such as
a commit message; an empty clerk model means the subagent one. `effort` is `default`,
`none`, `minimal`, `low`, `medium`, `high`, `xhigh` or `max`; a model gets the
nearest level it takes. With no profiles, both modes run `model` at `default`
effort. Set the starting mode with:

```toml
[mode]
default = "plan"   # or "act"; Tab switches in the chat
```

### Tune the context window

```toml
[context]
auto = true    # compact on its own when a reply fills the window
reserved = 0   # buffer below the window; 0 sizes it to the model
preserve = 0   # recent tokens kept as text; 0 sizes it to the window
```

### Tool limits

```toml
[tools.read]
default_limit = 2000      # lines when the model gives no limit
max_line_chars = 2000     # longer lines are cut
max_bytes = 51200         # output stops before this

[tools.bash]
default_timeout_ms = 120000
max_timeout_ms = 600000
max_output_chars = 30000  # longer output keeps only its tail
rtk = true                # pass commands through `rtk rewrite` when on PATH

[tools.websearch]
url = "https://mcp.exa.ai/mcp"   # Exa's MCP endpoint; EXA_API_KEY sent if set
```

### Extra skill folders

```toml
[skills]
paths = ["~/my-skills", "./team-skills"]   # ~ is home; relative is from cwd
```

Searched after the standard places; a skill there replaces an internal one of
the same name.

### Formatters

Run after a tool writes a file. The built-in table is ported from opencode's
`format/formatter.ts`; see https://opencode.ai/docs/formatters.

```toml
[format]
enabled = true

[format.prettier]
disabled = true

[format.taplo]
command = ["taplo", "format", "$FILE"]
extensions = [".toml"]
env = { RUST_LOG = "warn" }
```

A built-in name changes that formatter; any other name adds one, which needs
`command` and `extensions`. `$FILE` is the file to format. `nth formatters`
lists them and whether they apply to the working directory.

### Language servers

Their diagnostics are appended to a write result. The built-in table is ported
from opencode's `lsp/server.ts`; see https://opencode.ai/docs/lsp.

```toml
[lsp]
enabled = true
idle_secs = 600   # stop a server untouched this long; 0 keeps it for the run

[lsp.pyright]
disabled = true

[lsp.my-server]
command = ["my-server", "--stdio"]
extensions = [".foo"]
env = { RUST_LOG = "info" }
initialization = { someOption = true }
```

A server is used only when its program is on PATH. `nth lsp` lists them and the
project root each would run in.

### Chat, layout and notifications

```toml
[chat]
thinking = true       # show the model's thinking; /settings toggles per run
tool_output = true    # show tool output

[layout]
split_width = 160     # from this width the chat splits with another tab; 0 never

[notify]
enabled = true        # sent only while the terminal is unfocused
# backend = "notify-send"   # or "osascript" on macOS; the platform's by default
```

### Turn and subagent budgets

```toml
[session]
max_steps = 100        # model requests in a turn without a final answer

[task]                 # subagents started with the task tool
max_steps = 50
timeout_secs = 600     # 0 for no time limit

[work]                 # subagents on a work item
max_steps = 100
timeout_secs = 600
stall_secs = 120       # report silence this long; 0 never
```

## Reference

Every key with its default, in the order of `nth init`'s template.

| Key     | Default | Meaning                                                            |
| ------- | ------- | ------------------------------------------------------------------ |
| `model` | `""`    | The fallback model, as `provider/model`. Empty picks a random one. |

**`[provider.<id>]`** — one endpoint per id; the id prefixes its models.

| Key           | Default  | Meaning                                       |
| ------------- | -------- | --------------------------------------------- |
| `name`        | the id   | What listings call it.                        |
| `base_url`    | required | The endpoint, version included (`/v1`).       |
| `api_key_env` | required | The environment variable holding the API key. |
| `models`      | `[]`     | Ids to offer instead of asking `/models`.     |

**`[session]`**

| Key         | Default | Meaning                                          |
| ----------- | ------- | ------------------------------------------------ |
| `max_steps` | `100`   | Model requests in a turn without a final answer. |

**`[context]`**

| Key        | Default | Meaning                                                             |
| ---------- | ------- | ------------------------------------------------------------------- |
| `auto`     | `true`  | Compact on its own when a reply fills the model's window.           |
| `reserved` | `0`     | Buffer kept below the window, in tokens; 0 sizes it to the model.   |
| `preserve` | `0`     | Recent tokens a compaction keeps as text; 0 sizes it to the window. |

**`[task]`** — subagents started with the task tool.

| Key            | Default | Meaning                                  |
| -------------- | ------- | ---------------------------------------- |
| `max_steps`    | `50`    | Model requests before the task fails.    |
| `timeout_secs` | `600`   | Seconds before it fails; 0 for no limit. |

**`[work]`** — subagents on a work item.

| Key            | Default | Meaning                                      |
| -------------- | ------- | -------------------------------------------- |
| `max_steps`    | `100`   | Model requests before the item fails.        |
| `timeout_secs` | `600`   | Seconds before it times out; 0 for no limit. |
| `stall_secs`   | `120`   | Report a subagent silent this long; 0 never. |

**`[mode]`**

| Key       | Default  | Meaning                                             |
| --------- | -------- | --------------------------------------------------- |
| `default` | `"plan"` | The mode a new chat starts in; `"plan"` or `"act"`. |

**`[[profiles]]`** — plan, act, subagent and clerk presets, cycled with ctrl+p.

| Key                     | Default     | Meaning                                                                  |
| ----------------------- | ----------- | ------------------------------------------------------------------------ |
| `name`                  | required    | The profile's label.                                                     |
| `plan` / `act` `model`  | `""`        | Falls back to `model` above.                                             |
| `plan` / `act` `effort` | `"default"` | `default`, `none`, `minimal`, `low`, `medium`, `high`, `xhigh` or `max`. |
| `subagent` `model`      | `""`        | Empty runs subagents on their parent's model and effort.                 |
| `subagent` `effort`     | `"default"` | As above. An agent's own `model` still wins.                             |
| `clerk` `model`         | `""`        | The built-in clerk's model; empty runs it as the other subagents.        |
| `clerk` `effort`        | `"default"` | As above.                                                                |

**`[chat]`**

| Key           | Default | Meaning                    |
| ------------- | ------- | -------------------------- |
| `thinking`    | `true`  | Show the model's thinking. |
| `tool_output` | `true`  | Show tool output.          |

**`[layout]`**

| Key           | Default | Meaning                                                   |
| ------------- | ------- | --------------------------------------------------------- |
| `split_width` | `160`   | Split the chat from another tab from this width; 0 never. |

**`[tools.read]`**

| Key              | Default | Meaning                                       |
| ---------------- | ------- | --------------------------------------------- |
| `default_limit`  | `2000`  | Lines returned when the model gives no limit. |
| `max_line_chars` | `2000`  | Longer lines are cut to this many characters. |
| `max_bytes`      | `51200` | Output stops before this many bytes.          |

**`[tools.bash]`**

| Key                  | Default  | Meaning                                                  |
| -------------------- | -------- | -------------------------------------------------------- |
| `default_timeout_ms` | `120000` | Timeout when the model gives none.                       |
| `max_timeout_ms`     | `600000` | Cap on the timeout the model may ask for.                |
| `max_output_chars`   | `30000`  | Longer output keeps only its last this many characters.  |
| `rtk`                | `true`   | Pass commands through `rtk rewrite` when rtk is on PATH. |

**`[tools.websearch]`**

| Key   | Default                    | Meaning                                             |
| ----- | -------------------------- | --------------------------------------------------- |
| `url` | `"https://mcp.exa.ai/mcp"` | Exa's MCP endpoint; `EXA_API_KEY` is sent when set. |

**`[skills]`**

| Key     | Default | Meaning                                                         |
| ------- | ------- | --------------------------------------------------------------- |
| `paths` | `[]`    | Extra folders searched for `SKILL.md`, after the standard ones. |

**`[format]`** — run a file's formatters after a tool writes it.

| Key       | Default | Meaning                     |
| --------- | ------- | --------------------------- |
| `enabled` | `true`  | `false` turns them all off. |

**`[format.<name>]`** — a built-in name changes that formatter; any other adds
one, which needs `command` and `extensions`.

| Key          | Default    | Meaning                                     |
| ------------ | ---------- | ------------------------------------------- |
| `disabled`   | `false`    | `true` turns this formatter off.            |
| `command`    | built-in's | Program and arguments; `$FILE` is the file. |
| `extensions` | built-in's | The files it formats, with the dot.         |
| `env`        | none       | Extra environment variables.                |

**`[lsp]`** — check a file with its language servers after a tool reads or
writes it.

| Key         | Default | Meaning                                                    |
| ----------- | ------- | ---------------------------------------------------------- |
| `enabled`   | `true`  | `false` turns them all off.                                |
| `idle_secs` | `600`   | Stop a server untouched this long; 0 keeps it for the run. |

**`[lsp.<id>]`** — a built-in id changes that server; any other adds one, which
needs `command` and `extensions`.

| Key              | Default    | Meaning                            |
| ---------------- | ---------- | ---------------------------------- |
| `disabled`       | `false`    | `true` turns this server off.      |
| `command`        | built-in's | Program and arguments.             |
| `extensions`     | built-in's | The files it checks, with the dot. |
| `env`            | none       | Extra environment variables.       |
| `initialization` | none       | Sent as `initializationOptions`.   |

**`[notify]`**

| Key       | Default        | Meaning                                              |
| --------- | -------------- | ---------------------------------------------------- |
| `enabled` | `true`         | Send desktop notifications when the model needs you. |
| `backend` | the platform's | `"notify-send"` (Linux) or `"osascript"` (macOS).    |
