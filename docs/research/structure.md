# Code structure: research

Where nth's code structure has grown worse than its crate map, and what
to do about it. The crate layering is sound: no crate depends on one
above it, nth-tools does not know nth-session, and nth-context depends on
nothing. The debt is inside crates and at a few crate seams. It comes in
four kinds: copies of the same plumbing in nth-llm's three wire
protocols, one flat `App` in the TUI, contracts between crates that no
crate owns, and features spread over several modules in nth-session.

Research done 2026-10-19 at master `6c0ec97`. Every finding was read in
the code; the counts below were measured with `wc`, `grep` and `diff`.
Nothing here changes behaviour: each fix is a move, a merge of copies or
a rename.

## Where nth stands

About 70,000 lines in 13 crates, roughly half of them tests. Several
files that look large are mostly tests.

| Crate        | Lines  | Tests   | Notable                                                                                          |
| ------------ | ------ | ------- | ------------------------------------------------------------------------------------------------ |
| nth-tui      | 23,674 | ~10,000 | `App` has 111 fields; `run`'s `select!` 34 arms over a 32-variant `Step`; 9 fake providers       |
| nth-session  | 13,855 | ~7,700  | 15 fake providers; `session.rs` 557 code / 744 tests; `agent_loop/compact.rs` 201 / 985          |
| nth-tools    | 7,567  | ~3,500  | one folder per tool; the write path copied in three tools                                        |
| nth-llm      | 6,699  | ~3,000  | each protocol 300-340 code / 360-415 test lines; `success()` byte-identical in all three         |
| nth          | 4,144  | ~1,200  | `config.rs` 470 code / 575 tests; `main.rs:209-436` is session wiring shared by `chat` and `run` |
| nth-protocol | 3,321  |         | `inbox/notice.rs` is 722 lines of session domain types                                           |

| File                              | Code  | Tests |
| --------------------------------- | ----- | ----- |
| `nth-tui/src/app/turn.rs`         | 598   | 1,253 |
| `nth-tui/src/app/mod.rs`          | 1,170 | 0     |
| `nth-tui/src/chat/transcript.rs`  | 536   | 634   |
| `nth-session/src/session.rs`      | 557   | 744   |
| `nth-session/src/subagent/mod.rs` | 438   | 515   |
| `nth-session/src/work/store.rs`   | 528   | 207   |

The real outlier is `nth-tui/src/app/mod.rs`: 1,170 lines with no tests,
holding the struct, `Step`, `run`, `draw`, `draw_tab`, command dispatch
and usage loading.

## nth-llm

The three protocols are near-copies around a small core. What differs
per protocol is the request body (`wire.rs`), the events (`event.rs`),
how one event updates the reply, the in-stream error payload and its
retry policy, and request signing. Everything else is copied.

| #   | Finding                                                                                                                                                                                                                                                | Proposal                                                                                                             | Size |
| --- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------- | ---- |
| L1  | `Client` and `new` identical in all three (`chat_completions/mod.rs:58`, `messages/mod.rs:85`, `responses/mod.rs:56`); `success()` byte-identical (`:138`, `:133`, `:102`); `status_error()` identical in messages and responses                       | One `stream.rs` with `Client<P>`, `success` and `status_error::<P>`                                                  | M    |
| L2  | Three `Error` enums with the same seven variants; three `retry()` whose Status, Http, Incomplete and Stalled arms match                                                                                                                                | Generic `Error<P: Payload>` and `retry::<P>`, the `Payload` trait carrying each protocol's retry and overflow policy | M    |
| L3  | Three ~55-line `events()` unfold loops (`:219`, `:205`, `:168`) differing only in tail tolerance and the per-event apply                                                                                                                               | A `Protocol` trait implemented by each `State` and one driver                                                        | M    |
| L4  | Seven tests with the same names in all three, `an_overflow_refusal_becomes_a_context_overflow` to `only_transient_errors_are_retryable`                                                                                                                | Test the shared layer once against a null protocol                                                                   | M    |
| L5  | `USER_AGENT` set at five call sites                                                                                                                                                                                                                    | Set it once on the client builder in `http.rs`                                                                       | S    |
| L6  | `Endpoint::chatgpt()` (`providers/endpoint.rs:92`) sniffs credentials to branch `list()` and `wire()`; the `listed` mutex sits on every endpoint but only ChatGPT fills it; `Providers::stream` (`providers/mod.rs:162-210`) does per-wire preparation | `enum Kind { Configured, ChatGpt { listed } }` and `Endpoint::stream`                                                | M    |

