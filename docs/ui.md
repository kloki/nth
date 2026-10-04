# nth — UI layout

The screen is three bands stacked top to bottom. Each band has one job, and none of them knows how the others draw.

| Band          | Job                    | Default                                 |
| ------------- | ---------------------- | --------------------------------------- |
| Content panel | What you look at       | Chat history, or another open tab       |
| Input panel   | What you type into     | The prompt                              |
| Status bar    | What is true right now | Always 2 lines                          |

```
 1 chat  2 diagnostics                                     nth 0.2.0  header
 nth · glm-5.3 · ~/repos/nth                                          ┐
 ▎ you  add retry to the fetch client                                 │ content
 ▎ read  src/client.rs                                                │
 ▎ Added exponential backoff with jitter …                            ┘
▎ plan                                                                ┐
▎ Ask anything.                                                       │ input, 4 rows
▎                                                                     │
▎                                                                     ┘
 glm-5.3 · ~/repos/nth ⣿⣿⣿⣷⡀⠀⠀⠀⠀⠀⠀⠀⠀      git · fix-auth +3 *4 󰊐 2 ┐ status, 2 lines
 ● rust  ● typescript  rustfmt · prettier                              ┘
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
- **Tabs.** The content panel holds a list of tabs, and chat is always the first and can't be closed. Diagnostics, Plan and a tab per monitor are the others so far. Later come Diff and comment threads on the plan; they replace the side pane and agents sidebar sketched in design.md.
- **Tab strip.** On the left of the header, always shown: `1 chat  2 diagnostics`, numbered in the order the tabs were opened. The showing tab is bold magenta (`theme::pick`), the others dim. `nth` and its version stay on the right.
- **Read and navigate only.** Content tabs scroll and select, but text entry always goes through the input panel. Scrolling keys and the mouse wheel move the showing tab.
- **Independent of the input panel.** Switching tabs never changes the input panel, and the other way round. The tab keys work with any input panel open.

| Key              | Does                                                  |
| ---------------- | ----------------------------------------------------- |
| ctrl+t           | Shows the next tab, from the last back to chat        |
| ctrl+1 … ctrl+4  | Shows that tab; chat is always 1                      |
| ctrl+q, `/close` | Closes the showing tab, unless it is chat, a running monitor, or the plan while there is one |
| ctrl+w           | On a monitor's tab: stops it, or closes the tab once stopped |
| ctrl+g           | Opens your editor on the plan while its tab shows, else the prompt; see [Plan](#plan) |

Ctrl with a digit only arrives as its own key in terminals that disambiguate escape codes (kitty, foot, wezterm, ghostty); elsewhere ctrl+t reaches every tab.

- **Opening.** A command opens its tab, or shows it when it is already open. The agent can switch tabs too, with the `panel` tool (`chat`, `diagnostics` or `plan`); in `nth run` there is nothing to switch, and the tool tells the model so.

## Monitors

The `monitor` tool leaves a command running; each one gets its own tab, opened without being shown so the chat keeps the focus. The label is the monitor's description behind its state: `● ci` while running, `✓ ci` after exiting 0, `✗ ci` otherwise.

```
$ tail -f deploy.log | grep --line-buffered ERROR
running · 42s · 3 events · ~/.local/share/nth/monitors/<session>/1.log

ERROR db timeout
warning: slow query        (stderr, dim)
```

The tab follows the newest line unless scrolled up, and keeps the last 2000 lines; the log has all of them.

- **Stopping.** ctrl+w stops the showing monitor; the tab stays, marked ✗, so its output can still be read. The model stops one with `monitor_stop`, and one also ends by exiting, timing out or printing too much.
- **Closing.** A monitor's tab only closes once its process has stopped. On a running one, ctrl+q and `/close` leave it open and say on the status bar to stop it first; ctrl+w again, ctrl+q or `/close` close it after.
- **Leaving.** `/clear` and `/resume` stop every monitor; their tabs close as each process stops.
- **Quitting.** With monitors running, ctrl+c on an empty prompt (or `/exit`) only warns on the status bar: `1 monitor running · ctrl+c again to quit`. The second ctrl+c stops them and saves their end notices in the session, so a resumed model knows they are gone.
- **Notices.** What a monitor says reaches the model between its steps, or starts a turn when idle; after Esc it waits for your next prompt. The chat shows each as a row: `∿ monitor 1 · ci · 2 lines`.
- **Status bar.** `∿ 2 monitors` on line 2's right while any run.

## Plan

The plan file of plan mode, `.nth/plans/<session>.md`, with what its latest change did marked in colour. The tab opens and shows the moment the model writes the first plan, and stays while the plan exists. A revision only updates the tab and its label, so the chat keeps the focus and the model's reply stays in view; a plan already there when a session opens gets its tab without being shown.

```
.nth/plans/6b2e….md · +3 -1 · ctrl+g edit · /approve

  # Retry for the fetch client
