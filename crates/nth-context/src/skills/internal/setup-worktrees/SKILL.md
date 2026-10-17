---
name: setup-worktrees
description: Write or update the project's .nth/worktree-setup, the script nth runs in every new worktree so it builds and runs without a full reinstall or rebuild - for any language, from Rust and Node to Python, Go and polyglot monorepos. Load it when asked to set up worktrees, or when a fresh worktree does not build.
---

Write or update `.nth/worktree-setup` for this repository.

The goal is a script that turns a fresh worktree into one that builds, tests and runs, as fast as possible. A subagent put in a worktree that does not build wastes its whole turn; one that waits ten minutes for a cold build wastes most of it.

User-provided focus or constraints (honor these):
$ARGUMENTS

## What nth does with it

When `enter_worktree` creates a worktree (under `.nth/worktrees/<name>`, on branch `<name>`), it runs the main checkout's `.nth/worktree-setup`:

- with `bash`, so the script needs no executable bit
- in the new worktree as working directory
- with `NTH_WORKTREE_BASE` set to the main checkout and `NTH_WORKTREE_PATH` to the new worktree
- killed after ten minutes; the tail of its stdout and stderr and its exit code go back to the model

It runs only when the worktree is created, never when one is entered again. The script is read from the main checkout, but the worktree is a fresh checkout of a branch, so it must be committed to be there for everyone.

## How to investigate