## nth-tui

`App` is one flat bag: 111 fields, 18 of them `Job`s and 10 channel
ends. Adding a tab touches six exhaustive `Tab` matches
(`app/content.rs:50`, `:282`, `:318`, `:564`, `app/mod.rs:893`,
`app/mouse.rs:98`), the fields, `Step` and a `select!` arm. Plan code
lives in 10 files, subagent code in 10, work in 8.

| #   | Finding                                                                                                                                                                                                                                                                                   | Proposal                                                                                                                                                                                                                                                 | Size |
| --- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---- |
| T1  | `Diagnostics`, `UsageView`, `FileView`, `PlanView` and `WorkView` each carry `top`/`max_top`/`height`, six scroll methods, the same `scrollbar()` and render block; `app/content.rs:335-553` forwards to them in eight `Scrollable` impls; `MonitorView` re-implements following the tail | One `Pager` (with an optional follow flag) and one rendered-lines `Cache`                                                                                                                                                                                | M    |
| T2  | Wiring files not named after the view they wire: `checks.rs` for diagnostics, `shown.rs` for file, `llms.rs` for llm_picker, `resume.rs` for session_picker, `spent.rs` for usage; question wired in `input.rs`, settings in `keys.rs`                                                    | Rename to match; add `app/question.rs` and `app/settings.rs`                                                                                                                                                                                             | S    |
| T3  | `notices_due = Some(now + NOTICE_DELAY)` armed in three places (`app/monitor.rs:41`, `app/subagent.rs:96`, `app/turn.rs:503`)                                                                                                                                                             | `App::arm_notices`                                                                                                                                                                                                                                       | S    |
| T4  | `start_shell`, `start_compact` and `start_turn` (`app/turn.rs:160-372`) repeat the same tail and build `FrontEnd` twice                                                                                                                                                                   | One `ended()` and `front_end()`                                                                                                                                                                                                                          | S    |
| T5  | `app/monitor.rs:82-181` holds tab closing and quitting, not monitors                                                                                                                                                                                                                      | Move to `app/content.rs` and a new `app/quit.rs`                                                                                                                                                                                                         | S    |
| T6  | `keys.rs::apply` (`:114-341`) is one 230-line function, a block per `Input`                                                                                                                                                                                                               | Dispatch to a `key_*` method in each feature's wiring file                                                                                                                                                                                               | M    |
| T7  | `draw_tab` builds `diagnostics::Facts` inline (`app/mod.rs:935-975`); `load_tools` and `tools_found` sit in `mod.rs:1138-1170`                                                                                                                                                            | Move to the diagnostics and usage wiring                                                                                                                                                                                                                 | S    |
| T8  | 111 fields in one struct                                                                                                                                                                                                                                                                  | Per-feature structs (`PlanTab`, `WorkTab`, `Files`, `MonitorTabs`, `SubagentTabs`, `UsageTab`, `DiagnosticsTab`, `Workspace`), each with `async fn next()`; `Step` from 32 to about 20 variants, `App` to about 60 fields. Turn and Models stay on `App` | L    |
| T9  | Nine hand-written fake providers; `turn.rs`'s five (`Scripted`, `Hang`, `HangFirst`, `Answer`, `Gated`) are one configurable fake                                                                                                                                                         | One `Scripted` with `Reply { Events, Fail, Hang, HangUntil }`; split `turn.rs`'s tests by group                                                                                                                                                          | M    |
| T10 | The TUI parses wire text other crates write: `<skill_content`, `"Error: "`, `<system-reminder>`, `SHELL_PROMPT` in `chat/transcript.rs:159-239`; post-write notes scraped in `chat/after_write.rs`                                                                                        | Typed accessors from nth-session; post-write notes travel structured                                                                                                                                                                                     | M    |
| T11 | All-time tool totals aggregated in both `App::load_tools` and `nth usage` (`nth/src/usage.rs:48`, `:369`)                                                                                                                                                                                 | One function in `nth_session::usage`                                                                                                                                                                                                                     | S    |
| T12 | `live_work` (`app/work.rs:79-91`) matches subagents to items by stripping a description prefix                                                                                                                                                                                            | `Subagents` reports the item an assignment runs                                                                                                                                                                                                          | S    |
| T13 | `HOME` read directly five times (`app/mod.rs:451`, `app/resume.rs:98`, `app/turn.rs:525`, `session_picker/mod.rs:110`, `session_picker/view.rs:117`)                                                                                                                                      | Read it through `Paths`                                                                                                                                                                                                                                  | S    |
| T14 | Turn assembly (skill invocation, notices, spend-then-save) in both `start_turn` and `nth/src/run/mod.rs:40-80`                                                                                                                                                                            | One nth-session entry point both front-ends call                                                                                                                                                                                                         | M    |

