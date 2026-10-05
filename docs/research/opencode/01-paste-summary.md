# Paste summary

|                     |                                                                   |
| ------------------- | ----------------------------------------------------------------- |
| **opencode commit** | `03e67171ab2dc1e7f16e8cebfbc7f778f61b89f0`                        |
| **Researched**      | 2026-10-05                                                        |
| **Sources**         | source · docs                                                     |

## Summary

A large bracketed paste is collapsed in the prompt to a virtual
`[Pasted ~N lines]` token; the real text is kept in a prompt part and
swapped back on submit. The token is an **extmark**, so edits around it move
it and an edit across it destroys it. A `kv`-persisted toggle
(`app.toggle.paste_summary`, default on) turns the collapse off.

## User-facing behaviour

- Pasting three or more lines, or more than 150 characters, shows
  `[Pasted ~N lines]` where the text would be; anything smaller is inserted
  verbatim. [source]
- The token is styled apart from typed text (the `extmark.paste` style).
  [source]
- Submitting sends the full pasted text, not the token. [source]
- `app.toggle.paste_summary` ("Enable paste summary" / "Disable paste
  summary") flips it and the choice is remembered across launches. [source]
- It is on by default unless `experimental.disable_paste_summary` is set in
  config. [source]
- The command ships with no default keybind (`none`). [docs]

## Where it lives

```
packages/tui/src/
├── component/prompt/
│   └── index.tsx      # onPaste handler → pasteInputText → pasteText; expand on submit
├── prompt/
│   └── part.ts        # expandTrackedPastedText / expandPastedTextPlaceholders
├── app.tsx            # pasteSummaryEnabled signal + app.toggle.paste_summary
└── config/
    └── keybind.ts     # app_toggle_paste_summary keybind (default: none)
packages/core/src/v1/config/config.ts    # experimental.disable_paste_summary
```

## How it works

1. **Bracketed paste** — the textarea's `onPaste` normalizes `\r\n` then lone
   `\r` to `\n`, drops an empty paste, and calls `pasteInputText`
   ([`index.tsx:1396`](https://github.com/anomalyco/opencode/blob/03e67171ab2dc1e7f16e8cebfbc7f778f61b89f0/packages/tui/src/component/prompt/index.tsx#L1396)).
2. **Classify** — `pasteInputText` trims, tries local-file / URL handling
   first, then computes `lineCount = newlines + 1`. If `lineCount >= 3 ||
   length > 150` and the toggle is on it calls `pasteText(content, "[Pasted
   ~N lines]")`; otherwise it inserts the raw text
   ([`index.tsx:1206`](https://github.com/anomalyco/opencode/blob/03e67171ab2dc1e7f16e8cebfbc7f778f61b89f0/packages/tui/src/component/prompt/index.tsx#L1206)).
3. **Insert** — `pasteText` inserts the placeholder plus a trailing space,
   creates a `virtual: true` extmark over it, and appends a `prompt.parts`
   entry holding the real text keyed to that range
   ([`index.tsx:1149`](https://github.com/anomalyco/opencode/blob/03e67171ab2dc1e7f16e8cebfbc7f778f61b89f0/packages/tui/src/component/prompt/index.tsx#L1149)).
4. **Replay** — after a reload, each part is turned back into an extmark,
   text parts using `pasteStyleId`
   ([`index.tsx:678`](https://github.com/anomalyco/opencode/blob/03e67171ab2dc1e7f16e8cebfbc7f778f61b89f0/packages/tui/src/component/prompt/index.tsx#L678)).
5. **Submit** — `expandTrackedPastedText(input, ranges)` replaces every
   prompt-part range with the part's real text; text parts are then filtered
   out so the expanded copy isn't sent twice
   ([`index.tsx:1026`](https://github.com/anomalyco/opencode/blob/03e67171ab2dc1e7f16e8cebfbc7f778f61b89f0/packages/tui/src/component/prompt/index.tsx#L1026)).
   Ranges are sorted descending and spliced with `displaySlice`, so wide
   characters don't skew the offsets
   ([`part.ts:24`](https://github.com/anomalyco/opencode/blob/03e67171ab2dc1e7f16e8cebfbc7f778f61b89f0/packages/tui/src/prompt/part.ts#L24)).

## Key data types / interfaces

- **Prompt part** (pasted text):
  `{ type: "text", text, source: { text: { start, end, value } } }` — `value`
  is the placeholder shown, `text` is the real content.
- **Extmark**: `input.extmarks.create({ start, end, virtual: true, styleId,
  typeId })`; `typeId` is one type for all prompt parts, `styleId` picks the
  paste colour.
- **Toggle**: `kv` key `paste_summary_enabled`; config
  `experimental.disable_paste_summary`
  ([`config.ts:171`](https://github.com/anomalyco/opencode/blob/03e67171ab2dc1e7f16e8cebfbc7f778f61b89f0/packages/core/src/v1/config/config.ts#L171)).

## Design decisions & trade-offs

- **Observed:** the placeholder is an extmark, not literal text — the editor
  buffer holds `[Pasted ~N lines]`, so positions track edits and the model
  never sees the token.
- **Observed:** `~` in the label — the count is approximate (`\n` count + 1),
  so it reads as "about N lines".
- **Inferred:** the trailing space after the placeholder stops adjacent typed
  text gluing to the content once expanded.
- **Observed:** expansion sorts by range and slices by display width, so a
  paste beside wide characters still lands at the right offsets.
- **Observed:** the extmark layer is heavy — an id→part index, a part list,
  replay on reload — for what is, in nth's plain-buffer prompt, a couple of
  byte ranges.

## Takeaways for nth

- **Copy:** the threshold (`>= 3` lines or `> 150` bytes) and the
  collapse-then-expand model; expand at submit so the model, the transcript
  and history see the real text.
- **Avoid:** the extmark/virtual-text layer. nth's `Prompt` is a `String` +
  byte cursor, so a small side table of placeholder ranges (shifted on every
  edit) is the whole mechanism.
- **Do differently:** hardcode it on — nth keeps opinions in code
  (`docs/design.md`), so no `kv` key and no `app.toggle` command; and store
  the expanded text in history rather than carrying prompt parts.

## Open questions

- opencode's prompt history keeps `store.prompt` (parts included), so a
  recall likely restores the token and its parts; nth instead stores the
  expanded text. Whether recall should re-collapse is a follow-up note.
- nth draws the prompt as plain spans today, so the placeholder is uncoloured;
  styling it (dim) would need the wrap pass to emit styled segments.
