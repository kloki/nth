//! Edits text in the user's own editor, for prompts too long to write in
//! the prompt bar.

use std::{ffi::OsString, path::Path};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// Opens `text` in `$VISUAL`, `$EDITOR` or `vi`, and returns what was saved.
/// The caller hands the terminal over first, since the editor takes it.
pub async fn edit(text: &str) -> Result<String> {
    let path = std::env::temp_dir().join(format!("nth-prompt-{}.md", uuid::Uuid::new_v4()));
    tokio::fs::write(&path, text)
        .await
        .context("writing the prompt file")?;
    let edited = run(&path).await;
    let _ = tokio::fs::remove_file(&path).await;
    edited
}

async fn run(path: &Path) -> Result<String> {
    let editor = std::env::var_os("VISUAL")
        .or_else(|| std::env::var_os("EDITOR"))
        .filter(|editor| !editor.is_empty())
        .unwrap_or_else(|| OsString::from("vi"));
    // Through the shell, so an editor set with flags (`code --wait`) works.
    let status = Command::new("sh")
        .arg("-c")
        .arg(r#"$0 "$1""#)
        .arg(&editor)
        .arg(path)
        .status()
        .await
        .with_context(|| format!("starting {}", editor.to_string_lossy()))?;
    if !status.success() {
        bail!("{} exited with {status}", editor.to_string_lossy());
    }
    let text = tokio::fs::read_to_string(path)
        .await
        .context("reading the prompt file")?;
    // Editors end the file with a newline the prompt never wanted.
    Ok(text.trim_end_matches('\n').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_what_the_editor_saved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("editor");
        std::fs::write(&script, "#!/bin/sh\nprintf 'edited\\n' >> \"$1\"\n").expect("write");
        let path = dir.path().join("prompt.md");
        std::fs::write(&path, "draft ").expect("write");

        // SAFETY: no other test reads or writes VISUAL.
        unsafe { std::env::set_var("VISUAL", format!("sh {}", script.display())) };
        let text = run(&path).await.expect("edit");

        assert_eq!(text, "draft edited");
    }
}