`chat/scroll.rs` stays apart from the pager: the chat pages by half a
screen, hides its scrollbar while following and keeps following after
an up when nothing overflows. Merging them is a behaviour decision.

## Between crates

| #   | Finding                                                                                                                                                                                                                                      | Proposal                                                                                   | Size |
| --- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------ | ---- |
| X1  | The file-writing tools listed four times: `nth-session/src/subagent/mod.rs:15`, `nth-context/src/agents/mod.rs:359`, `nth-tui/src/chat/transcript.rs:518`, `nth-tui/src/app/mod.rs:1039`                                                     | One `tool_names` module in nth-context, re-exported by nth-protocol                        | S    |
| X2  | `"Error: "` on a failed tool result written at `nth-session/src/agent_loop/call.rs:23` and parsed back at `nth-session/src/usage.rs:88`, `nth-tui/src/chat/transcript.rs:199`; the interrupted text copied at `nth-tui/src/app/turn.rs:1017` | `failed`, `failure_reason` and `INTERRUPTED` in nth-protocol                               | S    |
| X3  | `parse_args` twice (`nth-session/src/agent_loop/call.rs:15`, `nth-tools/src/lib.rs:74`)                                                                                                                                                      | One in nth-protocol                                                                        | S    |
| X4  | `run_call` copies all 22 `ToolContext` fields by hand (`call.rs:46-68`)                                                                                                                                                                      | `ToolContext::for_call`                                                                    | S    |
| X5  | The XDG data directory computed in `nth-session/src/store.rs:64` and `nth-auth/src/store.rs:52`                                                                                                                                              | A data directory on nth-context's `Paths`                                                  | S    |
| X6  | `~` expanded three ways: `nth-tools/src/path.rs:15`, `nth/src/config.rs:463`, `nth-tui/src/command/add_dir.rs:26` (which reads `~foo` as home)                                                                                               | One `expand_home` beside `Paths`                                                           | S    |
| X7  | `age()` identical in `nth/src/session.rs:206` and `nth-tui/src/session_picker/mod.rs:143`; `shorten()` in `nth/src/agents.rs:122` and `skills.rs:132`; a literal `"!"` icon at `nth/src/main.rs:432` and `run/mod.rs:82`                     | One copy each; a `warn` icon                                                               | S    |
| X8  | `ToolContext` fields only nth-session uses (llm, subagent, clerk, context_window, max_output, compaction, extra_dirs), and the same model facts held loose on `Session` and `SubagentLlm`                                                    | One model-settings struct                                                                  | M-L  |
| X9  | `inbox/notice.rs`'s task and work types (`TaskNotice`, `WorkNotice`, `Todo`, `split_notices` and more) are used only by nth-session and nth-tui                                                                                              | An opaque notice in the `Inbox` (text, wakes, coalesce key); the types move to nth-session | L    |
| X10 | `SessionConfig`, `TaskConfig`, `WorkConfig`, `ChatConfig` and `LayoutConfig` live in the CLI while tools, format, lsp and notify own theirs                                                                                                  | Each crate owns its config                                                                 | S-M  |
| X11 | `main.rs:209-436` is session wiring; `takes_images` and limits (`main.rs:278-295`) restate `nth-tui/src/app/llms.rs:58`, `:64`                                                                                                               | `wiring.rs`; `Listing::get(model)`                                                         | S    |
| X12 | Unused `pub` items: `NOTICE_LINES`, `quiet_text`, `TaskId`, `LoadedInstructions` in nth-protocol; `system_prompt`, `Route`, `Turn`, `Spending` in nth-session                                                                                | `pub(crate)`                                                                               | S    |
| X13 | nth-context's skills and agents discovery repeat the project walk and the directory walker (`agents/mod.rs:196`, `:231`; `skills/mod.rs:206`, `:252`)                                                                                        | Extract both once                                                                          | S-M  |
| X14 | Monitor `log_dir` policy in nth-protocol (`monitor/registry.rs:211`)                                                                                                                                                                         | Beside `Store::images_dir`                                                                 | S    |