- manifests and lockfiles at the root and in every workspace member (`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, `Gemfile`, `mix.exs`, `build.gradle`, `pom.xml`, `composer.json`, `*.csproj`, …)
- how the project says to set up: `README*`, `CONTRIBUTING*`, `Makefile`, `justfile`, `Taskfile.yml`, `.tool-versions`, `mise.toml`, devcontainer config, and the CI workflows, which are the setup that is known to work
- what a fresh checkout lacks: `git status --ignored --short` in the main checkout lists ignored files; separate build output and caches (rebuilt or shared) from files a checkout needs to run, such as `.env` or local config
- `.gitmodules`, and `.gitattributes` for git-lfs
- codegen a build needs before its first compile (protobuf, ORM clients, GraphQL types)
- an existing `.nth/worktree-setup`: improve it in place rather than rewriting it

## Script rules

- Start with `#!/usr/bin/env bash` and `set -euo pipefail`, and read both paths with `${NTH_WORKTREE_BASE:?}` and `${NTH_WORKTREE_PATH:?}`.
- Guard each step by what calls for it (the manifest or lockfile is there, the tool is on `PATH`), so a polyglot repo gets one section per ecosystem and a missing tool skips its step with a message rather than failing the rest.
- Be idempotent and never overwrite what the worktree already has: running it twice must be harmless.
- Never prompt: use locked, frozen, non-interactive installs. Nobody is there to answer, and a script that waits runs into the timeout.
- Be fast. Reuse the main checkout's work where it is safe: copy build output with `cp -a --reflink=auto` (keeping mtimes, which incremental builds compare; reflinks share blocks on filesystems that support them), and rely on the package managers' shared global caches for dependencies.
- Never copy anything that holds absolute paths into the main checkout (virtualenvs, most `node_modules`): create those fresh instead.
- Echo one short line per step, so the tool result says what happened.
- Exit non-zero only when the worktree really cannot build; a skipped optional step is not a failure.

## Per ecosystem

| Detect                               | Do                                                                                                                                                          |
| ------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Cargo.toml`                         | Seed `target/` from the main checkout (see the example below). Registry sources live in the shared `CARGO_HOME`, so only the worktree's own crates rebuild. |
| `pnpm-lock.yaml`                     | `pnpm install --frozen-lockfile`: the global store hardlinks, so it is fast.                                                                                |
| `package-lock.json`                  | `npm ci`                                                                                                                                                    |
| `yarn.lock`                          | `yarn install --immutable` (Berry) or `yarn install --frozen-lockfile` (classic)                                                                            |
| `bun.lock` / `bun.lockb`             | `bun install --frozen-lockfile`                                                                                                                             |
| `uv.lock`                            | `uv sync --frozen`                                                                                                                                          |
| `poetry.lock`                        | `poetry install --no-interaction` (with `virtualenvs.in-project` if the project uses an in-project `.venv`)                                                 |
| `requirements*.txt`                  | `python3 -m venv .venv && .venv/bin/pip install -r requirements.txt`                                                                                        |
| `go.mod`                             | Usually nothing: the module and build caches are shared. `go mod download` at most.                                                                         |
| `Gemfile.lock`                       | `bundle install`                                                                                                                                            |
| `mix.lock`                           | `mix deps.get`, and copy `_build` from the main checkout for warm compiles.                                                                                 |
| `build.gradle*` / `pom.xml`          | Usually nothing: `~/.gradle` and `~/.m2` are shared.                                                                                                        |
| `composer.lock`                      | `composer install --no-interaction`                                                                                                                         |
| `*.csproj` / `*.sln`                 | `dotnet restore`                                                                                                                                            |
| `.gitmodules`                        | `git submodule update --init --recursive`                                                                                                                   |
| `.gitattributes` with `filter=lfs`   | `git lfs pull`                                                                                                                                              |
| ignored config (`.env`, `*.local.*`) | Copy from the main checkout if it is there and the worktree has none.                                                                                       |

For a toolchain not listed, the pattern is the same: install from the lockfile into the worktree, and copy build output from the main checkout only when the build tool compares mtimes or content hashes, so the copy is reused rather than trusted blindly.

## Example: seeding a Rust target

nth's own script, the Rust row above in full:

```bash
#!/usr/bin/env bash
# Seed a new worktree with the main checkout's compiled dependencies, so
# cargo reuses them instead of rebuilding the world.
set -euo pipefail

base=${NTH_WORKTREE_BASE:?NTH_WORKTREE_BASE is not set}
worktree=${NTH_WORKTREE_PATH:?NTH_WORKTREE_PATH is not set}
source_dir=$base/target

if [ ! -d "$source_dir/debug/deps" ]; then
    echo "no compiled target in $source_dir; nothing to seed"
    exit 0
fi

if [ -e "$worktree/target/debug/deps" ]; then
    echo "$worktree/target already exists; leaving it alone"
    exit 0
fi

echo "seeding $worktree/target from $source_dir"
mkdir -p "$worktree/target/debug"

copy() {
    [ -e "$1" ] || return 0
    cp -a --reflink=auto "$1" "$2" 2>/dev/null || cp -a "$1" "$2"
}

copy "$source_dir/CACHEDIR.TAG" "$worktree/target/"
copy "$source_dir/.rustc_info.json" "$worktree/target/"
copy "$source_dir/debug/deps" "$worktree/target/debug/"
copy "$source_dir/debug/build" "$worktree/target/debug/"
copy "$source_dir/debug/.fingerprint" "$worktree/target/debug/"

echo "seeded $(du -sh "$worktree/target" | cut -f1) of compiled dependencies"
```

A polyglot script puts each section in its own guarded block or function, one after another.

## Questions

Only ask the user what the repository cannot answer, with the `question` tool, one short batch at most. Good questions: which ignored files hold secrets that must not be copied into worktrees, and whether a slow step (a full build, a database seed) is worth its time in every new worktree.

## Test it

Run it in a throwaway worktree before calling it done, from the main checkout:

```bash
git worktree add --detach /tmp/nth-setup-test HEAD
cd /tmp/nth-setup-test
time NTH_WORKTREE_BASE=<main checkout> NTH_WORKTREE_PATH=/tmp/nth-setup-test bash <main checkout>/.nth/worktree-setup
```

Check that it exits 0, that a second run is harmless, and that the project's build or test command works there afterwards. Then remove it with `git worktree remove --force /tmp/nth-setup-test`.

Report what the script does, how long it took, and remind the user to commit `.nth/worktree-setup`.
