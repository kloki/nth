# nth — UI layout

The screen is three bands stacked top to bottom. Each band has one job, and none of them knows how the others draw.

| Band          | Job                    | Default                                 |
| ------------- | ---------------------- | --------------------------------------- |
| Content panel | What you look at       | Chat history; later one of several tabs |
| Input panel   | What you type into     | The prompt                              |
| Status bar    | What is true right now | Always 2 lines                          |

```
 nth · glm-5.3 · ~/repos/nth                                          ┐
 ▎ you  add retry to the fetch client                                 │ content
 ▎ read  src/client.rs                                                │
 ▎ Added exponential backoff with jitter …                            ┘
▎ build                                                               ┐
▎ Ask anything.                                                       │ input, 4 rows
▎                                                                     │
▎                                                                     ┘
 glm-5.3 · ~/repos/nth ⣿⣿⣿⣷⡀⠀⠀⠀⠀⠀⠀⠀⠀      git · fix-auth +3 *4 󰊐 2 ┐ status, 2 lines
                                                                       ┘
```

There are no borders or divider lines, as the styleguide in [design.md](design.md#tui) says. Bands are told apart by coloured bars and spacing.

## Sizing

Heights are decided bottom-up, in lines:

1. **Status bar**: fixed at 2 lines.
2. **Input panel**: the active input panel's own height. Each input panel declares a fixed number of lines; the prompt is 4, a label row and three rows of text.
3. **Content panel**: everything left over.

Swapping input panels therefore resizes the content panel. The content panel keeps its bottom anchored, so a chat scrolled to the end stays at the end. On a terminal too short for all three, the content panel shrinks to zero first and the status bar is the last thing to go.

## Content panel

- **Default: chat history.** The transcript, scrolled, with the banner on top as today.
- **Tabs, later.** The content panel holds a list of tabs, and chat is always the first. Examples are Plan (the plan file with its comment threads), Diff and Monitor. These replace the side pane and agents sidebar sketched in design.md.
- **Tab strip.** One line at the top of the panel, shown only when more than one tab is open, so a plain chat session looks exactly like today.
- **Read and navigate only.** Content tabs scroll and select, but text entry always goes through the input panel.
- **Independent of the input panel.** Switching tabs never changes the input panel, and the other way round.

## Chat

The chat tab is the transcript, which scrolls. While scrolled up, a grey scrollbar thumb in the right margin shows where the view is; it goes away once you are back at the bottom and following again. Each tool call keeps the output it produced right under its row.

```
▎ add retry to the fetch client

  ∴ thought · 2.1s
  ≡ read   src/client.rs
▎ use std::time::Duration;
▎ pub struct Client {
  $ bash   cargo test -p nth-llm
▎    Compiling nth-llm v0.1.0
▎ test sse::parses_go_stream ... ok

▎ Added exponential backoff with jitter …

  ∎ glm-5.3 · 2 tool calls · 14.2s
```

**Transcript**

| Entry | Bar | Shape |
| --- | --- | --- |
| Your message | green | Wrapped text under the bar |
| Model answer | cyan | Wrapped text under the bar |
| Thinking | none | `∴ thinking · 1.2s`, dim, one line |
| Tool call | none | The tool's icon, name in cyan, summary in dim; one line |
| Turn summary | none | `∎ model · N tool calls · 14.2s`, dim, after a blank line |
| Interrupted | none | `⏹ interrupted · 3.0s` in yellow, after a blank line |
| Error | red | `✗ message` in red |

- **Tool icon.** Each tool has its own icon, so calls are told apart at a glance: `≡` read, `✎` write, `±` edit, `Δ` apply_patch, `$` bash, `*` glob, `/` grep, `↓` webfetch, `?` websearch, `✦` skill, `¿` question, and `•` for any other. There is no success mark: the icon is dim while the call runs and cyan once it is done. A failed call turns its icon and name red and shows the error's first line.
- **Tool summary.** read and write show the path relative to the working directory. bash shows the command itself, not the model's description of it. skill shows the skill's name. question shows the questions' headers. A multi-line command shows its first line followed by `…`.
- **Turn summary.** `∎` closes the turn, as `∴` opens its thinking, and stays dim. The blank line above separates the summary from the last entry of the turn.

**Tool output**

Each tool call shows its output under its row as it streams in, and keeps it once the call finishes, so output that scrolls past too fast to read can be read back.

- **Bar.** Bright white, at the left edge like every other bar, so the output lines up with the messages around it.
- **Body.** At most 10 lines:

| Tool | Body |
| --- | --- |
| read | The file content it read, the first 10 lines |
| write | The content being written, the first 10 lines, taken from the call's arguments |
| bash | The command's output, stdout and stderr interleaved, the last 10 lines |
| skill | None: the row says which skill was loaded, and its body is for the model only |
| question | Your answers, one line per question |

- **Parallel calls.** The model can start several tool calls at once, and they run together. Each call's output stays under its own row, in the order they started.

**What the session needs to send**

Today the TUI only hears `ToolStarted` and `ToolFinished`. write needs nothing new: its content is in the call's arguments, which `ToolStarted` already carries. read and bash need one more event, tool output as it arrives, keyed by call id. It is generic rather than bash-specific, so the future monitor tool streams through the same event.

## Input panel

- **Default: the prompt.** See [Prompt](#prompt) below. The completion popup for `/` commands and `@` files floats right above the row being typed, lined up with the `/` or `@` it completes.
- **One style.** Every input panel looks the same; see [Input panel style](#input-panel-style).
- **Context swaps it.** Today that is the model picker. Later come question tool answers, permission prompts, the session list and similar. Each is its own input panel.
- **Each input panel declares its height in lines.** The prompt is 4; the model picker is a header plus a list, around 8. The height is fixed while the panel is open, so typing or filtering never makes the layout jump.
- **One input panel at a time.** Opening one replaces the prompt; finishing or `esc` returns to the prompt. The prompt keeps its text while hidden.
- **Keys go to the input panel first.** It handles what it knows and passes the rest on to app-level keys: content scrolling, tab switching and quit.

| Input panel        | Height       | Opens on              | Returns on    |
| ------------------ | ------------ | --------------------- | ------------- |
| Prompt             | 4            | default               | —             |
| Model picker       | ~8           | `/model`              | enter, esc    |
| Question           | per call     | the agent asks        | answer, esc   |
| Permission (later) | ~4           | a tool needs approval | allow, reject |

## Input panel style

Every input panel has the same shape, so a new one reads as the same kind of thing as the prompt. In code this is `theme::panel_title` and `theme::panel_row` in `crates/nth-tui/src/theme.rs`; build a new panel from those.

```
▎ switch model                         ↑↓ model · ←→ effort · enter · esc
▎ → glm   ✓ GLM 5.3  128k
▎   plain   Plain    32k
```

- **No border, default background.** The panel stands out by its bar, not by a box or a fill.
- **One accent colour.** The bar `▎` runs down every row in it, and the top row holds the panel's title or label in it too. The title is plain, not bold, so the content stays the loudest thing.
- **Content under the title.** Each row starts after the bar. A highlighted item is bold magenta (`theme::pick`), the same as in the completion popup.

| Panel | Accent | Title |
| --- | --- | --- |
| Prompt | the mode's colour: blue for build | the mode label, or the spinner |
| Model picker | magenta | `switch model` |
| Question | cyan | `question`, or a tab per question |

## Prompt

Modelled on opencode's prompt, in the [input panel style](#input-panel-style).

```
▎ build
▎ add retry to the fetch client, and
▎ back off with jitter█
▎
```

- **Shape.** 4 rows: the mode label, then three rows of text. Text wraps at the full width and scrolls to keep the cursor in view.
- **Mode label.** The top row shows the mode in lower case. For now the only mode is build; plan and other modes come later.
- **Mode colour.** The bar and the mode label share one colour per mode: build is blue. The typed text is the default fg.
- **Placeholder.** "Ask anything." in dim when the prompt is empty.
- **Completion popup.** Sits right above the cursor's row, lined up with the `/` or `@` it completes, and moves left when it would run off the right edge.
- **Skills as commands.** `/` lists nth's commands first, then every skill, at most 8 rows; typing narrows them. A skill's row shows the first line of its description. Ctrl+N or Enter fills in `/name ` for the arguments, and Enter on a fully typed `/name [args]` runs it. The chat shows the command as typed; the model gets the skill's body with `$1`…`$N` and `$ARGUMENTS` filled in, `` !`cmd` `` replaced by the command's output and `@path` files attached. A skill named like a command is hidden behind the command.
- **History.** Up and Down recall sent prompts, newest first, and Down past the newest gives back what was being typed. An edited recalled prompt is never replaced: Up and Down do nothing until it is sent or cleared. The last 100 prompts are kept across runs in `$XDG_DATA_HOME/nth/prompt-history.jsonl`, one JSON string per line. Builtin commands are not recorded.

**While a turn runs**

The mode label is replaced by a braille spinner in the same mode colour. Its frames are the `waverows` spinner, copied into nth: 16 frames, 4 characters wide, at 80 ms a frame.

```
⠖⠉⠉⠑ ⡠⠖⠉⠉ ⣠⡠⠖⠉ ⣄⣠⡠⠖ ⠢⣄⣠⡠ ⠙⠢⣄⣠ ⠉⠙⠢⣄ ⠊⠉⠙⠢ ⠜⠊⠉⠙ ⡤⠜⠊⠉ ⣀⡤⠜⠊ ⢤⣀⡤⠜ ⠣⢤⣀⡤ ⠑⠣⢤⣀ ⠉⠑⠣⢤ ⠋⠉⠑⠣
```

```
▎ ⣄⣠⡠⠖                                                  esc to cancel
▎ add retry to the fetch client
▎
▎
```

The bar keeps the mode colour, and the text is dimmed while Enter cannot submit. A dim "esc to cancel" sits against the right edge of the label row. When the turn ends, the mode label comes back and the hint goes.

The spinner runs for the whole turn: thinking, writing and tool calls. What exactly the turn is doing shows in the chat.

## Question

The question tool lets the model stop mid-turn and ask you something, as Claude Code's AskUserQuestion does. One call asks 1 to 4 questions. Each has 2 to 4 options, picks one or any number of them, and can give every option a one-line description. Every question also gets an open field for your own answer, so the model never adds an "Other" option itself.

The panel is cyan, the model answer's colour, because this is the model talking to you.

**One question, one choice**

```
▎ question                                   ↑↓ · 1-4 · enter · esc
▎ Which auth method should the client use?
▎ → 1. OAuth (Recommended)   Standard, works with SSO
▎   2. API key               Simplest; one secret per user
▎   3. mTLS                  Strongest, needs client certs
▎   4. Type your own answer…
```

- **Title row.** `question` in cyan, the keys dim against the right edge, dropped when the row is too narrow, as in the model picker.
- **Question.** Default fg, wrapped, at most 3 rows.
- **Options.** A number, then the label in blue, as model ids are in the model picker. The highlighted one has `→` and is bold magenta (`theme::pick`). The description is dim, in one column after the longest label, and is cut with `…` when it does not fit.
- **Open field.** Always the last row. Highlighting it and typing writes straight into it, with no separate edit mode. "Type your own answer…" is the dim placeholder.
- **Answering.** Enter on an option, or on an open field with text, answers. A lone one-choice question is sent right away.

**Any number of choices**

```
▎ question                              ↑↓ · space toggle · enter · esc
▎ Which checks should run before commit?
▎ → [x] 1. fmt        cargo fmt --check
▎   [x] 2. clippy     -D warnings
▎   [ ] 3. test       the whole workspace
▎   [ ] 4. Type your own answer…
```

`[x]` and `[ ]` sit in front of the number. Space or the number toggles an option, the open field counts as ticked once it has text, and Enter moves on.

**Several questions**

```
▎ ☒ Auth   ☐ Checks   ✓ Submit                ←→ question · ↑↓ · enter · esc
▎ Which checks should run before commit?
▎ → [x] 1. fmt        cargo fmt --check
```

- **Tab row.** Replaces the title: each question's short header, `☒` once it is answered and `☐` before, the current one bold magenta. Tab and ←→ move between them, and answering one moves to the next.
- **Submit.** The last tab reviews every answer before they go. Enter sends them; an unanswered question shows in yellow, and Enter waits until there are none.

```
▎ ☒ Auth   ☒ Checks   ✓ Submit                          enter send · esc
▎ Auth    OAuth (Recommended)
▎ Checks  fmt, clippy, "and a doc check"
```

**Height.** Set once, when the panel opens: the title row plus the tallest question with its options and open field, at most half the terminal; past that the options scroll. It stays fixed while open, like every input panel, so moving between questions never makes the layout jump.

**In the chat.** The call's row is `? question  Auth, Checks`, with the dim icon while you answer. Once answered, its body lists the answers as the Submit tab does.

**Esc.** Declines: the panel goes, the prompt comes back with its text, and the model reads that you declined and carries on. Esc at the prompt cancels the turn, as it always does.

## Status bar

Fixed at 2 lines, always visible, below the input panel. It holds general state, never anything you interact with. Line 1 is where you are; line 2 is empty for now. The right side of a line is cut first when it is too narrow.

```
 glm-5.3 · ~/repos/nth ⣿⣿⣿⣷⡀⠀⠀⠀⠀⠀⠀⠀⠀          git · fix-auth +3 *4 󰊐 2
```

**Line 1: where you are**

Left-aligned: `model · effort · path context`, all bright white. The git branch and status sit against the right edge.

Colours here are the terminal's standard colours; see [Colours](#colours). Purple in the starship config is magenta.

| Part | Shows | Colour |
| --- | --- | --- |
| Model | The current model, and its effort unless default | bright white |
| Path | The working directory, with home written as `~` | bright white |
| Context | Context used as a [braille bar](https://github.com/kloki/braille-bar), 13 characters wide, scaled to the model's context window | white |

The context bar is empty until the first turn reports usage. When the model's context window is unknown, the bar is hidden.

**Line 1, right: git status**

Prefixed with a bright white `git · ` and the current branch in green, then a reimplementation of this starship config. Each part shows only when its count is non-zero, and the whole section is hidden outside a git repo.

```toml
[git_status]
format = '[$conflicted$ahead_behind$modified$renamed$deleted$staged$untracked$stashed]($style)'
style = "white"
ahead = ' [+$count](yellow)'
behind = ' [-$count](yellow)'
conflicted = ' [](red)'
diverged = ' 󰱮'
stashed = ' '
staged = ' [󰊐 $count](blue)'
modified = ' [*$count](purple)'
renamed = ' [ $count](yellow)'
deleted = ' [ $count](red)'
untracked = ' [ $count](white)'
```

| Part | Format | Colour |
| --- | --- | --- |
| Conflicted | `` (U+F071, warning) | red |
| Ahead | `+N` | yellow |
| Behind | `-N` | yellow |
| Diverged, in place of ahead and behind | `󰱮` (U+F0C6E) | white |
| Modified | `*N` | magenta |
| Renamed | ` N` (U+F0EC, exchange) | yellow |
| Deleted | ` N` (U+F1F8, trash) | red |
| Staged | `󰊐 N` (U+F0290) | blue |
| Untracked | ` N` (U+F128, question) | white |
| Stashed | `` (U+F187, archive) | white |

Icons are Nerd Font glyphs, as in the starship config. Conflicts are red rather than the default colour, because they block a commit and should be the first thing you notice.

The status comes from one `git status --porcelain=v2 --branch` plus a stash check. It is refreshed at start-up, after every tool call that can write, and at the end of each turn, off the async runtime.

**Line 2: empty**

Kept so nothing above moves when it gets a job. That there is more chat below the view is shown by the chat's scrollbar, and how to cancel a running turn is on the input panel.

## Colours

Every colour is one of the terminal's 16 standard colours, so the terminal theme decides how it looks. nth never sets a colour of its own.

| Name in this doc | Terminal colour | Used for |
| --- | --- | --- |
| red | red | path, deleted, conflicted, errors |
| green | green | branch, your messages, success |
| yellow, orange | yellow | ahead, behind, renamed, interrupted |
| blue | blue | model, build, staged |
| magenta, purple | magenta | model picker, highlighted items, modified |
| cyan | cyan | tool names, model answer bar |
| white | white | context bar, untracked, stashed |
| bright white | bright white | status line 1 text, tool output bar |

Orange is not a standard terminal colour, so it means yellow.

## Open questions

- Which key switches content tabs: `ctrl+x <n>` with the leader, as planned, or a Tab-style cycle?
- Does the chat banner (`nth · model · place`) stay, now that status line 2 shows the same?