- 1. Wrap every request in a retry loop.
+ 1. Wrap idempotent requests in a retry loop.
+ 2. Back off with jitter, 2 s doubling.
  ## Verification
```

- **Label.** `plan` in the tab strip, or `plan +3 -1` while lines are marked.
- **Header.** The file, what changed, and the keys, dim.
- **Scrolling.** Like the chat: the scroll keys and the wheel move it, and a grey scrollbar thumb sits in the right margin while the plan is longer than the tab.
- **Lines.** Every line of the plan, wrapped at the tab's width. An added line is green behind `+`, a removed one red behind `-`, and an unchanged one has no mark.
- **What is marked.** The changes of the latest turn that changed the plan, against the plan as it was before that turn. A turn that only talks leaves the marks as they are, so asking a question about the plan does not wipe what its last revision did. The first plan of a session is shown unmarked too, since every line of it would be new, and so is the session's plan when it is opened by starting nth or `/resume`.
- **Reading.** The file is read after every write, edit or apply_patch, at the end of each turn, and when a session opens. A plan deleted from disk closes the tab.

**ctrl+g: commenting on the plan.** You are not expected to write the plan yourself, but you can leave comments in it. On this tab, ctrl+g opens a copy of the plan in `$VISUAL`, else `$EDITOR`, else vi, run through the shell so `code --wait` works. Write comments anywhere (`<!-- why not X? -->`, a `>>` line, `[which crate?]`), reword or delete lines, then save and quit.

- **What the model gets.** The diff from the plan to your copy, in a `<plan-edits>` element, then an instruction: these edits are review feedback, a plain change is carried into the plan, a question is answered, an objection is met or argued, each comment is removed once handled, and the plan file itself is edited, never your copy. The instruction is `crates/nth-session/src/plan/plan_edits.md`.
- **In the chat.** One row, `✎ plan edits · +2 -0`, the diff and instruction being for the model only.
- **Nothing to send.** Saving without changes says `no changes to the plan`. Quitting the editor with an error, vim's `:cq`, drops the edits. An editor that fails to start says why on the status bar.
- **While the editor runs.** nth keeps running but draws nothing: a turn goes on, monitors keep reporting, and their output shows when you come back. Edits made during a turn wait in the queue like any prompt.

**`/approve`.** Approves the plan: the mode switches to act, the chat shows, the marks clear, and the model gets opencode's approval, `The plan at <path> has been approved, you can now edit files. Execute the plan`, followed by the reminder that plan mode ended. The chat shows `/approve`. While a turn runs, the mode switches at once and the approval waits in the queue like any prompt. Without a plan file it only says `no plan to approve` on the status bar.

## Diagnostics

Opened with `/diagnostics`, or by the agent. What nth found and runs for this project, for when something does not work as expected. It scrolls like the chat.

```
model
  glm-5.3 · high · 128k context
  25 models served

language servers
  ✓ rust      ~/.cargo/bin/rust-analyzer  → ~/repos/nth
  ● ruff      ~/.local/bin/ruff  → ~/repos/nth
  ✗ gopls     not on PATH

formatters
  ✓ rustfmt   rustfmt $FILE
  ✗ prettier  no package.json here

instructions
  ~/repos/nth/CLAUDE.md

skills
  ✦ research-opencode  claude
