---
name: research-opencode
description: Research the opencode coding harness (github.com/anomalyco/opencode) — its source code, features and design — against a pinned local clone, writing findings in one standard note format under docs/research/opencode/. Use when the user says "research opencode", "how does opencode do X", "study opencode's <feature>", "look at opencode's source for...", "compare nth with opencode", or asks what to learn from opencode for building the nth harness.
---

# Research opencode

`nth` is a Rust coding harness in the making. opencode is the reference we
study. Every note must be **citable** (tied to a commit) and **comparable**
(same structure), so they stay useful while nth is designed.

## Paths

| What                         | Where                                                  |
| ---------------------------- | ------------------------------------------------------ |
| Reference clone              | `refs/opencode` (gitignored)                           |
| Sync script                  | `.claude/skills/research-opencode/scripts/sync-ref.sh` |
| Note template (the standard) | `.claude/skills/research-opencode/templates/topic.md`  |
| Index template               | `.claude/skills/research-opencode/templates/index.md`  |
| Output                       | `docs/research/opencode/`                              |

## Workflow

1. **Sync the reference.** Run `scripts/sync-ref.sh`. It clones on first run
   and otherwise keeps the current pin. Only pass `--repin` when the user asks
   to move to a newer opencode. The last output line is the pinned SHA.
2. **Ensure the index.** If `docs/research/opencode/README.md` is missing,
   create it from `templates/index.md` and fill in the SHA and date. After a
   re-pin, update the SHA there.
3. **Pick the topic.** The user's request, or else the first unchecked item
   in the index coverage checklist. One note per topic; if a note already
   exists, update it instead of creating a second one.
4. **Research source first.**
   - Orient: `refs/opencode/README.md`, `package.json` workspaces, `packages/`.
   - Find entry points with grep/glob, then trace the code path end to end.
   - Read the actual files — don't guess from names.
   - Secondary: the repo's docs and opencode.ai docs for user-facing
     behaviour. Label every point `[source]` or `[docs]`.
   - For a broad topic, fan out Explore agents over separate packages.
5. **Write the note** at `docs/research/opencode/<NN>-<kebab-topic>.md`
   (`NN` = next number in the index), following `templates/topic.md` exactly:
   same headings, same order. Leave a section as "n/a" rather than dropping it.
6. **Update the index**: add a row to Notes, tick the checklist item.

## Rules

- **Cite everything.** Each claim about code gets `path:line` and a permalink:
  `https://github.com/anomalyco/opencode/blob/<SHA>/<path>#L<line>`. Never
  link to `main` — it drifts.
- **Observed vs inferred.** Keep what the code does apart from what you
  think the intent is.
- **Short excerpts only** (≤ 15 lines). Link to the rest.
- **Takeaways for nth** are concrete and in Rust/Tokio terms — what to copy,
  what to avoid, what to do differently. This is the section that matters most.
- Scannable over complete: tables, trees and lists before prose.
- Never modify files in `refs/opencode`.
