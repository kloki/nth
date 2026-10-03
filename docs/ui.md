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
                                                  ↓ 12 more · ctrl+End ┘
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

The chat tab is the transcript, which scrolls. Each tool call keeps the output it produced right under its row.

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

- **Tool icon.** Each tool has its own icon, so calls are told apart at a glance: `≡` read, `✎` write, `$` bash, and `•` for any other. There is no success mark: the icon is dim while the call runs and cyan once it is done. A failed call turns its icon and name red and shows the error's first line.
- **Tool summary.** read and write show the path relative to the working directory. bash shows the command itself, not the model's description of it. A multi-line command shows its first line followed by `…`.
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
| Question (later)   | per question | the agent asks        | answer, esc   |
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

## Status bar

Fixed at 2 lines, always visible, below the input panel. It holds general state, never anything you interact with. Line 1 is where you are, line 2 a hint about the chat. The right side of a line is cut first when it is too narrow.

```
 glm-5.3 · ~/repos/nth ⣿⣿⣿⣷⡀⠀⠀⠀⠀⠀⠀⠀⠀          git · fix-auth +3 *4 󰊐 2
                                                   ↓ 12 more · ctrl+End
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

**Line 2: hints**

"↓ N more · ctrl+End" against the right edge when there is more chat below the view; empty otherwise. How to cancel a running turn is on the input panel, not here.

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
- May an input panel grow with its content, such as a long question, or is a fixed height per panel strict?