```

- **Sections.** A bold title each, and rows under it: model, language servers, formatters, instructions, skills. Context warnings follow the skills in yellow.
- **Servers and formatters.** Every one nth knows, the ones that can run here first: a green `✓` with the name in cyan, the program and where it would run dim. A server the tools started shows its status-bar dot in place of the tick, and a broken one its reason in red. One that can't run is dim with a red `✗` and why.
- **Fresh on open.** Servers and formatters are looked up each time the tab opens, since programs may have been installed since; "checking…" shows until they are.

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

| Entry        | Bar   | Shape                                                     |
| ------------ | ----- | --------------------------------------------------------- |
| Your message | green | Wrapped text under the bar                                |
| Model answer | cyan  | Wrapped text under the bar                                |
| Thinking     | none  | `∴ thinking · 1.2s`, dim, one line                        |
| Tool call    | none  | The tool's icon, name in cyan, summary in dim; one line   |
| Turn summary | none  | `∎ model · N tool calls · 14.2s`, dim, after a blank line |
| Interrupted  | none  | `⏹ interrupted · 3.0s` in yellow, after a blank line      |
| Error        | red   | `✗ message` in red                                        |

- **Tool icon.** Each tool has its own icon, so calls are told apart at a glance: `≡` read, `>` write, `±` edit, `Δ` apply_patch, `$` bash, `*` glob, `/` grep, `↓` webfetch, `?` websearch, `✦` skill, `¿` question, `▣` panel, and `•` for any other. There is no success mark: the icon is dim while the call runs and cyan once it is done. A failed call turns its icon and name red and shows the error's first line.
- **Tool summary.** read and write show the path relative to the working directory. bash shows the command itself, not the model's description of it. skill shows the skill's name. question shows the questions' headers. A multi-line command shows its first line followed by `…`.
- **Turn summary.** `∎` closes the turn, as `∴` opens its thinking, and stays dim. The blank line above separates the summary from the last entry of the turn.

**Tool output**

Each tool call shows its output under its row as it streams in, and keeps it once the call finishes, so output that scrolls past too fast to read can be read back.

- **Bar.** Bright white, at the left edge like every other bar, so the output lines up with the messages around it.
- **Body.** At most 10 lines:

| Tool     | Body                                                                           |
| -------- | ------------------------------------------------------------------------------ |
| read     | The file content it read, the first 10 lines                                   |
| write    | The content being written, the first 10 lines, taken from the call's arguments |
| bash     | The command's output, stdout and stderr interleaved, the last 10 lines         |
| skill    | None: the row says which skill was loaded, and its body is for the model only  |
| question | None: the row says what was asked, and the answers are for the model only |

- **After a write.** Once a write, edit or apply_patch is done, what checked it shows under its row and any content: a dim note per formatter that ran (`Formatted with rustfmt.`), then for each file a language server found errors in, its path and the `ERROR [line:col] message` lines, in red. Warnings are left out, as they are for the model. At most 12 lines.

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

| Input panel        | Height   | Opens on              | Returns on    |
| ------------------ | -------- | --------------------- | ------------- |
| Prompt             | 4        | default               | —             |
| Model picker       | ~8       | `/model`              | enter, esc    |
| Question           | per call | the agent asks        | answer, esc   |
| Permission (later) | ~4       | a tool needs approval | allow, reject |

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

| Panel        | Accent                            | Title                             |
| ------------ | --------------------------------- | --------------------------------- |
| Prompt       | the mode's colour: magenta for plan, blue for act | the mode label, or the spinner |
| Model picker | magenta                           | `switch model`                    |
| Question     | cyan                              | `question`, or a tab per question |

## Prompt

Modelled on opencode's prompt, in the [input panel style](#input-panel-style).

```
▎ plan
▎ add retry to the fetch client, and
▎ back off with jitter█
▎
```

- **Shape.** 4 rows: the mode label, then three rows of text. Text wraps at the full width and scrolls to keep the cursor in view.
- **Mode label.** The top row shows the mode in lower case: `plan` or `act`. A new chat starts in plan, or in `[mode] default` from the config; a resumed session in the mode it was left in.
- **Switching modes.** Tab and shift+Tab at the prompt switch between plan and act. Each mode keeps its own model and effort, from `[mode.plan]` and `[mode.act]` in the config, and the model picker changes the current mode's. A running turn keeps its mode; the next one runs in the new one.
- **Plan mode.** The model may write only its plan file, `.nth/plans/<session>.md`; write, edit and apply_patch refuse any other path. Bash is not restricted, as in opencode, but the reminder the model gets on entering plan mode forbids changing anything with it. Switching to act tells the model so and points it at the plan file.
- **Mode colour.** The bar and the mode label share one colour per mode: plan is magenta, act is blue. The typed text is the default fg.
- **Placeholder.** "Ask anything." in dim when the prompt is empty.
- **Completion popup.** Sits right above the cursor's row, lined up with the `/` or `@` it completes, and moves left when it would run off the right edge.
- **Skills as commands.** `/` lists nth's commands first, then every skill, at most 8 rows; typing narrows them. A skill's row shows the first line of its description. Ctrl+N or Enter fills in `/name ` for the arguments, and Enter on a fully typed `/name [args]` runs it. The chat shows the command as typed; the model gets the skill's body with `$1`…`$N` and `$ARGUMENTS` filled in, `` !`cmd` `` replaced by the command's output and `@path` files attached. A skill named like a command is hidden behind the command.
- **History.** Up and Down recall sent prompts, newest first, and Down past the newest gives back what was being typed. An edited recalled prompt is never replaced: Up and Down do nothing until it is sent or cleared. The last 100 prompts are kept across runs in `$XDG_DATA_HOME/nth/prompt-history.jsonl`, one JSON string per line. Builtin commands are not recorded.
- **External editor.** ctrl+g opens the prompt in `$VISUAL`, else `$EDITOR`, else vi, as the plan does on its tab. The text you save and quit comes back into the prompt; an empty prompt opens an empty buffer. Saving with no change or quitting with an error (vim's `:cq`) leaves the prompt as it was, the latter with a hint. While the plan tab shows, ctrl+g edits the plan, not the prompt (see [Plan](#plan)).

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

The bar keeps the mode colour. Enter still sends: the prompt is queued and runs as its own turn once the running one ends well. After Esc or a failed turn, queued prompts are not sent; they go back into the prompt, ahead of what is typed, separated by blank lines. A dim "esc to cancel" sits against the right edge of the label row. When the turn ends, the mode label comes back and the hint goes.

The spinner runs for the whole turn: thinking, writing and tool calls. What exactly the turn is doing shows in the chat.

## Question

The question tool lets the model stop mid-turn and ask you something, as Claude Code's AskUserQuestion does. One call asks 1 to 4 questions. Each has 2 to 4 options, picks one or any number of them, and can give every option a one-line description and a multi-line preview. Every question also gets an open field for your own answer, so the model never adds an "Other" option itself.

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
- **Options.** A number, then the label in blue, as model ids are in the model picker. The highlighted one has `→` and is bold magenta (`theme::pick`). The description is dim, in one column after the longest label, and is cut with `…` when it does not fit. Once a one-choice question is answered, a green `✓` follows the chosen label, so it still shows when you come back to its tab.
- **Open field.** Always the last row. Highlighting it and typing writes straight into it, with no separate edit mode; typing a letter on an option jumps there and starts your answer. "Type your own answer…" is the dim placeholder.
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

**Previews**

An option can also carry a `preview`: several lines of text, such as an ASCII mockup of a layout, a code snippet or a config, for when the choice is easier to see than to describe. When any option of the question has one, the panel splits in two, and the right side shows the highlighted option's preview, then its description under it. Moving the highlight swaps what the right side shows.

```
▎ question                                         ↑↓ · 1-3 · enter · esc
▎ Which layout for the status bar?
▎ → 1. Two lines     │ ┌──────────────────────────┐
▎   2. One line      │ │ glm-5.3 · ~/repos/nth    │
▎   3. Type your…    │ │ git · main +2 *1         │
▎                    │ └──────────────────────────┘
▎                    │ Room for git on its own row
```

- **Left: the options.** Numbers and labels as above, without the description column, which moves to the right side. The column is as wide as the longest label, at most 40% of the panel; longer labels are cut with `…`.
- **The rule.** A dim `│` between the sides, down every row under the question. It is the only divider line in nth: two columns of free text, one of them ASCII art, need something to keep them apart.
- **Right: the preview.** Shown as written, in the default fg, never wrapped, so a mockup keeps its shape; lines too long for the side are cut. The description follows on the next row, dim and wrapped.
- **Nothing to show.** An option without a preview shows only its description, and the open field shows nothing.
- **Height.** The tallest preview plus its description counts towards the panel's height alongside the options, still within half the terminal. A preview taller than that is cut at the bottom.

**Several questions**

```
▎ ☒ Auth   ☐ Checks   ✓ Submit       tab question · ↑↓ · space toggle · enter · esc
▎ Which checks should run before commit?
▎ → [x] 1. fmt        cargo fmt --check
```

- **Tab row.** Replaces the title: each question's short header, `☒` once it is answered and `☐` before, the current one bold magenta. Tab and shift+Tab move between them, and so do ←→ except while the open field is highlighted, where they move the cursor. Answering one moves to the next unanswered one.
- **Submit.** The last tab reviews every answer before they go. Enter sends them; an unanswered question shows in yellow, and Enter waits until there are none.

```
▎ ☒ Auth   ☒ Checks   ✓ Submit                          enter send · esc
▎ Auth    OAuth (Recommended)
▎ Checks  fmt, clippy, "and a doc check"
```

**Height.** Set once, when the panel opens: the title row plus the tallest question with its options and open field, or its tallest preview, at most half the terminal; past that the options scroll. It stays fixed while open, like every input panel, so moving between questions never makes the layout jump.

**In the chat.** The call's row is `¿ question  Auth, Checks`, with the dim icon while you answer. It stays one row once answered, like skill: the answers are for the model.

**Several at once.** Tool calls run in parallel, so two can ask together; the second waits until the first is answered or declined. When the turn ends, any question still open goes with it.

**Esc.** Declines: the panel goes, the prompt comes back with its text, and the model reads that you declined and carries on. Esc at the prompt cancels the turn, as it always does.

## Status bar

Fixed at 2 lines, always visible, below the input panel. It holds general state, never anything you interact with. Line 1 is where you are; line 2 is what is queued and what checks the model's writes. The right side of a line is cut first when it is too narrow.

```
 glm-5.3 · ~/repos/nth ⣿⣿⣿⣷⡀⠀⠀⠀⠀⠀⠀⠀⠀          git · fix-auth +3 *4 󰊐 2
 ⏵ 2 queued · fix the failing test                    ● rust  ● typescript
