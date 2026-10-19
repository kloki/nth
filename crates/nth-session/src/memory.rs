//! The project's memory folder on disk. What goes into the system prompt is
//! nth-context's; this keeps the folder out of git before the model writes
//! to it.

use std::{io, path::Path};

/// The memory folder's own `.gitignore`: it ignores everything, itself
/// included, so the folder never shows in git, as `.nth/worktrees` does
/// not. What the model learned about the person stays on their machine.
const GITIGNORE: &str = "# nth's memory of this project, kept out of git.\n*\n";

/// Creates `dir` with its `.gitignore` when the project is a repository;
/// outside one there is no git to keep it from, and no folder is made
/// before the model saves something.
pub(crate) async fn ensure_dir(dir: &Path, cwd: &Path) -> io::Result<()> {
    if nth_context::project_root(cwd).is_none() {
        return Ok(());
    }
    tokio::fs::create_dir_all(dir).await?;
    let gitignore = dir.join(".gitignore");
    if !tokio::fs::try_exists(&gitignore).await.unwrap_or(false) {
        tokio::fs::write(&gitignore, GITIGNORE).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_repository_gets_the_folder_ignored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(tmp.path().join(".git")).expect("dir");
        let dir = tmp.path().join(".nth/memory");

        ensure_dir(&dir, tmp.path()).await.expect("ready");

        let gitignore = std::fs::read_to_string(dir.join(".gitignore")).expect("written");
        assert_eq!(gitignore, GITIGNORE);
    }

    #[tokio::test]
    async fn outside_a_repository_nothing_is_made() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join(".nth/memory");

        ensure_dir(&dir, tmp.path()).await.expect("fine");

        assert!(!dir.exists());
    }
}
