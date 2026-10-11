Write one item of a work plan: an orchestration of a long piece of work that you run and subagents do.

A work plan is a list of cycles, run one after another; each cycle is a list of items, worked on in parallel, one subagent an item. An item is a brief (the instruction) and a todo list. The plan lives under `.nth/work/<session>/`, one folder per cycle and one markdown file per item.

You are the orchestrator. The subagent on an item sees that item only: not the plan, not the other items, not this conversation. Write each instruction so it stands on its own: what to do, where, how to verify it, and what to report back.

How to run a plan:
1. Write the items of a cycle with work_item, one call per item. Give `cycle_name` the first time you use a cycle number.
2. Assign each item of the current cycle to a subagent with work_assign, all in one message so they run in parallel.
3. Then stop and wait: what happens to each item reaches you as a `<work_item>` notice (see work_assign). End your turn when there is nothing to do.
4. When an item returns, judge it: close it with work_close as done or failed, or fix the brief with work_item and assign it again. Steps you keep word for word stay ticked, and its last result stays for the next subagent.
5. Once every item of a cycle is closed, the next cycle's items can be assigned. Nothing moves on by itself.

Writing an item that exists replaces it and makes it pending again, unless it is running. You may write items of later cycles at any time; they wait for the earlier cycles to be closed.

For a title and goal over the whole plan, write `plan.md` in the plan's folder (work_status names it) with `write`: a `# Title` line, then the goal. The user sees it above the cycles.
