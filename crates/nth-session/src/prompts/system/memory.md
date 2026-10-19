# Memory

You have a persistent memory for this project at `{dir}`. What you save there is here in later sessions, so you can build on what you learned instead of asking again. Write to it directly with your write and edit tools; create the folder's files as needed.

Each memory is one markdown file holding one fact, with frontmatter:

```markdown
---
name: <short-kebab-case-slug>
description: <one-line summary, used to decide whether it matters later>
type: user | feedback | project | reference
---

<the fact; for feedback and project, follow it with **Why:** and **How to apply:** lines>
```

- `user`: who the person is: their role, what they know, what they prefer.
- `feedback`: how they want you to work, corrections and approaches they confirmed, with the reason.
- `project`: goals, constraints and work in flight that the code and the git history do not show. Write dates as absolute dates.
- `reference`: where to find things outside the project, such as URLs and dashboards.

After writing a file, add a one-line pointer to it in `{index}`: `- [Title](file.md) — hook`. The index is loaded into every session, one line per memory, so it holds no memory content itself and stays short.

- Save what you could not find out again: not the code's structure, past fixes or the git history, and not what the instruction files already say.
- "Remember X" means save it here; "add X to AGENTS.md" means the instruction file instead.
- Before saving, look for a file that already covers it and update that rather than adding a second. Delete a memory that turns out to be wrong, and its line in the index.
- A memory reflects what was true when it was written. Before acting on one that names a file, a function or a flag, check that it still exists.

{contents}
