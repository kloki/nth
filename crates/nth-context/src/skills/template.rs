//! Running a skill as `/name args`: the body is a template, filled in the
//! way opencode fills in its commands. `$1`, `$2`, … take the arguments one
//! by one, `$ARGUMENTS` takes them all, `` !`cmd` `` becomes the command's
//! output, and `@path` attaches that file.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use super::{Skill, Skills};

/// Long enough for `git diff` or a test listing, short enough that a hung
/// command does not hold the turn up for long.
const SHELL_TIMEOUT: Duration = Duration::from_secs(30);
/// Attached files are cut here, as read cuts what it returns.
const MAX_FILE_BYTES: usize = 50 * 1024;
const FILE: &str = include_str!("file.md");

/// The skill `text` runs and its arguments, when `text` is `/name` or
/// `/name args` and `name` is a skill.
pub fn parse<'a>(text: &'a str, skills: &'a Skills) -> Option<(&'a Skill, &'a str)> {
    let rest = text.trim().strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    Some((skills.get(name)?, args.trim()))
}

impl Skill {
    /// The user message that runs this skill with `args`: the command as
    /// typed on the first line, so the chat and session titles show that,
    /// then the filled-in skill.
    pub async fn invoke(&self, args: &str, cwd: &Path) -> Result<String, String> {
        let skill = self.clone();
        let body = crate::blocking(move || skill.body()).await?;
        let body = shell(&arguments(&body, args), cwd).await;
        let skill = self.clone();
        let cwd = cwd.to_path_buf();
        let content = crate::blocking(move || {
            let files = attachments(&body, &cwd);
            let body = match files.is_empty() {
                true => body,
                false => format!("{body}\n\n{}", files.join("\n")),
            };
            skill.render_body(&body)
        })
        .await;
        let command = match args.is_empty() {
            true => format!("/{}", self.name),
            false => format!("/{} {args}", self.name),
        };
        Ok(format!("{command}\n\n{content}"))
    }
}

