# Images: research plan

What it takes for the model in nth to see an image. There are two ways in,
both on the model's side. You name a file (`look at shot.png`, or
`@shot.png` from the prompt's completion, which already sends the path as
text) and the model reads it. Or the model takes a screenshot itself and
reads that. Nothing enters from the prompt as bytes: no clipboard paste,
no attachments, and the TUI never draws a picture. Image *output* is out
of scope.

Research done 2026-10-17, against opencode `80fb31e`. Every task below is
answered; the design and pull requests at the end follow from them. The
probe scripts were throwaway and are not kept.

## Where nth stands

Text runs end to end, so these layers need a hand:

| Layer      | Today                                                                                              | Where                                                                         |
| ---------- | -------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| Protocol   | `ToolResult { content: String }` in the history, `ToolResult = Result<String, String>` from a tool | `nth-protocol/src/message.rs`, `tool.rs:133`                                  |
| Wires      | A tool result is a string on all three protocols                                                   | `chat_completions/wire.rs:56`, `messages/wire.rs:111`, `responses/wire.rs:58` |
| Catalogue  | `catalog::Model` has no `modalities`; `ModelInfo` says nothing about input                         | `nth-llm/src/catalog.rs:29`, `nth-protocol/src/provider.rs:234`               |
| Read tool  | Refuses anything that is not UTF-8: `cannot read binary file`                                      | `nth-tools/src/read/mod.rs:156`                                               |
| Store      | One JSON file per session, rewritten after every turn; listing parses every file in full           | `nth-session/src/store.rs:87`, `:152`                                         |
| Compaction | Transcript lines as text; size estimated as serialized chars / 4                                   | `nth-session/src/compaction.rs:193`, `:237`                                   |
| TUI        | A read's tool row shows its output highlighted as text                                             | `nth-tui/src/chat/render.rs`                                                  |
| Headless   | `render.rs` prints tool results as text                                                            | `nth/src/run/`                                                                |

Untouched: the prompt, the clipboard, `nth run`'s arguments, subagents and
work items (a subagent reads the file itself; an item names a path).

## What the models take

From models.dev, `modalities.input` containing `image`, for the providers
nth reaches:

| Provider (models.dev id) | Image input | Of  | Notes                                       |
| ------------------------ | ----------- | --- | ------------------------------------------- |
| anthropic                | 17          | 17  | Every Claude                                |
| openai                   | 47          | 53  | Every GPT-4o/4.1/5 line                     |
| opencode (Zen)           | 85          | 127 |                                             |
| opencode-go              | 22          | 32  | Kimi, GLM, Grok, DeepSeek flash, MiniMax M3 |
| xai                      | 13          | 13  | Refuses GIF (R1)                            |
| moonshotai               | 4           | 4   |                                             |
| zai                      | 6           | 18  | The `v` models only                         |
| deepseek                 | 3           | 4   |                                             |
| minimax                  | 1           | 7   | MiniMax-M3 only                             |

## R1 ✓ Wire shapes

Probed live with a test image sent three ways: inside a tool result, as a
text tool result followed by a user message carrying the image, and in a
plain user message. A model counted as seeing the image when it named
what the image showed: two colour halves, a solid colour, a written word.

The shapes, as sent:

| Protocol         | Image inside a tool result                                                                                                        | Image in a user message                                        |
| ---------------- | --------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------- |
| messages         | `tool_result.content` as an array: `{"type":"text",…}` then `{"type":"image","source":{"type":"base64","media_type":…,"data":…}}` | the same image block in the user turn, after the tool results |
| responses        | `function_call_output.output` as an array: `{"type":"input_text",…}` then `{"type":"input_image","image_url":"data:…;base64,…"}`  | `{"type":"input_image","image_url":…}` in `content`            |
| chat completions | the `tool` message's `content` as an array: `{"type":"text",…}` then `{"type":"image_url","image_url":{"url":"data:…"}}`          | the same `image_url` part in a user message's `content` array  |

What each endpoint did:

| Endpoint | Protocol         | Models                                      | Inside a tool result       | Tool result, then user image |
| -------- | ---------------- | ------------------------------------------- | -------------------------- | ---------------------------- |
| Zen      | messages         | claude-haiku-5-5, qwen3.8-flash             | seen                       | seen                         |
| Zen      | responses        | gpt-5.4-nano, grok-4.7                      | seen                       | seen                         |
| Zen      | chat completions | kimi-k3, glm-5.3-flash, deepseek-v4.1-flash | seen                       | seen                         |
| Go       | messages         | minimax-m3, claude-haiku-5-5                | seen                       | seen                         |
| Go       | responses        | grok-4.7, gpt-5.6-luna                      | seen                       | seen                         |
| Go       | chat completions | kimi-k3, deepseek-v4.1-flash                | seen                       | seen                         |
| Go       | chat completions | glm-5.3-flash                               | **refused** (seen once)    | seen                         |
| ChatGPT  | responses        | gpt-5.6-luna, gpt-6-luna                    | seen                       | seen                         |

