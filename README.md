# nth

> I've asked you for the n-th time!

I wanted to tweak my coding harness more than I could, so I product engineered it myself.

Whenever I did not know what to do, I copied [opencode](https://github.com/anomalyco/opencode). So all props to them.

# Install

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/kloki/nth/releases/latest/download/nth-installer.sh | sh
```

## Optional

nth integrates with these tools, and uses them automatically when they are installed:

- [rtk](https://github.com/rtk-ai/rtk) compresses the output of common commands for the model.
- [agent-browser](https://github.com/vercel-labs/agent-browser) drives a browser from the `browser` skill.

# Set up

```
nth init
```

## Setup providers

- Set up a provider in the config. There are examples for opencode go and zen
- Set your API key as an environment variable: `  export OPENCODE_API_KEY=...`

## Setup subscriptions

Only Codex is supported.

- run `/connect` in nth

# Features

## Tabbed view

nth supports a tabbed view for main chat, plan, subagents, etc. The agent is aware of this and can control it.

## Plan view

- When the plan updates, show a diff view
- Place a comment directly in the plan to be processed
- Finalize with /approve

## Worktree

- nth will actually use worktrees when making features
- `/setup-worktrees` to improve it for your project

## Workplan

- Complete a task in cycles
- The model will dynamically update the plan based on its findings
- nth will automatically use it when needed

## Profiles

- Profiles configure different models for plan/act/subagent
- Defined in your config
- ctrl-p toggles between them