/// Fills in `$1`, `$2`, … and `$ARGUMENTS`. The highest numbered
/// placeholder takes the rest of the arguments, so `$1` alone takes them
/// all. A body with no placeholder gets the arguments appended.
fn arguments(body: &str, args: &str) -> String {
    let words = words(args);
    let highest = placeholders(body).max().unwrap_or(0);
    if highest == 0 && !body.contains("$ARGUMENTS") {
        return match args.is_empty() {
            true => body.to_string(),
            false => format!("{body}\n\nARGUMENTS: {args}"),
        };
    }
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        match after[..digits].parse::<usize>() {
            Ok(n) if n > 0 => {
                let value = match n == highest {
                    true => words.get(n - 1..).map(|w| w.join(" ")),
                    false => words.get(n - 1).cloned(),
                };
                out.push_str(&value.unwrap_or_default());
                rest = &after[digits..];
            }
            _ => {
                out.push('$');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.replace("$ARGUMENTS", args)
}

/// The numbers of the `$N` placeholders in `body`.
fn placeholders(body: &str) -> impl Iterator<Item = usize> + '_ {
    body.split('$').skip(1).filter_map(|after| {
        let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        after[..digits].parse().ok().filter(|&n| n > 0)
    })
}

/// Splits like a shell: on whitespace, with quotes holding words together.
fn words(args: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    for c in args.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// Replaces every `` !`cmd` `` with what the command prints, run in `cwd`
/// one after another. A failure is put in its place, so the model sees it.
async fn shell(body: &str, cwd: &Path) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("!`") {
        let Some(len) = rest[start + 2..].find('`') else {
            break;
        };
        out.push_str(&rest[..start]);
        let command = &rest[start + 2..start + 2 + len];
        out.push_str(&run(command, cwd).await);
        rest = &rest[start + 2 + len + 1..];
    }
    out.push_str(rest);
    out
}

async fn run(command: &str, cwd: &Path) -> String {
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(SHELL_TIMEOUT, child).await {
        Err(_) => format!("(`{command}` timed out after {}s)", SHELL_TIMEOUT.as_secs()),
        Ok(Err(e)) => format!("(`{command}` could not run: {e})"),
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stdout = stdout.trim_end();
            if output.status.success() {
                return stdout.to_string();
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            let status = output.status.code().map_or_else(
                || "a signal".to_string(),
                |code| format!("exit code {code}"),
            );
            format!(
                "{stdout}\n(`{command}` failed with {status}: {})",
                stderr.trim_end()
            )
            .trim_start()
            .to_string()
        }
    }
}

/// A `<file>` block for every `@path` in `body` that names a file under
/// `cwd`, each file once.
fn attachments(body: &str, cwd: &Path) -> Vec<String> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut blocks = Vec::new();
    for word in body.split_whitespace() {
        let Some(path) = word.strip_prefix('@') else {
            continue;
        };
        let path = path.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '"', '\'', '`']);
        if path.is_empty() {
            continue;
        }
        let file = cwd.join(path);
        if seen.contains(&file) || !file.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(&file) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_FILE_BYTES)]);
        blocks.push(
            FILE.replace("{path}", &file.display().to_string())
                .replace("{content}", text.trim_end())
                .trim_end()
                .to_string(),
        );
        seen.push(file);
    }
    blocks
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn numbered_placeholders_take_words_and_the_last_takes_the_rest() {
        assert_eq!(
            arguments("Fix $1 in $2.", r#"bug "src/a b.rs" quickly"#),
            "Fix bug in src/a b.rs quickly."
        );
        assert_eq!(arguments("Fix $1 and $3.", "a"), "Fix a and .");
        assert_eq!(
            arguments("Costs $ 5, $0.", "a"),
            "Costs $ 5, $0.\n\nARGUMENTS: a"
        );
    }

    #[test]
    fn arguments_takes_everything_and_no_placeholder_appends() {
        assert_eq!(arguments("Review $ARGUMENTS now", "x y"), "Review x y now");
        assert_eq!(arguments("Review.", "x y"), "Review.\n\nARGUMENTS: x y");
        assert_eq!(arguments("Review.", ""), "Review.");
    }

    #[tokio::test]
    async fn shell_commands_become_their_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.txt"), "").expect("writes");

        assert_eq!(
            shell("Files: !`ls`. Done: !`echo hi`", dir.path()).await,
            "Files: a.txt. Done: hi"
        );
        assert_eq!(
            shell("!`echo out; echo err >&2; exit 2`", dir.path()).await,
            "out\n(`echo out; echo err >&2; exit 2` failed with exit code 2: err)"
        );
        assert_eq!(
            shell("not closed !`ls", dir.path()).await,
            "not closed !`ls"
        );
    }

    #[test]
    fn existing_files_are_attached_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("notes.md"), "remember\n").expect("writes");

        let blocks = attachments(
            "See @notes.md, then @notes.md again and @missing.md.",
            dir.path(),
        );

        assert_eq!(
            blocks,
            [format!(
                "<file path=\"{}\">\nremember\n</file>",
                dir.path().join("notes.md").display()
            )]
        );
    }

    #[tokio::test]
    async fn invoking_puts_the_command_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skill_dir = dir.path().join(".agents/skills/fix");
        fs::create_dir_all(&skill_dir).expect("dirs");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: fix\ndescription: fix things\n---\nFix $ARGUMENTS in !`echo here`.\n",
        )
        .expect("writes");
        let skills = super::super::discover(dir.path(), &crate::Paths::default(), &mut Vec::new());
        let (skill, args) = parse("/fix the build ", &skills).expect("a skill");

        let message = skill.invoke(args, dir.path()).await.expect("invokes");

        assert!(
            message.starts_with(
                "/fix the build\n\n<skill_content name=\"fix\">\n# Skill: fix\n\nFix the build in here.\n"
            ),
            "{message}"
        );
    }

    #[test]
    fn only_skill_names_parse() {
        let skills = Skills::default();
        assert!(parse("/etc/hosts what is this", &skills).is_none());
        assert!(parse("fix", &skills).is_none());
    }
}
