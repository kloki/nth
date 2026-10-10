# nth — UI layout

The screen is three bands stacked top to bottom. Each band has one job, and none of them knows how the others draw.

| Band          | Job                    | Default                                 |
| ------------- | ---------------------- | --------------------------------------- |
| Content panel | What you look at       | Chat history, or another open tab       |
| Input panel   | What you type into     | The prompt                              |
| Status bar    | What is true right now | Always 2 lines                          |

```
 [› chat] ● diagnostics                                    nth 0.2.0  header
 ▎ you  add retry to the fetch client                                 ┐ content
 ▎ read  src/client.rs                                                │
 ▎ Added exponential backoff with jitter …                            ┘
▎ plan                                                                ┐
▎ Ask anything.                                                       │ input, 4 rows
▎                                                                     │
▎                                                                     ┘
 glm-5.3 · ~/repos/nth ⣿⣿⣿⣷⡀⠀⠀⠀⠀⠀⠀⠀⠀      git · fix-auth #123 +3 *4 󰊐 2 ┐ status, 2 lines
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

- **Default: chat history.** The transcript, scrolled. Until anything is said, an ASCII field fills it instead (`hero.rs`, after performative-ui's AsciiHero): dim characters that drift with time and ripple and brighten under the mouse.
- **Tabs.** The content panel holds a list of tabs, and chat is always the first and can't be closed. Diagnostics, Plan, a tab per monitor and a tab per subagent are the others so far. Later come Diff and comment threads on the plan; they replace the side pane and agents sidebar sketched in design.md.
- **Tab strip.** On the left of the header, always shown: `[› chat] ● diagnostics  ≡ plan +3 -1  $ ci  @ find tabs`, in the order the tabs were opened. Each tab is its icon and name on the default background; the showing one is wrapped in `[ ]` and the others in spaces, so moving between them never shifts the strip. `nth` and its version stay on the right.
- **Tab colours.** A tab's foreground says how it is doing, the same way for every kind:

  | Colour  | State                              | Chat                    | Plan         | Monitor | Subagent            |
  | ------- | ---------------------------------- | ----------------------- | ------------ | ------- | ------------------- |
  | default | nothing to tell                    | idle, or you stopped it | not approved |         |                     |
  | blue    | working                            | turn running            |              | running | starting or running |
  | green   | done, until the tab has shown      | turn ended while away   | approved     |         | answered, showing   |
  | red     | failed, until it is something else | turn failed             |              |         | stopped or failed   |
  | magenta | needs you                          | a question waits        |              |         |                     |

  Diagnostics is always the default. A monitor's tab closes once its process stops, so it is only ever blue; a subagent's closes once it answered, so green shows only on the one you are looking at. Green fades once you have looked at the tab, so it means something new to see; the plan's stays until the plan is revised, since an approval is a fact about it. Red stays.
- **Read and navigate only.** Content tabs scroll and select, but text entry always goes through the input panel. Scrolling keys and the mouse wheel move the showing tab.
- **Mouse.** A click on a tab in the header shows it. In a chat, a click on a link opens it in your browser (markdown links and bare `http(s)://` addresses; nothing else opens, as the model writes the targets), and a right click on an entry copies what it says, an answer as its markdown, a tool row as its output. On the status bar, a click on the branch's pull-request link opens the PR, the one thing in the bar that answers a click. The copy goes through the terminal (OSC 52), so it works over ssh; tmux needs `set-clipboard on`. The status bar says `copied` or `opened …`, meaning the terminal or the opener was told: neither reports back.
- **Independent of the input panel.** Switching tabs never changes the input panel, and the other way round. The tab keys work with any input panel open. The one exception is a subagent's tab: the prompt stays, but talks to that subagent and says so in its label; see [Subagents](#subagents).

