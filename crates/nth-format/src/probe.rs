//! Running a [`Probe`]: does a built-in formatter apply to a project, and
//! with which command?

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use nth_context::which_in;

use crate::registry::{Builtin, Probe};

/// Where a probe looks.
pub(crate) struct Env<'a> {
    /// The project directory; files are looked for here and above.
    pub cwd: &'a Path,
    /// Where looking up stops, inclusive: the project root. A `.clang-format`
    /// in the home directory says nothing about this project, so the walk
    /// does not go on to `/`; opencode's stops at the worktree the same way.
    pub root: &'a Path,
    /// The `PATH` programs are looked up in.
    pub search_path: Option<&'a OsStr>,
}

impl Env<'_> {
    /// `cwd` and the directories above it up to the root.
    fn dirs(&self) -> impl Iterator<Item = &Path> {
        self.cwd
            .ancestors()
            .take_while(|dir| dir.starts_with(self.root))
    }
}

/// The command to run, with the program resolved, or why the formatter
/// does not apply.
pub(crate) async fn check(builtin: &Builtin, env: &Env<'_>) -> Result<Vec<String>, String> {
    let bin = builtin.command[0];
    let program = match &builtin.probe {
        Probe::OnPath => on_path(bin, env)?,
        Probe::Marker(markers) => {
            let program = on_path(bin, env)?;
            any_marker(markers, env).await?;
            program
        }
        Probe::NodeDep(package) => {
            node_dep(package, env).await?;
            node_bin(bin, env)?
        }
        Probe::NodeMarker(markers) => {
            any_marker(markers, env).await?;
            node_bin(bin, env)?
        }
        Probe::Ruff => {
            let program = on_path(bin, env)?;
            uses_ruff(env).await?;
            program
        }
        Probe::Help { args, first_line } => {
            let program = on_path(bin, env)?;
            help_matches(&program, args, first_line, env.cwd).await?;
            program
        }
    };
    let mut command = vec![program.to_string_lossy().into_owned()];
    command.extend(builtin.command[1..].iter().map(|arg| arg.to_string()));
    Ok(command)
}

/// A handful of `stat`s on the PATH folders: not worth a blocking task.
fn on_path(bin: &str, env: &Env<'_>) -> Result<PathBuf, String> {
    which_in(bin, env.search_path).ok_or_else(|| format!("{bin} not on PATH"))
}

/// Every `name` from the project directory up to the root, nearest first.
async fn find_up(name: &str, env: &Env<'_>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in env.dirs() {
        let path = dir.join(name);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            found.push(path);
        }
    }
    found
}

async fn any_marker(markers: &[&str], env: &Env<'_>) -> Result<(), String> {
    for marker in markers {
        if !find_up(marker, env).await.is_empty() {
            return Ok(());
        }
    }
    Err(format!("no {} found", markers.join(" or ")))
}

async fn node_dep(package: &str, env: &Env<'_>) -> Result<(), String> {
    for manifest in find_up("package.json", env).await {
        let Ok(text) = tokio::fs::read_to_string(&manifest).await else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let listed = ["dependencies", "devDependencies"]
            .iter()
            .any(|key| json[key].get(package).is_some());
        if listed {
            return Ok(());
        }
    }
    Err(format!("{package} not in package.json"))
}

/// The project's own copy of a node program first, then one on `PATH`.
fn node_bin(bin: &str, env: &Env<'_>) -> Result<PathBuf, String> {
    env.dirs()
        .find_map(|dir| which_in(bin, Some(dir.join("node_modules/.bin").as_os_str())))
        .or_else(|| which_in(bin, env.search_path))
        .ok_or_else(|| format!("{bin} not in node_modules/.bin or on PATH"))
}

async fn uses_ruff(env: &Env<'_>) -> Result<(), String> {
    for config in ["pyproject.toml", "ruff.toml", ".ruff.toml"] {
        let Some(found) = find_up(config, env).await.into_iter().next() else {
            continue;
        };
        if config != "pyproject.toml" {
            return Ok(());
        }
        let text = tokio::fs::read_to_string(&found).await.unwrap_or_default();
        if text.contains("[tool.ruff]") {
            return Ok(());
        }
    }
    for deps in ["requirements.txt", "pyproject.toml", "Pipfile"] {
        if let Some(found) = find_up(deps, env).await.into_iter().next() {
            let text = tokio::fs::read_to_string(&found).await.unwrap_or_default();
            if text.contains("ruff") {
                return Ok(());
            }
        }
    }
    Err("no ruff config or dependency found".into())
}

/// Printing help is quick; a program that hangs on it is not the one
/// looked for.
const HELP_TIMEOUT: Duration = Duration::from_secs(5);

async fn help_matches(
    program: &Path,
    args: &[&str],
    first_line: &[&str],
    cwd: &Path,
) -> Result<(), String> {
    let name = program.file_name().unwrap_or(program.as_os_str());
    let name = name.to_string_lossy();
    let run = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(HELP_TIMEOUT, run)
        .await
        .map_err(|_| format!("{name} {} timed out", args.join(" ")))?
        .map_err(|e| format!("cannot run {name}: {e}"))?;
    if !output.status.success() {
        return Err(format!("{name} {} failed", args.join(" ")));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or_default();
    match first_line.iter().all(|word| line.contains(word)) {
        true => Ok(()),
        false => Err(format!("{name} is not the expected program")),
    }
}