- **GLM on Go is the exception that decides chat completions.** It took an
  image in a tool result in the first run, then refused it five times out
  of five later: `tool content: part type "image_url" is not supported;
  only text is`. Go seems to route GLM to more than one upstream. The
  text-then-user shape worked in every run on every chat completions
  model, so **chat completions always sends a tool result's images as a
  user message after the results**. messages and responses carry them
  inside the result.
- **GPT misreads tiny images.** On a 64×64 red-and-blue image every GPT
  answered wrongly, in every shape. On a 256×256 colour and a 512×160 word
  it was right every time. Test fixtures need real-sized images.
- **DeepSeek in thinking mode** refuses a tool turn without the
  assistant's `reasoning_content`; nth already sends it back.

Formats and sizes, one model per protocol and endpoint:

| Image        | Claude (Zen) | GPT (Zen, ChatGPT) | Grok (Zen)  | Kimi, GLM (Go) | MiniMax (Go) |
| ------------ | ------------ | ------------------ | ----------- | -------------- | ------------ |
| GIF          | ok           | accepted, misread  | **refused** | ok             | ok           |
| WebP         | ok           | ok                 | ok          | ok             | ok           |
| JPEG         | ok           | ok                 | ok          | ok             | ok           |
| PNG, 6.7 MB  | accepted     | accepted           | accepted    | accepted       | accepted     |
| PNG, 8200×64 | **refused**  | ok                 | ok          | ok             | ok           |

- Grok: `Downloaded response does not contain a valid JPG, PNG, WebP, or
  ICO image`. Claude: `At least one of the image dimensions exceed max
  allowed size: 8000 pixels`.
- No endpoint refused a 6.7 MB image, above Anthropic's documented 5 MB.

Per request, with the image in a user message:

| Request                    | Claude (Zen) | GPT-6 Luna (ChatGPT) | Kimi (Go)   | Grok (Zen)    |
| -------------------------- | ------------ | -------------------- | ----------- | ------------- |
| 50 small images            | ok           | ok                   | ok          | ok            |
| 101 small images           | ok           | ok                   | ok          | ok            |
| 250 small images           | ok           | ok                   | ok          | ok            |
| 3 × 6.7 MB (26 MiB base64) | ok           | ok                   | ok          | ok            |
| 6 × 6.7 MB (52 MiB base64) | ok           | ok                   | **timeout** | **503**       |

No count was refused, but above about 100 images the models' own counts
went wrong (Kimi said 150 of 250, Claude "about 150"), so images past that
are likely dropped quietly. Past 26 MiB some endpoints fail.

## R2 ✓ Catalogue

- Every one of the 8,988 models on models.dev has `modalities.input`. The
  `attachment` flag disagrees with it for 344 models, so the gate reads
  `modalities.input` containing `image` and ignores `attachment`.
- `catalog::Model` gains `modalities`, and `ModelInfo` a `takes_images`
  flag. An endpoint that borrows a model's entry (`borrowed`) borrows its
  modalities too: it is a fact about the model, like its limits.
- A model the catalogue does not know is treated as text-only, so a read
  of an image errors rather than sending what the server may refuse.
- **ChatGPT has its own list.** `GET
  https://chatgpt.com/backend-api/codex/models?client_version=<v>`
  (bearer token and `ChatGPT-Account-Id`; `client_version` is required, and
  models whose `minimal_client_version` is above it are left out, so send
  a high one) returns per model `slug`, `visibility` (`list` or `hide`),
  `supported_in_api`, `input_modalities`, `context_window`,
  `supported_reasoning_levels` and `supports_image_detail_original`. For
  this account it lists gpt-6-luna, gpt-5.6-terra and gpt-5.6-luna, all
  with `text` and `image`: exactly the three the backend served in R1.
  opencode has this fetch but it is commented out at the pin.

## R3 ✓ opencode

Written up in [opencode/04-images.md](opencode/04-images.md). What nth
takes: read's media types, the unsupported-model error text, the
oldest-first image budget (25 MiB trigger, 15 MiB target) with its
removal note, the `[Attached …]` compaction line and a per-image token
estimate. opencode does not downscale; nth will (R6).

## R4 ✓ Screenshots

**A skill, not a tool.** Every capture tool is a CLI that writes a file,
the read tool then shows it, and a skill's `` !`cmd` `` probe tells the
model which tools the machine has when it loads. No new tool, nothing
platform-specific in the code. The browser skill already screenshots web
pages with `agent-browser screenshot`; those files become visible the
same way.