| Key              | Does                                                  |
| ---------------- | ----------------------------------------------------- |
| ctrl+t           | Shows the next tab, from the last back to chat        |
| ctrl+1 … ctrl+4  | Shows that tab; chat is always 1                      |
| ctrl+q, `/close` | Closes the showing tab, unless it is chat, a monitor, a running subagent, or the plan while there is one |
| ctrl+w           | On a monitor's tab: stops it, and the tab closes. On a subagent's: stops it, or closes the tab once stopped |
| esc              | On a subagent's tab: stops its turn; elsewhere cancels the main session's turn |
| ctrl+g           | Opens your editor on the plan while its tab shows, else the prompt; see [Plan](#plan) |

Ctrl with a digit only arrives as its own key in terminals that disambiguate escape codes (kitty, foot, wezterm, ghostty); elsewhere ctrl+t reaches every tab.

- **Opening.** A command opens its tab, or shows it when it is already open. The agent can switch tabs too, with the `panel` tool (`chat`, `diagnostics` or `plan`); in `nth run` there is nothing to switch, and the tool tells the model so.

## Monitors

The `monitor` tool leaves a command running; each one gets its own tab, opened without being shown so the chat keeps the focus. The label is `$` and the monitor's description, `$ ci`, in blue.

```
$ tail -f deploy.log | grep --line-buffered ERROR
running · 42s · 3 events · ~/.local/share/nth/monitors/<session>/1.log

ERROR db timeout
warning: slow query        (stderr, dim)
```

The tab follows the newest line unless scrolled up, and keeps the last 2000 lines; the log has all of them.

- **Stopping.** ctrl+w stops the showing monitor. The model stops one with `monitor_stop`, and one also ends by exiting, timing out or printing too much.
- **Closing.** A monitor's tab closes by itself the moment its process stops, however it ended; the log keeps all of its output, and the model hears how it ended. ctrl+q and `/close` leave a running one open and say on the status bar to stop it first.
- **Leaving.** `/clear` and `/resume` stop every monitor; their tabs close as each process stops.
- **Quitting.** With monitors running, ctrl+c on an empty prompt (or `/exit`) only warns on the status bar: `1 monitor running · ctrl+c again to quit`. The second ctrl+c stops them and saves their end notices in the session, so a resumed model knows they are gone.
- **Notices.** What a monitor says reaches the model between its steps, or starts a turn when idle; after Esc it waits for your next prompt. The chat shows each as a row: `& monitor 1 · ci · 2 lines`.

## Subagents

The `task` tool starts a subagent: an agent nth-context found (`nth agents` lists them: opencode's `general` and `explore`, plus your own `.claude/agents/*.md` and the like) on a session of its own, in the background. The call returns at once with the subagent's id, and the answer reaches the model as a notice when it is done, as a monitor's output does: between its steps, or waking it when idle. Each subagent gets its own tab, opened without being shown. The label is what it was asked, cut at 20 characters, behind `@`: `@ find how tabs open`, coloured by its state: blue while its turn runs, green once it answered, red when it was stopped or failed. While its turn runs, a one-character braille spinner stands in for the `@`, so the tab keeps its width.

```
@explore · find how tabs open · running · 12s · 3 tool calls · 1 queued

▎ Find where the content panel's tabs are opened …
  ≡ read   crates/nth-tui/src/app/content.rs
▎ Tabs open in `Content::open` …
```

- **Header.** The agent, what the model asked of it, its state in colour (running yellow, done green, interrupted yellow, failed red), how long, how many tool calls this turn, and how many prompts wait in its inbox.
- **Chat.** Its own transcript under the header, drawn like the main chat: the prompt it got, its tool calls with their output, its answer, and a turn summary.
- **The prompt is its.** While a subagent's tab shows, the input panel talks to it: the label row reads the agent's name, `explore`, in cyan in place of `plan` or `act`, and the bar turns cyan with it. Enter sends what you typed to the subagent; it waits in its inbox behind whatever the subagent is doing, and the tab shows it once its turn starts. The spinner and `esc to cancel` follow the subagent's turn, not the main session's. Tab and shift+Tab do nothing: the mode belongs to the main session. The main session never hears what you say to a subagent; the model gets only the answers to its own tasks, and continues a subagent with `task_id`.
- **Commands.** `/` commands work as everywhere. A `!` command belongs to the main session, so running one from a subagent's tab shows the chat tab where its output lands.
- **Stopping.** Esc or ctrl+w stops the subagent's running turn and drops the prompts queued behind it; the tab stays, red, and the subagent can be prompted again. Esc on the chat tab cancels the main session's turn only; subagents keep running, like monitors. The model can stop one of its own with `task_stop`, which the tab shows the same way.
- **Budget.** A turn the model started fails once it has spent `[task] max_steps` model requests (50) or `[task] timeout_secs` seconds (600, 0 for no clock); the tab turns red with the reason and the model's notice says so, and it can continue the subagent by its `task_id`. A prompt you type on the tab has no clock: you are watching it.
- **Closing.** A subagent's tab only closes once its turn has ended. It closes by itself once the subagent answered, or when you leave it if it is showing; a stopped or failed one stays, red, to say why, until ctrl+w, ctrl+q or `/close`. On a running one, ctrl+q and `/close` say on the status bar to stop it first. A closed tab opens again when the model continues its subagent, with its earlier turns, so nothing runs out of sight.
- **Leaving.** `/clear` and `/resume` end every subagent: what they had queued never runs, and what they answer does not reach the next session's model. Their tabs close as each turn ends.
- **Quitting.** With subagents running, ctrl+c on an empty prompt (or `/exit`) only warns on the status bar: `1 subagent running · ctrl+c again to quit`, counted with the monitors. The second ctrl+c stops them and saves their notices in the session, so a resumed model knows they are gone.
- **In the chat.** The task call is one row, `↳ task  find how tabs open`, its result the id the model continues it with. The answer shows as a notice row when it arrives: `↳ subagent 1 · explore · completed` (or `failed`, `interrupted`); its text is for the model.
- **Headless.** `nth run` has nothing to wake the model, so there the task tool waits for the subagent and returns its answer in the call.

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

- **Label.** `≡ plan` in the tab strip, or `≡ plan +3 -1` while lines are marked; green once approved, until the plan changes.
- **Header.** The file, what changed, and the keys, dim.
- **Scrolling.** Like the chat: the scroll keys and the wheel move it, and a grey scrollbar thumb sits in the right margin while the plan is longer than the tab.
- **Lines.** Every line of the plan, wrapped at the tab's width. An added line is green behind `+`, a removed one red behind `-`, and an unchanged one has no mark.
- **What is marked.** The changes of the latest turn that changed the plan, against the plan as it was before that turn. A turn that only talks leaves the marks as they are, so asking a question about the plan does not wipe what its last revision did. The first plan of a session is shown unmarked too, since every line of it would be new, and so is the session's plan when it is opened by starting nth or `/resume`.
- **Reading.** The file is read after every write, edit or apply_patch, at the end of each turn, and when a session opens. A plan deleted from disk closes the tab.

**ctrl+g: commenting on the plan.** You are not expected to write the plan yourself, but you can leave comments in it. On this tab, ctrl+g opens a copy of the plan in `$VISUAL`, else `$EDITOR`, else vi, run through the shell so `code --wait` works. Write comments anywhere (`<!-- why not X? -->`, a `>>` line, `[which crate?]`), reword or delete lines, then save and quit.

- **What the model gets.** The diff from the plan to your copy, in a `<plan-edits>` element, then an instruction: these edits are review feedback, a plain change is carried into the plan, a question is answered, an objection is met or argued, each comment is removed once handled, and the plan file itself is edited, never your copy. The instruction is `crates/nth-session/src/plan/plan_edits.md`.
- **In the chat.** One row, `✎ plan edits · +2 -0`, the diff and instruction being for the model only.
- **Nothing to send.** Saving without changes says `no changes to the plan`. Quitting the editor with an error, vim's `:cq`, drops the edits. An editor that fails to start says why on the status bar.
- **While the editor runs.** nth keeps running but draws nothing: a turn goes on, monitors keep reporting, and their output shows when you come back. Edits made during a turn wait for it to end.

**`/approve`.** Approves the plan: the mode switches to act, the chat shows, the marks clear, and the model gets opencode's approval, `The plan at <path> has been approved, you can now edit files. Execute the plan`, followed by the reminder that plan mode ended. The chat shows `/approve`. While a turn runs, the mode switches at once and the approval waits for it to end. Without a plan file it only says `no plan to approve` on the status bar.

## Diagnostics

Opened with `/diagnostics`, or by the agent. What nth found and runs for this project, for when something does not work as expected. It scrolls like the chat, with the same scrollbar while it does not all fit.

```
model
    plan  kimi-k3 · high · 256k context
  ▸ act   glm-5.3 · 128k context
  25 models served

model usage
  opencode/glm-5.3  ██████████████████████████████  142
  openrouter/kimi   ███████▍                         35
  opencode/qwen-4   ▏                                 1

instructions
  ~/repos/nth/CLAUDE.md

skills
  ✦ research-opencode  claude

agents
  ↳ explore  builtin
  ↳ general  builtin

language servers
  ✓ rust      ~/.cargo/bin/rust-analyzer  → ~/repos/nth
  ● ruff      ~/.local/bin/ruff  → ~/repos/nth
  ✗ gopls     not on PATH

formatters
  ✓ rustfmt   rustfmt $FILE
  ✗ prettier  no package.json here
```

- **Sections.** A bold title each, and rows under it: model, model usage, instructions, skills, agents, language servers, formatters. Context warnings follow the agents in yellow.
- **Model.** A row per mode, plan then act, each with the model, effort and context window it runs with, in blue. A `▸` marks the mode the next turn runs in; the other mode's name is dim.
- **Model usage.** A bar per model that ran a turn, by its `provider/model` id, most first, in blue and scaled to the busiest one; the eighth blocks give the end of a bar, and any used model shows at least one. The count follows dim. "no turns yet" until there is one.
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
| Model answer | blue  | Wrapped text under the bar                                |
| Thinking     | none  | `∴ thinking · 1.2s`, dim; the reasoning in italic below   |
| Tool call    | cyan  | The tool's icon, name in cyan, summary in dim; one line   |
| Turn summary | none  | `∎ model · N tool calls · 14.2s`, dim, after a blank line |
| Interrupted  | none  | `⏹ interrupted · 3.0s` in yellow, after a blank line      |
| Error        | red   | `✗ message` in red                                        |

- **Tool icon.** Each tool has its own icon, so calls are told apart at a glance: `≡` read, `>` write, `±` edit, `Δ` apply_patch, `$` bash, `*` glob, `/` grep, `↓` webfetch, `?` websearch, `✦` skill, `¿` question, `▣` panel, and `•` for any other. There is no success mark: the icon is dim while the call runs and cyan once it is done. A failed call turns its icon and name red and shows the error's first line.
- **Tool summary.** read and write show the path relative to the working directory. bash shows the command itself, not the model's description of it. skill shows the skill's name. question shows the questions' headers. A multi-line command shows its first line followed by `…`.
- **Turn summary.** `∎` closes the turn, as `∴` opens its thinking, and stays dim. The blank line above separates the summary from the last entry of the turn.

**Tool output**

Each tool call shows its output under its row as it streams in, and keeps it once the call finishes, so output that scrolls past too fast to read can be read back.

- **Bar.** Cyan, continuing the call's own bar, so the call and its output read as one block that lines up with the messages around it.
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

- **Default: the prompt.** See [Prompt](#prompt) below. The completion popup for `/` commands and `@` agents and files floats right above the row being typed, lined up with the `/` or `@` it completes.
- **One style.** Every input panel looks the same; see [Input panel style](#input-panel-style).
- **Context swaps it.** Today that is the model picker. Later come question tool answers, permission prompts, the session list and similar. Each is its own input panel.
- **Each input panel declares its height in lines.** The prompt is 4; the model and session pickers are a header, a query row and a list, 16 rows but at most half the screen. The height is fixed while the panel is open, so typing or filtering never makes the layout jump.
- **The model picker keeps the listing's order** and opens on the model in use.
- **Pickers filter as you type, as telescope does.** The model and session pickers have a query row under the title: `> query`, and how many items match against the right edge. Typing narrows the list by fuzzy matching (`fuzzy.rs`, nucleo, smart case, words in any order) and highlights the best match; the matched characters are bold and underlined. A model matches by its id and name, a session by its title and directory. Backspace widens the list again; ctrl+c clears the query, then closes. With an empty query the list keeps its own order: the listing's for models, newest first for sessions.
- **One input panel at a time.** Opening one replaces the prompt; finishing or `esc` returns to the prompt. The prompt keeps its text while hidden.
- **Keys go to the input panel first.** It handles what it knows and passes the rest on to app-level keys: content scrolling, tab switching and quit.

| Input panel        | Height   | Opens on              | Returns on    |
| ------------------ | -------- | --------------------- | ------------- |
| Prompt             | 4        | default               | —             |
| Model picker       | ≤16      | `/models`, ctrl+m     | enter, esc    |
| Question           | per call | the agent asks        | answer, esc   |
| Settings           | 3        | `/settings`           | esc           |
| Permission (later) | ~4       | a tool needs approval | allow, reject |

## Input panel style

Every input panel has the same shape, so a new one reads as the same kind of thing as the prompt. In code this is `theme::panel_title` and `theme::panel_row` in `crates/nth-tui/src/theme.rs`; build a new panel from those.

```
▎ switch model        type to filter · ↑↓ model · ←→ effort · enter · esc
▎ >                                                                    3/3
▎   opencode/kimi      Kimi     256k
▎ → openrouter/glm   ✓ GLM 5.3  128k
▎   openrouter/plain   Plain    32k
```

With models from more than one provider, the picker shows ids with their `provider/` prefix in one list; with one, without it. A provider that could not be listed is one red `✗ OpenRouter: 401 …` row after the models.

- **No border, default background.** The panel stands out by its bar, not by a box or a fill.
- **One accent colour.** The bar `▎` runs down every row in it, and the top row holds the panel's title or label in it too. The title is plain, not bold, so the content stays the loudest thing.
- **Content under the title.** Each row starts after the bar. A highlighted item is bold magenta (`theme::pick`), the same as in the completion popup.

| Panel        | Accent                            | Title                             |
| ------------ | --------------------------------- | --------------------------------- |
| Prompt       | the mode's colour: magenta for plan, blue for act; yellow for a command; cyan on a subagent's tab | the mode label (`cmd` for a command, the agent's name on a subagent's tab), or the spinner |
| Model picker | magenta                           | `switch model`                    |
| Question     | cyan                              | `question`, or a tab per question |
| Settings     | magenta                           | `settings`                        |

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
- **Commands.** `!` typed at the very start of the prompt makes it a command, as in opencode: the `!` is not kept, the label reads `cmd` and the bar turns yellow, and the placeholder becomes "Run a command.". Esc, ctrl+c on an empty prompt, or Backspace at the start goes back to the mode; Tab does nothing meanwhile. Enter runs the text with bash in the session's directory, with no timeout, and the model does not answer. The chat shows it as a bash row with its output, and the session keeps it the way opencode does: a user message saying the user ran a tool, then a bash call with its result, so the model sees it next turn. It runs like a turn: the spinner shows, Esc kills it, and a command sent while a turn runs is queued like a prompt. Prompt history keeps it with its `!`, and recalling it comes back as a command.
- **Placeholder.** "Ask anything." in dim when the prompt is empty.
- **Completion popup.** Sits right above the cursor's row, lined up with the `/` or `@` it completes, and moves left when it would run off the right edge.
- **Agents and files.** `@` starting a word lists the agents whose name starts with what follows (not opencode's `hidden` ones, and none on a subagent's tab), `@explore` with the first line of its description, then the files under the working directory that fuzzy-match it, and the files under a directory added with `/add-dir` after them, by absolute path, at most 8 rows in all; a query with a `/` in it is a path and lists files only. Ctrl+N or Enter fills in `@name `. Sent, `@explore` tells the model to call the task tool with that agent, as in opencode: you pick the agent, the model writes the task. The chat shows the prompt as typed; a word that is also a file under the working directory is the file.
- **Skills as commands.** `/` lists nth's commands first, then every skill, at most 10 rows; typing narrows them. A skill's row shows the first line of its description. Ctrl+N or Enter fills in `/name ` for the arguments, and Enter on a fully typed `/name [args]` runs it. The chat shows the command as typed; the model gets the skill's body with `$1`…`$N` and `$ARGUMENTS` filled in, `` !`cmd` `` replaced by the command's output and `@path` files attached. A skill named like a command is hidden behind the command.
- **Added directories.** `/add-dir <dir>` adds a working directory to the session, as Claude Code's does: the path expanded (`~`, or against the working directory) and canonicalized, and only when it names a directory that is not inside the working directory or one already added, nor around one. Typing after `/add-dir ` completes to the directories under the one being typed, symlinked ones too; Ctrl+N fills the highlighted one in over the whole argument, and so does Enter until the argument is a directory ending in `/`, when it runs the command, so Enter on `/add-dir ` or `/add-dir ~` walks down rather than adding the working or the home directory. `/add-dir` with nothing says how it is used. How many were added shows on the status bar as `(+N)` after the working directory; their files list in `@` (20,000 in all, the working directory's first), and the model and its subagents are told of them in their system prompts and may read and edit them by absolute path. They are saved with the session at once and kept by `/clear` and `/resume`, as the working directory is.
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

The bar keeps the mode colour. Enter still sends, and a plain prompt does not wait for the turn to end: the turn picks it up at its next step, skipping the tool calls it was about to run — they show as interrupted, like an Esc'd one — and the model hears your message at its next step. A reply without tool calls finishes first, and the prompt then runs as its own turn, ahead of anything that queued after it. A `!` command, `/approve`, plan edits and a `/skill` still wait for the turn to end well, as does a prompt sent while one of those already waits, while a command runs, or after Tab or the model picker changed what the next turn runs with. After Esc or a failed turn, what the turn had not picked up is not sent; it goes back into the prompt, ahead of what is typed, separated by blank lines. A dim "esc to cancel" sits against the right edge of the label row. When the turn ends, the mode label comes back and the hint goes.

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

## Settings

Opened with `/settings`. What the chats show, for this run of nth only: a change is never written to the config or the saved session, and the next nth starts with everything on.

```
▎ settings                   ↑↓ setting · enter toggle · esc
▎ → [x] thinking     the model's reasoning under ∴
▎   [ ] tool output  what each tool call returned
```

- **Toggles.** ↑↓ move, Enter or Space flips the highlighted one, Esc goes back to the prompt with its text kept. A change shows in the chat behind the panel at once.
- **Thinking.** Off, a reasoning entry is only its `∴ thought · 2.1s` line, without the text under it.
- **Tool output.** Off, a tool call is only its row, with any format note and language server errors under it, without what it returned.
- **Every chat.** Both apply to the main chat and every subagent's tab, and stay through `/clear` and `/resume`.

## Status bar

Fixed at 2 lines, always visible, below the input panel. It holds general state; the one thing in it you interact with is the branch's pull-request link. Line 1 is where you are; line 2 is what is queued and what checks the model's writes. The right side of a line is cut first when it is too narrow.

```
 glm-5.3 · ~/repos/nth ⣿⣿⣿⣷⡀⠀⠀⠀⠀⠀⠀⠀⠀          git · fix-auth #123 +3 *4 󰊐 2
 ⏵ 2 queued · fix the failing test                    ● rust  ● typescript
```

**Line 1: where you are**

Left-aligned: `model · effort · place`, the model and effort in bright white and the place in magenta. The git branch and status sit against the right edge.

Colours here are the terminal's standard colours; see [Colours](#colours). Purple in the starship config is magenta.

| Part    | Shows                                                                                                                           | Colour       |
| ------- | ------------------------------------------------------------------------------------------------------------------------------- | ------------ |
| Model   | The current model, and its effort unless default                                                                                | bright white |
| Place   | The working directory, with home written as `~`, then how many were added with `/add-dir` as `(+N)`                              | magenta      |
| Context | Context used as a [braille bar](https://github.com/kloki/braille-bar), 13 characters wide, scaled to the model's context window | white        |
| Spent   | What the session spent, subagents included: its price at the catalogue's rates and the share of input read from the prompt cache, `≈$3.10 · 82% cached`; `cache ?` when the provider never said | white        |

The context bar is empty until the first turn reports usage. When the model's context window is unknown, the bar is hidden. What the session spent shows from the first reported usage on. The price is a list-price estimate from models.dev, whatever the plan bills, and ends in `+` when some model had no price; it is left out when none had one.

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

**The branch's pull request**

After the branch, when the branch has one, the pull request's `#N` in yellow, the one clickable thing in the bar: a left click opens the PR in the browser, with `opened <url>` on line 2 until the next key. It shows whichever state the PR is in — open, merged or closed — and no link while the branch has none or the forge's tool cannot find it.

Which tool is asked is decided by where `origin` points: `gh pr view` on a GitHub remote (github.com, or an instance under it), `tea pr list` on any other, matched to the branch. The check runs at start-up, at the end of each turn and when the session moves, not after every write, since it goes over the network; no PR found by either is no error — there is simply no link. When the line is too narrow the link is cut with the rest of the right side, and cut off it is not clickable.

**Line 2, left: queued prompts**

While prompts wait — queued behind the turn, or on their way into it — `⏵ N queued · ` and the first line of the next one, in white. Blank otherwise.

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
| blue             | blue            | model, act mode, staged, model answer bar |
| magenta, purple  | magenta         | plan mode, model picker, highlighted items, modified, status bar place |
| cyan             | cyan            | tool names, tool call and output bar      |
| white            | white           | context bar, untracked, stashed           |
| bright white     | bright white    | status line 1 model                      |

Orange is not a standard terminal colour, so it means yellow.
