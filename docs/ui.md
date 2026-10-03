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
▎                                                                     ┐
▎ BUILD  > Ask anything.                                              │ input, 3 rows
▎                                                                     ┘
 edit  src/client.rs       glm-5.3 ~/repos/nth fix-auth ⣿⣿⣿⣷⡀⠀⠀⠀⠀⠀⠀⠀⠀ ┐ status, 2 lines
 esc to interrupt                                        +3 *4 󰊐 2 ┘
```

There are no borders or divider lines, as the styleguide in [design.md](design.md#tui) says. Bands are told apart by background colour and spacing.

## Sizing

Heights are decided bottom-up, in lines:

1. **Status bar**: fixed at 2 lines.
2. **Input panel**: the active input panel's own height. Each input panel declares a fixed number of lines; the prompt is 3, one line of text with a padding row above and below.
3. **Content panel**: everything left over.

Swapping input panels therefore resizes the content panel. The content panel keeps its bottom anchored, so a chat scrolled to the end stays at the end. On a terminal too short for all three, the content panel shrinks to zero first and the status bar is the last thing to go.

## Content panel

- **Default: chat history.** The transcript, scrolled, with the banner on top as today.
- **Tabs, later.** The content panel holds a list of tabs, and chat is always the first. Examples are Plan (the plan file with its comment threads), Diff and Monitor. These replace the side pane and agents sidebar sketched in design.md.
- **Tab strip.** One line at the top of the panel, shown only when more than one tab is open, so a plain chat session looks exactly like today.
- **Read and navigate only.** Content tabs scroll and select, but text entry always goes through the input panel.
- **Independent of the input panel.** Switching tabs never changes the input panel, and the other way round.

## Chat

The chat tab has two parts: the transcript, which scrolls, and the live tool section, pinned under it while tools run.

```
▎ add retry to the fetch client                          ┐
                                                         │
  ∴ thought · 2.1s                                       │
  ✓ read   src/client.rs                                 │ transcript
  ✓ bash   cargo test -p nth-llm                         │
                                                         │
▎ Added exponential backoff with jitter …                │
                                                         │
  ∎ glm-5.3 · 2 tool calls · 14.2s                       ┘
                                                         
▎ ▸ bash   cargo test -p nth-llm                         ┐
▎    Compiling nth-llm v0.1.0                            │ live tools
▎    Running unittests src/lib.rs                        │
▎ test sse::parses_go_stream ... ok                      ┘
```

**Transcript**

| Entry | Bar | Shape |
| --- | --- | --- |
| Your message | green | Wrapped text under the bar |
| Model answer | grey (bright black) | Wrapped text under the bar |
| Thinking | none | `∴ thinking · 1.2s`, dim, one line |
| Tool call | none | Marker, name in cyan, summary in dim; one line |
| Turn summary | none | `∎ model · N tool calls · 14.2s`, dim, after a blank line |
| Interrupted | none | `⏹ interrupted · 3.0s` in yellow, after a blank line |
| Error | red | `✗ message` in red |

- **Tool summary.** read and write show the path relative to the working directory. bash shows the command itself, not the model's description of it. A multi-line command shows its first line followed by `…`.
- **Turn summary.** `∎` closes the turn, as `∴` opens its thinking, and stays dim. The check mark is kept for tool calls, where it means success. The blank line above separates the summary from the last entry of the turn.

**Live tool section**

A section at the bottom of the chat tab shows each tool call that is still running, with its content streaming in. When a call finishes, its block disappears and the call stays in the transcript as its usual one-line row.

- **Placement.** Pinned under the transcript, above the input panel, with one blank row above it. The transcript area shrinks to make room, and keeps its bottom anchored.
- **Bar.** Yellow, down every row of the block, so it reads as one thing that is still moving.
- **Header.** `▸ name  summary`, the same as the transcript row, so the block visibly turns into that row when it finishes.
- **Body.** At most 10 lines, streamed:

| Tool | Body |
| --- | --- |
| read | The file content it read, the first 10 lines |
| write | The content being written, the first 10 lines, taken from the call's arguments |
| bash | The command's output, stdout and stderr interleaved, the last 10 lines |

- **Parallel calls.** The model can start several tool calls at once, and they run together. Each running call gets its own block, stacked in the order they started.
- **Height cap.** The section takes at most half the content panel. When the blocks do not fit, the oldest blocks shrink to their header row first.
- **Scrolling.** The section is not part of the transcript scroll. Scrolling up through history leaves it in place.

**What the session needs to send**

Today the TUI only hears `ToolStarted` and `ToolFinished`. write needs nothing new: its content is in the call's arguments, which `ToolStarted` already carries. read and bash need one more event, tool output as it arrives, keyed by call id. It is generic rather than bash-specific, so the future monitor tool streams through the same event.

## Input panel

- **Default: the prompt.** See [Prompt](#prompt) below. The completion popup for `/` commands and `@` files floats over the content panel, anchored to the prompt.
- **Context swaps it.** Today that is the model picker. Later come question tool answers, permission prompts, the session list and similar. Each is its own input panel.
- **Each input panel declares its height in lines.** The prompt is 3; the model picker is a header plus a list, around 8. The height is fixed while the panel is open, so typing or filtering never makes the layout jump.
- **One input panel at a time.** Opening one replaces the prompt; finishing or `esc` returns to the prompt. The prompt keeps its text while hidden.
- **Keys go to the input panel first.** It handles what it knows and passes the rest on to app-level keys: content scrolling, tab switching and quit.

| Input panel        | Height       | Opens on              | Returns on    |
| ------------------ | ------------ | --------------------- | ------------- |
| Prompt             | 3            | default               | —             |
| Model picker       | ~8           | `/model`              | enter, esc    |
| Question (later)   | per question | the agent asks        | answer, esc   |
| Permission (later) | ~4           | a tool needs approval | allow, reject |

## Prompt

Modelled on opencode's prompt: a coloured bar down the left and a lighter background, so the input stands out from the content without a border.

```
▎
▎ BUILD  > add retry to the fetch client█
▎
```

- **Shape.** 3 rows on a bright black background: a padding row, the text row, a padding row. The bar `▎` runs down the left edge of all three rows.
- **Mode label.** The text row starts with the mode, then two spaces and `>`, then the text. For now the only mode is BUILD; PLAN and other modes come later.
- **Mode colour.** The bar and the mode label share one colour per mode: BUILD is blue, and later PLAN is magenta. The typed text is the default fg.
- **Placeholder.** "Ask anything." in dim when the prompt is empty.
- **Text row.** One line. Long or multi-line input scrolls inside it, following the cursor, as today.

**While a turn runs**

The mode label is replaced by a braille spinner in the same mode colour. Its frames are the `waverows` spinner, copied into nth: 16 frames, 4 characters wide, at 80 ms a frame.

```
⠖⠉⠉⠑ ⡠⠖⠉⠉ ⣠⡠⠖⠉ ⣄⣠⡠⠖ ⠢⣄⣠⡠ ⠙⠢⣄⣠ ⠉⠙⠢⣄ ⠊⠉⠙⠢ ⠜⠊⠉⠙ ⡤⠜⠊⠉ ⣀⡤⠜⠊ ⢤⣀⡤⠜ ⠣⢤⣀⡤ ⠑⠣⢤⣀ ⠉⠑⠣⢤ ⠋⠉⠑⠣
```

```
▎
▎ ⣄⣠⡠⠖   > add retry to the fetch client
▎
```

The spinner is padded to the width of the mode label, so `>` and the text never move when it starts or stops. The bar keeps the mode colour, and the text is dimmed while Enter cannot submit, as today. When the turn ends, the mode label comes back.

The spinner runs for the whole turn: thinking, writing and tool calls. What exactly the turn is doing is on status line 1.

## Status bar

Fixed at 2 lines, always visible, below the input panel. It holds general state, never anything you interact with. The state is right-aligned; the left is reserved for what the running turn is doing.

```
 edit  src/client.rs           glm-5.3 ~/repos/nth fix-auth ⣿⣿⣿⣷⡀⠀⠀⠀⠀⠀⠀⠀⠀
 esc to interrupt                                        +3 *4 󰊐 2