## nth-tools

| #   | Finding                                                                                                                                                                                                                                          | Proposal                                                                           | Size |
| --- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------- | ---- |
| W1  | write, edit and apply_patch each resolve, check `writable`, take `EDITS`, write, format under the lock and append notes; `EDITS` lives in `edit/mod.rs:20` and `write_with_dirs` in `write/`; `PostWrite::after_write` is used only by its tests | A `PostWrite::lock()` guard that formats; each tool keeps its own diagnostics call | S-M  |
| W2  | apply_patch's hunk application (`apply_patch/mod.rs:232-369`) beside its tool code                                                                                                                                                               | `apply_patch/apply.rs`, as `parse.rs` is                                           | S    |
| W3  | `DRAIN` in both `bash/mod.rs:21` and `monitor/mod.rs:34`                                                                                                                                                                                         | In `process.rs`                                                                    | S    |

A single closure-taking write helper does not fit: apply_patch writes
and reads many files under the lock, edit fails with model-facing
strings, and each tool reports diagnostics its own way.

## nth-session

| #   | Finding                                                                                                                                                                                                                                                                                           | Proposal                                                                           | Size |
| --- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------- | ---- |
| S1  | The task tool and `work_assign` duplicate the spawn path: the provider/tools/subagents/limits bundle (`subagent/task.rs:40-46`, `work/mod.rs:73-80`), the agent-not-found error (`task.rs:143-151`, `work/assign.rs:118-126`), the continue-by-id checks (`task.rs:156-175`, `assign.rs:178-199`) | `subagent/spawn.rs` with one `Spawner`                                             | M    |
| S2  | Compaction in four places: `compaction.rs`, `agent_loop/compact.rs`, `effective` in `agent_loop/stream.rs:170`, `agent_loop/tests/compaction_e2e.rs`; `compact.rs:60-70` re-implements `last_marker`                                                                                              | A `compaction/` folder                                                             | M    |
| S3  | Plan mode half in `session.rs`: `reminder` (`:528-547`), the plan-mode `Writable` (`:418`), its tests (`:876-1050`)                                                                                                                                                                               | All in `plan/`                                                                     | S-M  |
| S4  | `Session::prompt` and `Session::compact` build the same `Turn` (`session.rs:362-390`, `:451-480`); `Request` built twice (`agent_loop/stream.rs:84`, `compact.rs:133`)                                                                                                                            | `Session::turn()`, `Route::request`                                                | S    |
| S5  | Turn result to outcome mapped three times (`subagent/actor.rs:174-178`, `:276-281`, `work/mod.rs:155`)                                                                                                                                                                                            | `From` impls                                                                       | S    |
| S6  | Work tools repeat the lock-and-reconcile preamble (`close.rs:60-62`, `item.rs:59-74`, `status.rs:33-36`, `assign.rs:116-132`); the `Assignment` literal built twice; an empty `impl Work {}`; the work root computed twice                                                                        | `Work::locked_plan`, `Assignment::new`, one `work::root`                           | S    |
| S7  | `store.rs:222-262` repeats the directory scan; `subagent::Turn` is a `Result` alias beside the loop's `Turn`; `work::Limits` restates `subagent::Limits`                                                                                                                                          | One scan; rename to `Answer`; embed the limits                                     | S    |
| S8  | About 15 one-off fake providers, and five compaction tests with near-twins in the e2e file                                                                                                                                                                                                        | Extend the crate's `Scripted`; drop the unit twins                                 | S-M  |
| S9  | `subagent/mod.rs` mixes event and job types, child-session policy (`:109-176`) and the registry (`:178-437`)                                                                                                                                                                                      | `child.rs`, `registry.rs`                                                          | M    |
| S10 | subagent knows work through `Done::Work`, and work uses subagent's internals                                                                                                                                                                                                                      | A settlement trait so the actor stays work-agnostic                                | M    |
| S11 | `work/store.rs` mixes the model (`:27-218`), disk IO (`:221-340`) and the markdown format (`:382-527`)                                                                                                                                                                                            | `plan.rs`, `store.rs`, `format.rs`                                                 | M    |
| S12 | `usage.rs` mixes the ledger with table formatting; `columns()` used only by tests; the TUI recomputes widths `padded()` computes                                                                                                                                                                  | Split; reuse `padded`                                                              | S    |
| S13 | Prompt files in two layouts: `src/prompts/` for the session, beside the Rust file for plan, work and task                                                                                                                                                                                         | Beside their feature                                                               | S    |
| S14 | Serde defaults and fallbacks for saves from before a field existed (`session.rs:50-55`, `store.rs:264-273`)                                                                                                                                                                                       | A deliberate call under the no-backwards-compatibility rule; listed, not scheduled | S    |