```

**Line 1: where you are**

Left-aligned: `model · effort · path context`, all bright white. The git branch and status sit against the right edge.

Colours here are the terminal's standard colours; see [Colours](#colours). Purple in the starship config is magenta.

| Part    | Shows                                                                                                                           | Colour       |
| ------- | ------------------------------------------------------------------------------------------------------------------------------- | ------------ |
| Model   | The current model, and its effort unless default                                                                                | bright white |
| Path    | The working directory, with home written as `~`                                                                                 | bright white |
| Context | Context used as a [braille bar](https://github.com/kloki/braille-bar), 13 characters wide, scaled to the model's context window | white        |

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

| Part                                   | Format                   | Colour  |
| -------------------------------------- | ------------------------ | ------- |
| Conflicted                             | `` (U+F071, warning)    | red     |
| Ahead                                  | `+N`                     | yellow  |
| Behind                                 | `-N`                     | yellow  |
| Diverged, in place of ahead and behind | `󰱮` (U+F0C6E)            | white   |
| Modified                               | `*N`                     | magenta |
| Renamed                                | ` N` (U+F0EC, exchange) | yellow  |
| Deleted                                | ` N` (U+F1F8, trash)    | red     |
| Staged                                 | `󰊐 N` (U+F0290)          | blue    |
| Untracked                              | ` N` (U+F128, question) | white   |
| Stashed                                | `` (U+F187, archive)    | white   |

Icons are Nerd Font glyphs, as in the starship config. Conflicts are red rather than the default colour, because they block a commit and should be the first thing you notice.

The status comes from one `git status --porcelain=v2 --branch` plus a stash check. It is refreshed at start-up, after every tool call that can write, and at the end of each turn, off the async runtime.

**Line 2, left: queued prompts**

While prompts are queued, `⏵ N queued · ` and the first line of the next one, in white. Blank otherwise.

**Line 2, right: the language servers**

Against the right edge. A dot and the id of every language server the tools have started, in the order they started. Nothing shows until a server starts.

| Part | Shows | Colour |
| --- | --- | --- |
| Server dot | `●`, by state: connected, starting, broken | green, yellow, red |
| Server id | The server's id, as in opencode: `rust`, `typescript` | white |

Servers start on the first read or write of a file they cover, so none show at start-up. Their states come from the same language servers the tools use, over a `watch` channel. The formatters that run on writes are not shown; `nth formatters` lists them.

## Colours

Every colour is one of the terminal's 16 standard colours, so the terminal theme decides how it looks. nth never sets a colour of its own.

| Name in this doc | Terminal colour | Used for                                  |
| ---------------- | --------------- | ----------------------------------------- |
| red              | red             | path, deleted, conflicted, errors         |
| green            | green           | branch, your messages, success            |
| yellow, orange   | yellow          | ahead, behind, renamed, interrupted       |
| blue             | blue            | model, act mode, staged                   |
| magenta, purple  | magenta         | plan mode, model picker, highlighted items, modified |
| cyan             | cyan            | tool names, model answer bar              |
| white            | white           | context bar, untracked, stashed           |
| bright white     | bright white    | status line 1 text, tool output bar       |

Orange is not a standard terminal colour, so it means yellow.

## Open questions

- Does the chat banner (`nth · model · place`) stay, now that status line 2 shows the same?