```

**Line 1: where you are**

Right-aligned, separated by single spaces: `model path branch context`.

Colours here are the terminal's ANSI colours, so your terminal theme decides how they look. Purple in the starship config is ANSI magenta. See [Colours](#colours) for how they look in Dracula.

| Part | Shows | Colour |
| --- | --- | --- |
| Model | The current model | blue |
| Path | The working directory, with home written as `~` | red |
| Branch | The current git branch; hidden outside a git repo | green |
| Context | Context used as a [braille bar](https://github.com/kloki/braille-bar), 13 characters wide, scaled to the model's context window | white |

The context bar is empty until the first turn reports usage. When the model's context window is unknown, the bar is hidden.

**Line 2: git status**

Right-aligned, a reimplementation of this starship config. Each part shows only when its count is non-zero, and the whole line is empty when the tree is clean or outside a git repo.

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

**Left side: the running turn**

| Line | Left |
| --- | --- |
| 1 | What the running turn is doing: thinking, writing, or the tool and its summary |
| 2 | The one hint that matters now, such as "↓ N more · ctrl+End" or "esc to interrupt" |

Both are empty while idle. When a line is too narrow for both sides, the left side is cut first.

## Colours

Every colour is one of the 16 ANSI colours, so the terminal theme decides how it looks. The reference theme is Dracula, which maps them like this:

| Name in this doc | ANSI | Dracula | Used for |
| --- | --- | --- | --- |
| red | red | `#FF5555` | path, deleted, conflicted, errors |
| green | green | `#50FA7B` | branch, your messages, success |
| yellow, orange | yellow | `#F1FA8C` | live tool bar, ahead, behind, renamed, interrupted |
| blue | blue | `#BD93F9` (Dracula purple) | model, BUILD, staged |
| magenta, purple | magenta | `#FF79C6` (Dracula pink) | PLAN, modified |
| cyan | cyan | `#8BE9FD` | tool names |
| white | white | `#F8F8F2` | context bar, untracked, stashed |
| grey | bright black | `#6272A4` (Dracula comment) | model answer bar, prompt background |

Note that in Dracula ANSI blue renders as purple and ANSI magenta as pink. Dracula's orange (`#FFB86C`) is not one of its ANSI colours, so orange in this doc means ANSI yellow.

## Open questions

- Which key switches content tabs: `ctrl+x <n>` with the leader, as planned, or a Tab-style cycle?
- Does the chat banner (`nth · model · place`) stay, now that status line 2 shows the same?
- May an input panel grow with its content, such as a long question, or is a fixed height per panel strict?