## Sound as is

- **nth-session's `agent_loop/`** is split by step concern (stream,
  call, repair, steer, doom_loop), and `work/outline.rs` and the usage
  tables are presentation both front-ends share.
- **nth-tools** has one folder per tool with its `mod.rs`,
  `description.txt` and tests, and the large ones are large for real.
- **nth's `run/render.rs`** is a plain stream printer; it does not copy
  the TUI's chat rendering. Every subcommand is its own module.
- **nth-format, nth-lsp, nth-notify, nth-worktree, nth-icons and
  nth-auth** need nothing. nth-format and nth-lsp depend on nth-context
  only for `which`, `project_root` and `extension_keys`, which is fine.
- **The local HTTP test servers** (six copies) differ enough to leave.
- **Duration formatters** (seven) mostly differ on purpose; only `age()`
  is a true copy.

## Pull requests

The first batch, each from a worktree on an `nth/<topic>` branch. A, C
and D stand alone; B and E build on C.

| PR  | Branch                  | What                                                                                 | Needs |
| --- | ----------------------- | ------------------------------------------------------------------------------------ | ----- |
| A   | `nth/llm-shared-stream` | L1-L6: one stream layer under the three protocols, `Endpoint::Kind`                  |       |
| C   | `nth/shared-contracts`  | X1-X7: tool names, the failed-result format, `parse_args`, `for_call`, data dir, `~` |       |
| D   | `nth/tools-write-path`  | W1-W3                                                                                |       |
| E   | `nth/session-internals` | S1-S8                                                                                | C     |
| B   | `nth/tui-app-split`     | T1-T9, the feature structs starting with `PlanTab`                                   | C     |

Later, each its own PR: T10-T14, X8-X14, S9-S13. X9 (the notice types)
and X8 (one model-settings struct) are the largest; T8's feature structs
after `PlanTab` can go one per PR.
