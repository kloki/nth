# Memory

|                |                                                                  |
| -------------- | ---------------------------------------------------------------- |
| **Researched** | 2026-10-19                                                       |
| **Question**   | What Claude Code and opencode keep for the model across sessions |
| **Decision**   | Open; a proposal is at the end                                   |

## Summary

"Memory" here is what the model knows at the start of a session that it
learned in an earlier one. Claude Code has two layers of it. The first is
instruction files that you write: CLAUDE.md, rules and imports. The second
is auto memory, which the model writes itself: a folder per project of
small typed files, with an index loaded into every session. opencode has
only the first layer, `AGENTS.md`. Every request for auto memory on its
issue tracker was closed, and memory is left to plugins. What opencode
does better is reloading: it watches the instruction files and sends the
model a diff when one changes mid-session.

nth has the first layer, as opencode does. It has no auto memory and does
not resolve `@path` imports.

## Claude Code

Sources: [memory](https://code.claude.com/docs/en/memory.md),
[sub-agents](https://code.claude.com/docs/en/sub-agents.md),
[context window](https://code.claude.com/docs/en/context-window.md).

### Instruction files (written by you)

| Where                                         | Scope                                                       |
| --------------------------------------------- | ----------------------------------------------------------- |
| `/etc/claude-code/CLAUDE.md` (Linux)          | Managed policy, cannot be excluded                          |
| `~/.claude/CLAUDE.md`, `~/.claude/rules/*.md` | You, every project                                          |
| `./CLAUDE.md` or `./.claude/CLAUDE.md`        | The project, checked in                                     |
| `./CLAUDE.local.md`                           | You, this project, gitignored                               |
| `.claude/rules/**/*.md`                       | The project; `paths:` limits a rule to the files it matches |

- **Load order:** files are concatenated from broadest to narrowest, never
  overridden. The walk goes from the working directory up through every
  parent directory, ordered from the root down.
- **Nested files:** the CLAUDE.md files under the working directory load
  only once a file in their folder is read, written or edited, including
  by a single-file `cat` in bash. A rule with `paths:` loads the same way
  when a file it matches is touched.
- **`@path` imports:** a path resolves relative to the importing file and
  imports go at most 4 levels deep. Code spans and code blocks are
  skipped. Importing a file outside the project asks for approval once
  per project.
- **AGENTS.md:** read only where there is no CLAUDE.md, the reverse of
  nth's preference.
- **Delivery and limits:** the files are sent as a user message after the
  system prompt, with HTML comments stripped. A file over 4 MiB is skipped,
  and the docs advise keeping each under 200 lines.
- **Commands:** `/memory` lists the files, opens one in your editor and
  toggles auto memory. `/init` writes a starter CLAUDE.md. A `#` prefix
  that quick-adds a line appears in third-party guides only, not in the
  current docs.

### Auto memory (written by the model)

- **Folder:** `~/.claude/projects/<project>/memory/`. The project is the
  git repository, so worktrees share one folder. The location can be
  changed with `autoMemoryDirectory`.
- **Shape:** one fact per file, with frontmatter giving its `name`,
  `description` and `type` (`user`, `feedback`, `project` or
  `reference`). `MEMORY.md` is the index: one line per file, with no
  content of its own.
- **Loading:** the first 200 lines or 25 KB of `MEMORY.md`, whichever
  comes first, go into every session, and the model is warned as the
  index nears that limit. The topic files are read when the model needs
  them, with the ordinary read tool.
- **Writing:** the model writes with its ordinary write and edit tools; no
  memory tool is involved. The system prompt says what is worth saving:
  what the code and git history cannot tell, and nothing CLAUDE.md
  already says. It also says to update an existing file rather than add a
  second, and to delete memories that turn out to be wrong. "Remember X"
  goes to auto memory, and "add this to CLAUDE.md" goes there.
- **Switches:** auto memory is on by default. It is turned off with
  `autoMemoryEnabled: false` or `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`.
- **Subagents:** they never see the main session's memory. A subagent with
  `memory: user`, `memory: project` or `memory: local` in its frontmatter
  gets its own `MEMORY.md`, under `agent-memory/<name>/` in that scope's
  folder.
- **After compaction:** the root CLAUDE.md, the rules without `paths:` and
  auto memory are read again from disk. Nested files come back when a file
  next to them is touched again.

### On the API (for comparison only)

The API's `memory` tool (`memory_20250818`, run on your side) has
`view`, `create`, `str_replace`, `insert`, `delete` and `rename` over a
`/memories` folder. It pairs with context editing, which tells the model to
save to memory before old tool results are cleared. Managed Agents add
memory stores, mounted at `/mnt/memory/<slug>/` with versioning, and
_dreaming_, which consolidates a store from up to 100 past sessions. A
Claude Code "auto dream" is reported by third parties but not in the docs.

## opencode

Full note: [opencode/05-memory.md](opencode/05-memory.md).

- **No memory feature.** There is no memory tool, auto memory, `remember`
  command, memory folder or experimental flag, in v1 or v2.
- **Instruction files:** `AGENTS.md` only: one global file and the walk up
  from the working directory, with `read` attaching nested files. The
  docs still promise a CLAUDE.md fallback and an `instructions` key, both
  removed with v1.
- **Live reload:** the instruction files are watched with a 100 ms
  debounce. The next step tells the model what changed, as a diff when that
  is shorter than the whole file, so an `AGENTS.md` the model just edited
  is in effect at once.
- **Who writes them:** `/init`, plus two model-specific prompts that ask
  the model to write findings back to `AGENTS.md`.
- **Issue tracker:** requests for auto memory (#20322, #21660, #41998,
  #52934) were closed by bots, and #48497 (`/teach`, `/recall`, `/memory`)
  has no maintainer reply. The ecosystem page points to the supermemory
  plugin.

## nth today

`nth-context/src/instructions/mod.rs`:

- **Global file:** the first of `~/.config/nth/AGENTS.md`,
  `~/.config/opencode/AGENTS.md` and `~/.claude/CLAUDE.md` that exists.
- **Project files:** the walk up to home, or to the project root outside
  home. In each folder `AGENTS.md` wins over `CLAUDE.md`.
- **Nested files:** attached by `read`, as opencode does.
- **No `@path` imports:** a global `~/.claude/CLAUDE.md` that is just
  `@RTK.md` reaches the model as that literal line.
- **No `CLAUDE.local.md`, no rules folder, no reloading mid-session and no
  auto memory.**

## Proposal for nth

Smallest first. Each is its own PR.

1. **`@path` imports** in instruction files, with Claude Code's rules:
   paths relative to the importing file, a leading `~` read through
   `expand_home`, at most 4 levels deep, code spans and code blocks
   skipped. No approval step: the files are yours. It fixes the
   `@RTK.md` case above and needs no new concept.
2. **Reload instruction files mid-turn, as opencode does.** Watch the
   loaded files, and between steps put what changed on the `Inbox` as a
   notice, a diff when that is shorter. Then an `AGENTS.md` the model
   edits is in effect for its next step, and so is a memory file (item
   3).
3. **Auto memory, in Claude Code's shape.**
   - **Folder:** `<nth_data>/projects/<main checkout>/memory/`, keyed by
     the main checkout as `work::root` is, so the session's worktrees
     share it.
   - **Files and index:** one file per fact with `name`, `description`
     and `type` frontmatter, read with `nth_context::frontmatter`, and a
     `MEMORY.md` index, capped and loaded into the system prompt with
     the other context.
   - **No memory tool:** the model writes with write and edit, and an
     internal `memory` skill teaches the format and what is worth
     saving. The prompt carries only the index and a pointer to the
     skill, which keeps the persona prompts free of it.
   - **Plan mode:** `plan::writable` must also let the memory folder
     through.
   - **Subagents:** none get it at first. An agent's `memory:`
     frontmatter can come later.
   - **Config:** `[memory] enabled` (default on), added to the `nth init`
     template and the `config` skill.
   - **Viewing:** a `/memory` command that opens the index in a file tab
     through `show_user`'s path.
4. **Not worth taking:**
   - path-scoped `.claude/rules/`, since nested `AGENTS.md` covers it;
   - `CLAUDE.local.md`, since a gitignored `AGENTS.md` above the project
     does the same job;
   - managed policy files;
   - the API memory tool, a tool doing what write and edit already do;
   - dreaming or consolidation, until the folders prove to grow stale.

**Open question: share Claude Code's folder?** nth already reads Claude
Code's skills, agents and `~/.claude/CLAUDE.md`. Reading and writing
`~/.claude/projects/<cwd with / as ->/memory/` would carry what Claude Code
has learned about a project into nth, and back again, since the format is
the same. The cost is tying nth to a path Claude Code can move. The middle
way: nth writes its own folder, and when that is empty, loads Claude Code's
index.
