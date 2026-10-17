# Images

|                     |                                            |
| ------------------- | ------------------------------------------ |
| **opencode commit** | `80fb31ebb7e8dfa5c26fc51ca81da8e06b618a52`                                       |
| **Researched**      | 2026-10-17                                 |
| **Sources**         | source                                     |

## Summary

An image reaches the model as a `data:` URI file part: from the read tool,
from an attachment in the prompt, or from an MCP tool. Before every request
an image the model cannot take becomes an error text, and when the images
of one request pass 25 MiB the oldest become a removal note until 15 MiB
remain. Nothing is downscaled. Compaction counts an image as 1,500 tokens
and writes it as an `[Attached …]` line.

## User-facing behaviour

- `read` on a png, jpeg, gif, webp or pdf shows it to the model; any other
  binary is `Cannot read binary file`. Files over 20 MiB are refused.
  [source]
- The prompt takes a pasted file path or list of paths as attachments, and
  a clipboard image (ctrl+v) as a PNG; up to three thumbnails show above
  the prompt when `prompt.image_preview` is on. [source]
- A model that cannot take images gets `ERROR: Cannot read "<name>" (this
  model does not support image input). Inform the user.` in the image's
  place. [source]
- On xAI, an image that is not png, jpeg or webp is treated as unsupported.
  [source]

## Where it lives

```
packages/core/src/
├── tool/read-filesystem.ts      # MEDIA_MIMES, MAX_MEDIA_INGEST_BYTES
├── tool/plugin/read.ts          # toModelContent: text + file part
├── tool/mcp.ts                  # MCP image content as file parts
├── session/model-request.ts     # unsupportedMedia, boundImages
└── session/compaction.ts        # IMAGE_TOKEN_ESTIMATE, [Attached …] lines
packages/tui/src/
├── clipboard.ts                 # clipboard read preferring image/png
└── component/prompt/local-attachment.ts   # pasted paths as attachments
```

## How it works

1. **Read** — media types are png, jpeg, gif, webp and pdf
   ([`read-filesystem.ts:21`](https://github.com/anomalyco/opencode/blob/80fb31ebb7e8dfa5c26fc51ca81da8e06b618a52/packages/core/src/tool/read-filesystem.ts#L21)),
   capped at 20 MiB
   ([`:16`](https://github.com/anomalyco/opencode/blob/80fb31ebb7e8dfa5c26fc51ca81da8e06b618a52/packages/core/src/tool/read-filesystem.ts#L16)). The
   result is the text `Image read successfully` and a file part with the
   base64 `data:` URI and the path as its name
   ([`read.ts:175`](https://github.com/anomalyco/opencode/blob/80fb31ebb7e8dfa5c26fc51ca81da8e06b618a52/packages/core/src/tool/plugin/read.ts#L175)).
2. **Unsupported media** — each file part's mime maps to a modality
   (image, audio, video, pdf); one missing from the model's
   `capabilities.input`, or a non-png/jpeg/webp image on xAI, is replaced
   by the error text
   ([`model-request.ts:132`](https://github.com/anomalyco/opencode/blob/80fb31ebb7e8dfa5c26fc51ca81da8e06b618a52/packages/core/src/session/model-request.ts#L132)).
3. **Image budget** — the base64 bytes of every image in the request,
   in user messages and tool results alike, are summed; over 25 MiB,
   images are replaced oldest first by the removal note until 15 MiB
   remain ([`model-request.ts:189`](https://github.com/anomalyco/opencode/blob/80fb31ebb7e8dfa5c26fc51ca81da8e06b618a52/packages/core/src/session/model-request.ts#L189)).
   Both passes run on every request
   ([`:297`](https://github.com/anomalyco/opencode/blob/80fb31ebb7e8dfa5c26fc51ca81da8e06b618a52/packages/core/src/session/model-request.ts#L297)) and on
   the compaction request
   ([`compaction.ts:862`](https://github.com/anomalyco/opencode/blob/80fb31ebb7e8dfa5c26fc51ca81da8e06b618a52/packages/core/src/session/compaction.ts#L862)).
4. **Compaction** — an image counts as 1,500 tokens, a PDF 2,000
   ([`compaction.ts:101`](https://github.com/anomalyco/opencode/blob/80fb31ebb7e8dfa5c26fc51ca81da8e06b618a52/packages/core/src/session/compaction.ts#L101)),
   and the transcript the summarizer reads has `[Attached image/png:
   name]` in its place
   ([`:766`](https://github.com/anomalyco/opencode/blob/80fb31ebb7e8dfa5c26fc51ca81da8e06b618a52/packages/core/src/session/compaction.ts#L766)).
5. **ChatGPT** — `fetchModels` reads `input_modalities` from the
   backend's model list
   ([`chatgpt.ts:404`](https://github.com/anomalyco/opencode/blob/80fb31ebb7e8dfa5c26fc51ca81da8e06b618a52/packages/core/src/plugin/provider/chatgpt.ts#L404)),
   but the call that would refresh it is commented out at this commit
   ([`:324`](https://github.com/anomalyco/opencode/blob/80fb31ebb7e8dfa5c26fc51ca81da8e06b618a52/packages/core/src/plugin/provider/chatgpt.ts#L324)).

## Key data types / interfaces

- A file part: `{ type: "file", uri: "data:<mime>;base64,…", mime, name }`,
  the same in a user message and in a tool result's content.
- `Model.Capabilities.input`: the modalities from models.dev, which the
  unsupported-media pass reads.

## What nth takes from it

nth's plan is [images.md](../images.md). It takes the read tool's media
types (re-encoding GIF instead of the xAI special case), the unsupported
error text, the oldest-first image budget with its removal note, the
compaction line and a per-image token estimate. It leaves out prompt
attachments and the clipboard, since images reach nth's model only
through tools. Unlike opencode, nth downscales on read: R6 in the plan
measured 1920 px on the longest side as the size most models still read.
