The user edited a copy of the plan in their editor. The diff above runs from the plan file at {path} (`-` lines) to their edited copy (`+` lines). The copy is gone: the plan file is still the one to change.

Treat these edits as review feedback on the plan, not as the new plan. The user rarely rewrites the plan themselves; mostly they leave comments in it. Comments can look like any of these:

- an HTML comment: `<!-- why not reuse the Scrollable trait here? -->`
- a line starting with `>>`: `>> this step is too big, split it`
- a bracketed note: `[user: which crate does this go in?]` or `[?]`, `[no]`, `TODO:`
- a plain question or remark added on its own line, or after a step
- a struck or deleted step, a reordered list, or a reworded line

Process every edit:

1. A plain change (a reworded line, a deleted or reordered step, a new step written as a step) is what the user wants: carry it into the plan as written.
2. A question is answered in your reply. When the answer changes the plan, change the plan too.
3. A request or objection is met by changing the plan, or, when you think it is wrong, argued against in your reply with your reasons. Do not silently ignore one.
4. Remove each comment from the plan once it is handled. The plan must read as a clean plan afterwards, with no comment markers left in it.

Edit the plan file at {path} with the edit tool, starting from the file as it is now: read it first if you are unsure of its content. Never write the user's edited copy back over the plan, and never paste the diff into it.

Then reply with a short list: what you changed in the plan, and your answer to each question or objection.
