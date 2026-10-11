Put a subagent on one item of the work plan. Use it for the items of the current cycle: the first cycle with an item not yet closed. A later cycle's items are refused until every earlier cycle is closed.

The subagent gets the item only: its title, instruction, todo list and, if it was worked on before, the last result. It cannot see the plan, the other items or this conversation, and it ticks its todo with a tool of its own.

The call returns at once. Every change to the item then reaches you as a `<work_item>` notice, which wakes you: `state="todo"` as it ticks steps, `state="returned"` when it answers (`outcome` completed, failed, interrupted or timed_out, with its answer or error), and `state="stalled"` when it has said nothing for a while (it still runs: look at its tab, stop it with task_stop and the task id, or wait). Do not sleep, poll or call work_status to wait for them; carry on with other work or end your turn. In a run without a front-end the call waits for the answer instead.

Assign every item of a cycle in one message, so they run in parallel. A returned or pending item may be assigned again; its ticked steps are kept. `task_id` continues the subagent that last worked on this item, with what it remembers; leave it out for a fresh one.
