# nth-worktree

Notes for working in this crate. The root `CLAUDE.md` has the commands, the crate map and the rules every crate keeps.

git worktrees under `.nth/worktrees` in the main checkout, one per name and on the branch of that name: `open` makes one on first use (the folder ignored by its own `.gitignore`) and says whether it did, and on a new branch or an existing one; nothing removes one. Depends only on nth-directories, which places the folder.
