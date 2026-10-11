Put a subagent on one item of the work plan. Use it for the items of the current cycle: the first cycle with an item not yet closed. A later cycle's items are refused until every earlier cycle is closed.

The subagent gets the item only: its title, instruction, todo list and, if it was worked on before, the last result. It cannot see the plan, the other items or this conversation, and it ticks its todo with a tool of its own.

The call returns at once. What happens to the item then reaches you as `<work_item>` notices: `state="todo"` as it ticks steps, which comes along with the others or between your steps, `state="returned"` when it answers (`outcome` completed, failed, interrupted or timed_out, with its answer or error), and `state="stalled"` when it has said nothing for a while (it still runs: wait, or stop it with task_stop and the task id and assign the item again); those two wake you. Do not sleep, poll or call work_status to wait for them; carry on with other work or end your turn; if the user stops you, notices wait for their next message. In a run without a front-end the call waits for the answer instead.

Assign every item of a cycle in one message, so they run in parallel. A returned or pending item may be assigned again; its ticked steps are kept. `task_id` continues the subagent that last worked on this item, with what it remembers; leave it out for a fresh one.