Tried here (niri 26.04, one 3440×1440 output at scale 1); the captures
stayed local:

| Command                                          | Result                                                                                                       |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------ |
| `grim <file>`                                    | whole desktop, 3440×1440                                                                                     |
| `grim -o DP-2 <file>`                            | one output                                                                                                   |
| `grim -g "100,100 800x600" <file>`               | a region, 800×600                                                                                            |
| `niri msg --json windows`                        | every window's id, app_id and title, so the model can pick one by name                                       |
| `niri msg action screenshot-window --id N --path <file>` | a window, focused or not; not run, since it also replaces the clipboard (`--help`: "in addition to putting it in your clipboard") |

niri's window layout has no position on screen (`tile_pos_in_workspace_view`
is null for a tiled window), so a window can't be cut out with `grim -g`;
niri's own action is the way. Elsewhere: `grim` with `swaymsg -t get_tree`
or `hyprctl activewindow` for a window's geometry on sway and Hyprland;
`spectacle -b -n -o <file>` on KDE; `gnome-screenshot -f <file>` on GNOME;
`maim` or `import -window root` on X11; `screencapture -x <file>` (`-l
<window id>`, `-R x,y,w,h`) on macOS.

The skill's rules:

- Capture the smallest thing that answers: a window or region before the
  whole desktop, since everything on screen goes to the provider. Take
  the whole desktop only when asked.
- Write to `${TMPDIR:-/tmp}/nth-screenshot-<time>.png`, then `read` it.
  The read keeps its own copy (R5), so the temporary file may go.
- Small text that came out unreadable after downscaling: capture a region
  around it rather than the whole screen again.
- No capture tool found: say which to install for the platform, never
  install one unasked.

## R5 ✓ Where the bytes live

**Sidecar files, content-addressed, written by the read tool:**
`<data dir>/images/<root session id>/<sha256>.<ext>`, the layout the
monitor logs use (`monitors/<session id>/`). The history holds a
reference, the image bytes never go into the session JSON.

- Not inline base64: `Store::list` parses every session file in full for
  `/resume`, `nth session list` and `nth usage --all`, every turn rewrites
  the whole file, and compaction's estimate (serialized chars / 4) would
  count a 1 MB screenshot as about 250,000 tokens.
- Not re-read from the path: a screenshot in `/tmp` goes away, and a file
  edited since would show the model a different image than it read.
