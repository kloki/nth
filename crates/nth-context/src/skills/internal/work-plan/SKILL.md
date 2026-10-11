---
name: work-plan
description: How to run a large task as an nth work plan (work_item, work_assign, work_close, work_status) - when one is worth it, how to cut it into cycles and items, write briefs a subagent can act on alone, manage its worktrees, branches, merges and pull requests, test and review the result, and judge what comes back. Load it before writing the first item of a plan.
---

A work plan is for work too big to do well in one conversation: parts that each fit one subagent, run in cycles. You are the orchestrator. You plan, brief, judge and decide, and you own the git side: worktrees, branches, merges and pull requests. The subagents do the work. If you can finish the change yourself in a handful of steps, do not make a plan.

## Cut the work into cycles

A cycle is a barrier: its items run in parallel, and the next cycle starts only when you have closed every one of them.

- Put items in the same cycle only when they do not depend on each other.
- A subagent works in the directory you are in when you assign it. Items that share a checkout must **not edit the same files**, or they overwrite each other. When parallel items would collide, or each should land as its own branch, give each item a worktree (see below).
- Work that needs another item's output goes in a later cycle.
- Keep the number of cycles small; each one costs a full round of waiting and judging.

A plan usually looks like this:

1. **Survey** (optional): read-only items that map out what the build cycles need, when you do not know the code well enough to brief them yet.
2. **Build**: one or more cycles of items that make the change.
3. **Test**: after each build cycle that adds something substantial, a dedicated cycle whose items only write and run tests for what was just built, one item per area. Do not rely on the builders' own checks: someone who did not write the code tests it against the brief. A failing test goes back to a build item, rewritten and assigned again, before you move on.
4. **Integrate**: when items ran on their own branches, merge them before the work that needs the whole change (see "Manage branches, merges and pull requests").
5. **Review**: the last cycle is always a bug hunt over the whole change, by subagents that did not write it. One item per angle: correctness and edge cases, error handling and failure paths, tests that are missing or assert too little, consistency with the codebase's conventions. Tell each to look for real bugs, reproduce every one it reports, and give file:line and the failing input. Then check each finding yourself, fix the real ones (yourself, or as fix items in one more cycle), and run the full test suite, formatter and linter before you call the plan done.
6. **Deliver**: push and open the pull requests the plan set out to make, or tell the user which branch holds the result.

Write the later cycles' items when you know enough to brief them, and rewrite them with what earlier cycles taught you before assigning them.

## Size an item

One coherent change that a subagent can finish within its step limit, with a todo of about three to eight steps. When an item needs more, split it into items or cycles.

## Write the brief

The subagent sees its item and nothing else: not the plan, not the other items, not this conversation. Every brief stands on its own:

- **Goal**: what to change and why, in a sentence or two.
- **Where**: the files, functions and types involved. Give the paths you already found, so it does not search for them again.
- **Constraints**: conventions to follow, what not to touch (the files other items in the same checkout own), decisions already made.
- **Verify**: the exact command that shows it works, such as one test or the crate's test suite.
- **Report**: what to say in its answer: files changed, how it verified them, what it could not do.

Seed the todo with the steps you expect; the subagent ticks them and adds its own.

## Pick the agent

`work_assign` takes `subagent_type`, an agent from your agents list. Use `general` for items that change code and `explore` for read-only investigation; a project agent (an opencode-style markdown agent with `mode: subagent`) when one fits the item better, such as a reviewer whose tools cannot write.

## Give items their own worktrees

To put an item in its own checkout, call enter_worktree with a name for it, assign the item, and move on to the next item's worktree; exit_worktree takes you back to the main checkout. Each enter_worktree goes in a step of its own, before that item's work_assign, since the subagent starts wherever you are when you assign it. The assignments still return at once, so the items run in parallel. Say in the brief that the item works on its worktree's branch and should commit there.

The plan stays in the main checkout wherever you are. Worktrees are refused in plan mode.

## Manage branches, merges and pull requests

Git is part of the plan, not an afterthought. Decide the layout before the first build cycle and keep it current:

- **Write it down.** In `plan.md`, keep a section listing each branch: what it holds, what it is based on, its worktree, and its pull request once there is one. A resumed session, and the user, read the state of the work from it.
- **Name and base each branch.** A worktree's branch is its name; pick names that say what they hold, under one prefix for the plan (such as `retry/backoff`, `retry/config`). enter_worktree branches from the main checkout's HEAD. To base a branch on another, such as the previous cycle's result, create it first with `git branch <name> <base>`, then enter_worktree with that name.
- **Subagents only commit.** Tell every item in a worktree to commit its work on its branch, and never to push, merge, rebase or switch branches. Everything else in git is yours.
- **Check a branch before closing its item.** Review `git log <base>..<branch>` and `git diff <base>...<branch>`. Uncommitted changes in its worktree mean the item is not done.
- **Merge deliberately.** Merge item branches into the branch the plan delivers, one at a time, in the order that conflicts least, and run the tests after each merge. Resolve a small conflict yourself; give a large one to a merge item whose brief names both branches and what each was for. The test and review cycles run on the merged result, never on a single item's branch.
- **Pull requests.** When the project delivers through pull requests, plan them up front: one per branch the user should review, sized to be read in one sitting, stacked when one builds on another (each opened against the branch below it). Push and open them with git and gh; follow what the project's instructions say about branches and PR descriptions. Pushing and opening pull requests is visible to others: do it when the user asked for it, otherwise ask first. A fix found later goes on the branch it belongs to, then rebase the branches stacked above it with `git rebase --update-refs`, and push them all.
- **Clean up.** Once a branch is merged, remove its worktree and branch (`git worktree remove .nth/worktrees/<name>`, `git branch -d <name>`). Never delete a branch that is not merged, or one with an open pull request.

## Run it

1. Write `plan.md` in the plan's folder (work_status names it): a `# Title` line, then the goal and the branch layout. The user sees it above the cycles.
2. Write the current cycle's items with work_item.
3. Assign every item of the cycle with work_assign, all in one message, so they run in parallel (items in their own worktrees are assigned one after another, each after its enter_worktree).
4. End your turn. Each item reaches you as a `<work_item>` notice when it returns or stalls. Never sleep, poll, or call work_status to wait.

## Judge what comes back

- Read the answer, then check the work as far as you need to: the diff, the verification command. Close an item as done only once you checked it.
- When it falls short, rewrite the brief with work_item and assign it again. Ticked steps and the last result are kept for the next subagent; pass `task_id` to continue the same subagent when what it remembers helps.
- When it cannot be done, close it as failed and plan around it.
- When an item stalls, wait, or stop it with task_stop and assign it again.

When you resume a session that has a plan, call work_status first, then check the branches `plan.md` lists against `git worktree list` and `git branch`.
