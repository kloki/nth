---
name: opencode-changes
description: Review what changed in opencode (github.com/anomalyco/opencode) since the pinned reference commit — list the new features and notable fixes, each linked to its commit, judged for whether nth should adopt them — then move the pin in docs/research/opencode/README.md to the new commit. Use when the user says "what's new in opencode", "opencode changes since the pin", "new opencode features", "anything to adopt from opencode", "update/bump the opencode ref" or "re-pin opencode".
---

# opencode changes

nth copies opencode where it has no strong opinion, against a pinned clone.
This skill is the sweep over what opencode did since the pin: one report per
sweep, each item linked to its commit and given a verdict, so the user can
pick what to adopt. A deep look at one item is a `research-opencode` note,
not this.

## Paths

| What             | Where                                                         |
| ---------------- | ------------------------------------------------------------- |
| Reference clone  | `refs/opencode` in the main checkout (gitignored)              |
| Pin              | `docs/research/opencode/README.md` (**Pinned commit**)        |
| Candidate list   | `.claude/skills/opencode-changes/scripts/new-commits.sh`      |
| Re-pin           | `.claude/skills/research-opencode/scripts/sync-ref.sh --repin` |
| Report template  | `.claude/skills/opencode-changes/templates/changes.md`        |
| Output           | `docs/research/opencode/changes/<YYYY-MM-DD>.md`              |

Both scripts find the clone through the main checkout, so they work from a
worktree. Run `sync-ref.sh` once without flags if the clone is missing.

## Workflow

1. **Read the pin** (`OLD`) from the README. Stop if `git -C refs/opencode
   status --porcelain` shows changes.
2. **Fetch without moving**: `git -C refs/opencode fetch origin`, and
   `NEW=$(git -C refs/opencode rev-parse origin/HEAD)`. If `OLD == NEW`, say
   there is nothing new and stop.
3. **List candidates**: `scripts/new-commits.sh OLD NEW`. It walks the
   first-parent history, drops noise (chores, docs, CI, releases, and the
   product-only packages: console, stats, web, desktop, app ...), and prints
   TSV: sha, date, subject, PR, packages. It also prints to stderr every
   merge that brought in a branch of more than 20 commits.
4. **Branch merges**: for each one, `scripts/new-commits.sh --merge SHA`
   lists the branch's own candidates. A big one (a version promotion, a
   rewrite) is too large to triage commit by commit: take its `feat`
   commits, group them by scope, and fan out Explore agents, one per area
   (core, tui, ai/providers, cli, plugin, ...), each returning the
   user-facing features of its area with the commit that introduced each.
   Say in the Summary that the range carries such a merge.
5. **Triage** each candidate from its subject, `git show --stat`, and the
   diff where the subject is unclear. Keep:
   - **Features**: a new user-facing capability, tool, command, config key,
     TUI behaviour, provider or prompt change.
   - **Fixes worth knowing**: only in areas nth copied (provider wires,
     models.dev, tools, edit/patch, LSP, formatters, skills and agents
     discovery, plan mode, compaction, subagents), since nth may share
     the bug. Name the nth file that may.
   Drop the rest without listing it.
6. **Judge against nth**: grep nth's `crates/`, `docs/design.md` and
   `docs/todo.md` for whether nth has it. Verdict:
   - **adopt**: fits nth and nth lacks it;
   - **consider**: useful but needs design, or nth does it differently;
   - **skip**: a non-goal in `docs/design.md` (client/server, web, desktop,
     IDE plugins), opencode's products (Zen, Go, console), or nth already
     has it. Keep skip short; it records that the item was seen.
7. **Write the report** at `docs/research/opencode/changes/<today>.md`
   from `templates/changes.md`: same headings, same order, "none" for an
   empty section. For **Stale notes**, intersect the `packages/...` paths
   the notes in `docs/research/opencode/` cite with
   `git -C refs/opencode diff --name-only OLD NEW`.
8. **Re-pin**: run `sync-ref.sh --repin`, then in the README set
   **Pinned commit** to the full new SHA and **Pinned on** to today, and add
   a row to **Change reviews**. The formatter hook reflows markdown tables:
   check `git diff --stat docs/` afterwards.
9. **Report back**: the adopt and consider items as a short list, each with
   its commit link, and the path of the report. Do not edit `docs/todo.md`
   unless the user picks items for it.

## Rules

- **Link every item** to its commit by full SHA:
  `https://github.com/anomalyco/opencode/commit/<SHA>`, with the PR link
  when the subject names one. Never link to `dev` or `main`; they drift.
- **One line per item.** What it does, what nth has, why the verdict.
- **Observed vs inferred.** What the diff does is not why it was done.
- **Group related commits** (a feature and its follow-up fixes) into one row
  listing every commit.
- Never modify files in `refs/opencode`.