- Written at read time, not at save time: subagent sessions are never
  saved, and a read in a subagent must still find its bytes. Keyed by the
  root session (a subagent's `parent`), so `nth session delete` removes
  the subagents' images with the parent's, beside the plan, work and
  monitor folders it already removes.
- Content-addressed: the same screenshot read twice is stored once.
- A worktree move changes nothing: the folder is under the data dir.
- In memory an `Image` holds `mime`, `sha256`, width and height, and its
  bytes (an `Arc<[u8]>` skipped by serde); `Store::load` fills the bytes
  back from the folder, and a missing file becomes the removal note.

## R6 ✓ Size and cost

A synthetic 4K screenshot (3840×2160, a terminal log in 26 px text and a
sidebar in 18 px, as on a 4K screen at 2× scale) sent in a tool result at
each size. The question was two codes, one in each text size. Token cost
is the request's input tokens minus the same request without the image.

| Longest side | PNG     | Claude Haiku | MiniMax M3 | GPT-5.4 nano | GPT-6 Luna | Grok 4.7 | Kimi K3 | DeepSeek flash |
| ------------ | ------- | ------------ | ---------- | ------------ | ---------- | -------- | ------- | -------------- |
| 3840         | 631 KiB | 4805 ✓✓      | 2954 ·✓    | 2768 ··      | 9793 ✓✓    | 2443 ✓·  | 10777 ✓✓ | 969 ·✓        |
| 2560         | 1.1 MiB | 4805 ✓✓      | 2954 ·✓    | 2768 ··      | 4321 ✓✓    | 2443 ✓·  | 4794 ✓✓ | 969 ··         |
| 1920         | 806 KiB | 2712 ✓✓      | 2693 ·✓    | 2452 ··      | 2449 ✓·    | 2078 ✓✓  | 2701 ✓✓ | 969 ·✓         |
| 1568         | 787 KiB | 1813 ✓·      | 1794 ✓·    | 1650 ··      | 1647 ··    | 1410 ✓✓  | 1804 ✓· | 822 ··         |
| 1280         | 536 KiB | 1217 ✓·      | 1198 ··    | 1108 ··      | 1105 ··    | 958 ✓·   | 1208 ·· | 579 ··         |
| 1024         | 407 KiB | 798 ··       | 779 ··     | 695 ··       | 692 ··     | 614 ··   | 789 ··  | 367 ··         |

Tokens, then whether the 26 px code and the 18 px code were read exactly.
GLM refused the tool-result shape (R1).

- **1920 px on the longest side is the default.** It is a 4K screen's
  logical size at 2× scale, every model that can read the screenshot at
  all reads normal text there, and it costs about 2,500 tokens. The 18 px
  text still fails for some at 1920; that is what a region capture is for.
- Claude, MiniMax, GPT nano, Grok and DeepSeek shrink large images on the
  server, so 3840 costs them the same as 2560. GPT-6 Luna and Kimi do not:
  a full 4K screenshot costs them about 10,000 tokens. Downscaling in nth
  is what keeps the cost even.
- GPT-5.4 nano, MiniMax and DeepSeek misread `Q` as `0` at every size: a
  model limit, not a scale one.
- Downscaling a 4K PNG with the `image` crate (0.25, png, jpeg, gif and
  webp only, 32 crates) on a release build: decode 25 ms, resize to 1920
  74 ms (Triangle) to 113 ms (Lanczos3), PNG encode 4 ms. It runs on a
  blocking thread. Triangle is enough for screenshots and makes the
  smallest file.
- Caps: each image within 1920 px on the longest side (well inside
  Claude's 8000 px) and re-encoded as PNG when it is GIF or larger than
  needed; a request's images bounded at 25 MiB, trimmed oldest first to
  15 MiB, and at 50 images, since counts past about 100 went wrong.
- Compaction counts an image at 2,500 tokens, the measured cost at 1920,
  instead of its serialized size.

## Design

- **Tool results carry images.** `ToolResult` becomes `Result<Output,
  String>` with `Output { text, images: Vec<Image> }`; the history's
  `Message::ToolResult` gets the same `images`. Every tool but read
  returns text alone. `Message::User` stays a string.
- **Wires.** messages and responses put the images inside the tool result
  (R1's shapes). Chat completions sends the result's text in the `tool`
  message and its images in one user message after the turn's results.
- **Read reads images.** png, jpeg, gif and webp by magic bytes. It
  downscales to 1920 px, re-encodes GIF as PNG, writes the sidecar (R5)
  and returns `Image read: shot.png, 1920×1080 png (from 3840×2160)` with
  the image.
- **Gating.** `ModelInfo::takes_images` from the catalogue, or for
  ChatGPT from the backend's list (R2). The read tool sees it through
  `ToolContext` and errors with opencode's text on a text-only model. A
  request to such a model, after a switch mid-session, replaces earlier
  images with the same text.
- **Budget.** Before each request, past 25 MiB or 50 images, the oldest
  images become opencode's removal note until 15 MiB remain.
- **Showing.** The chat's read row and `render.rs` show the read's text
  line. No picture is drawn.
- **Compaction** writes `[Read image/png: path]` in the transcript, counts
  2,500 tokens per image, and sends the summarizer no images.
- **Screenshots** are an internal skill (R4).

## Pull requests

In order, each on an `nth/<topic>` branch from a worktree, stacked where
one needs the last.

| #   | Branch                 | What                                                                                                                                                                  | Needs |
| --- | ---------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----- |
| 1   | `nth/chatgpt-models`   | ChatGPT's models from the backend's list instead of the catalogue filter: what the account is served, its context window and `input_modalities`. Stands on its own   |       |
| 2   | `nth/image-protocol`   | `Image`, `Output` for tools and `images` on `Message::ToolResult`; catalogue `modalities` and `ModelInfo::takes_images`; the three wires; fixtures with real-sized images | 1     |
| 3   | `nth/image-read`       | The read tool reads images: downscale, sidecar, gate; the chat row and `render.rs`                                                                                    | 2     |
| 4   | `nth/image-budget`     | The per-request bound, compaction's line and estimate, `session delete` removes the images                                                                           | 3     |
| 5   | `nth/screenshot-skill` | The internal screenshot skill                                                                                                                                         | 3     |

Later, each its own PR: PDF as a document block where the model takes it,
webfetch of an image URL.

## Side findings

- **ChatGPT lists models it cannot use.** nth offers 11 ChatGPT models;
  the Codex backend serves 3 to this account, and the other 8 answer `The
  '<model>' model is not supported when using Codex with a ChatGPT
  account`. The backend's list also gives a 272,000-token context where
  the catalogue says 400,000 or 1,050,000. PR 1 fixes both.
- **Zen's catalogue entry no longer matches by URL.** models.dev now gives
  Zen's `api` as `https://opencode.ai/inference/openai/v1` and Go's as
  `…/inference/go/openai/v1`, while nth's template uses
  `https://opencode.ai/zen/v1` and `…/zen/go/v1`. A Zen endpoint not
  named `opencode` therefore borrows its models' entries (no price),
  which the `zen/` models in this machine's listing show. Not
  investigated further.
