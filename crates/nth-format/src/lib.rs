//! Format a file after a tool writes it, the way opencode does: every
//! formatter that handles the file's extension and applies to the project
//! runs on it, one after another.

mod config;
mod probe;
mod registry;

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Mutex, MutexGuard},
    time::Duration,
};

pub use config::{FormatConfig, FormatterConfig};
use registry::Builtin;

/// How long one formatter may run on one file.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The formatters from the config, built-in and custom, with what their
/// probes found per project.
pub struct Formatters {
    enabled: bool,
    list: Vec<Formatter>,
    search_path: Option<OsString>,
    timeout: Duration,
    /// Probe results per (formatter, project directory), failures too, so
    /// a missing formatter costs one lookup per project, not one per write.
    probed: Mutex<Probed>,
}

/// Probe results keyed by (index into the list, project directory).
type Probed = HashMap<(usize, PathBuf), Result<Vec<String>, String>>;

struct Formatter {
    name: String,
    extensions: Vec<String>,
    env: BTreeMap<String, String>,
    kind: Kind,
}

enum Kind {
    Disabled,
    Command(Vec<String>),
    Builtin(&'static Builtin),
}

/// What running one formatter on a file did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub name: String,
    /// The error is one line: the first line of stderr, or why it did not
    /// run to the end.
    pub result: Result<(), String>,
}

/// One formatter as it stands for a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatterStatus {
    pub name: String,
    pub extensions: Vec<String>,
    /// The command it runs, or why it does not run.
    pub command: Result<Vec<String>, String>,
}

impl Formatters {
    pub fn new(config: &FormatConfig) -> Self {
        let mut list: Vec<_> = registry::BUILTINS
            .iter()
            .map(|builtin| {
                let entry = config.formatters.get(builtin.name);
                let kind = match entry {
                    Some(entry) if entry.disabled => Kind::Disabled,
                    Some(FormatterConfig {
                        command: Some(command),
                        ..
                    }) => Kind::Command(command.clone()),
                    _ => Kind::Builtin(builtin),
                };
                Formatter {
                    name: builtin.name.into(),
                    extensions: entry.and_then(|e| e.extensions.clone()).unwrap_or_else(|| {
                        builtin.extensions.iter().map(|e| e.to_string()).collect()
                    }),
                    env: entry.map(|e| e.env.clone()).unwrap_or_default(),
                    kind,
                }
            })
            .collect();
        for (name, entry) in &config.formatters {
            if registry::find(name).is_some() {
                continue;
            }
            list.push(Formatter {
                name: name.clone(),
                extensions: entry.extensions.clone().unwrap_or_default(),
                env: entry.env.clone(),
                kind: match &entry.command {
                    Some(command) if !entry.disabled => Kind::Command(command.clone()),
                    _ => Kind::Disabled,
                },
            });
        }
        Self {
            enabled: config.enabled,
            list,
            search_path: std::env::var_os("PATH"),
            timeout: TIMEOUT,
            probed: Mutex::default(),
        }
    }

    /// Runs every formatter for `path` in turn, in `cwd`, which is also the
    /// project directory the probes look from. Empty when none applies.
    pub async fn format(&self, path: &Path, cwd: &Path) -> Vec<Outcome> {
        let mut outcomes = Vec::new();
        if !self.enabled {
            return outcomes;
        }
        let Some(extension) = path.extension() else {
            return outcomes;
        };
        let extension = format!(".{}", extension.to_string_lossy());
        for (index, formatter) in self.list.iter().enumerate() {
            if !formatter.extensions.contains(&extension) {
                continue;
            }
            if let Ok(command) = self.command(index, cwd).await {
                outcomes.push(Outcome {
                    name: formatter.name.clone(),
                    result: self.run(formatter, &command, path, cwd).await,
                });
            }
        }
        outcomes
    }

    /// Every formatter, in the order they run, as it stands for `cwd`.
    pub async fn status(&self, cwd: &Path) -> Vec<FormatterStatus> {
        let mut status = Vec::with_capacity(self.list.len());
        for (index, formatter) in self.list.iter().enumerate() {
            let command = match self.enabled {
                true => self.command(index, cwd).await,
                false => Err("formatting is off in the config".into()),
            };
            status.push(FormatterStatus {
                name: formatter.name.clone(),
                extensions: formatter.extensions.clone(),
                command,
            });
        }
        status
    }

    /// The command for a formatter, or why it does not run. One that
    /// yields to another (uv to ruff) is off while the other is enabled,
    /// and also when the other is disabled in the config, as in opencode.
    async fn command(&self, index: usize, cwd: &Path) -> Result<Vec<String>, String> {
        if let Kind::Builtin(Builtin {
            yields_to: Some(other),
            ..
        }) = self.list[index].kind
            && let Some(other) = self.list.iter().position(|f| f.name == *other)
        {
            match self.probe(other, cwd).await {
                Ok(_) => return Err(format!("{} is enabled", self.list[other].name)),
                Err(_) if matches!(self.list[other].kind, Kind::Disabled) => {
                    return Err(format!(
                        "{} is disabled in the config",
                        self.list[other].name
                    ));
                }
                Err(_) => {}
            }
        }
        self.probe(index, cwd).await
    }

    async fn probe(&self, index: usize, cwd: &Path) -> Result<Vec<String>, String> {
        let builtin = match &self.list[index].kind {
            Kind::Disabled => return Err("disabled in the config".into()),
            Kind::Command(command) => return Ok(command.clone()),
            Kind::Builtin(builtin) => builtin,
        };
        let key = (index, cwd.to_path_buf());
        if let Some(found) = self.cache().get(&key) {
            return found.clone();
        }
        let env = probe::Env {
            cwd,
            search_path: self.search_path.as_deref(),
        };
        let found = probe::check(builtin, &env).await;
        self.cache().insert(key, found.clone());
        found
    }

    /// Held only for a lookup or an insert, never across an await.
    fn cache(&self) -> MutexGuard<'_, Probed> {
        self.probed.lock().expect("probe cache poisoned")
    }

    async fn run(
        &self,
        formatter: &Formatter,
        command: &[String],
        path: &Path,
        cwd: &Path,
    ) -> Result<(), String> {
        let mut args = command.iter().map(|arg| match arg.as_str() {
            "$FILE" => path.as_os_str().to_owned(),
            _ => arg.replace("$FILE", &path.to_string_lossy()).into(),
        });
        let program = args.next().ok_or("empty command")?;
        let child = tokio::process::Command::new(&program)
            .args(args)
            .current_dir(cwd)
            .envs(&formatter.env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot run {}: {e}", program.to_string_lossy()))?;
        // On timeout the child is dropped, and `kill_on_drop` stops it.
        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| format!("timed out after {:?}", self.timeout))?
            .map_err(|e| e.to_string())?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(
            match stderr.lines().map(str::trim).find(|l| !l.is_empty()) {
                Some(line) => line.to_string(),
                None => format!("exited with {}", output.status),
            },
        )
    }

    #[cfg(test)]
    fn with_search_path(mut self, search_path: &Path) -> Self {
        self.search_path = Some(search_path.as_os_str().to_owned());
        self
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> FormatConfig {
        toml::from_str(text).expect("valid format config")
    }

    fn custom(name: &str, command: &[&str], extension: &str) -> (String, FormatterConfig) {
        let entry = FormatterConfig {
            command: Some(command.iter().map(|s| s.to_string()).collect()),
            extensions: Some(vec![extension.into()]),
            ..FormatterConfig::default()
        };
        (name.into(), entry)
    }

    fn formatters(entries: impl IntoIterator<Item = (String, FormatterConfig)>) -> Formatters {
        Formatters::new(&FormatConfig {
            enabled: true,
            formatters: entries.into_iter().collect(),
        })
    }

    fn file(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).expect("write file");
        path
    }

    /// A program called `name` in a fresh `bin` folder, for a search path
    /// that holds nothing else.
    fn fake_program(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        let path = bin.join(name);
        std::fs::write(&path, "#!/bin/sh\n").expect("write program");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        bin
    }

    #[tokio::test]
    async fn custom_formatter_rewrites_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a.txt", "aaa\n");
        let formatters = formatters([custom("sed", &["sed", "-i", "s/a/b/g", "$FILE"], ".txt")]);

        let outcomes = formatters.format(&path, dir.path()).await;

        assert_eq!(
            outcomes,
            [Outcome {
                name: "sed".into(),
                result: Ok(())
            }]
        );
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "bbb\n");
    }

    #[tokio::test]
    async fn every_matching_formatter_runs_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a.txt", "a\n");
        let formatters = formatters([
            custom("one", &["sed", "-i", "s/a/b/", "$FILE"], ".txt"),
            custom("two", &["sed", "-i", "s/b/c/", "$FILE"], ".txt"),
            custom("other", &["false"], ".md"),
        ]);

        let names: Vec<_> = formatters
            .format(&path, dir.path())
            .await
            .into_iter()
            .map(|o| o.name)
            .collect();

        assert_eq!(names, ["one", "two"]);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "c\n");
    }

    #[tokio::test]
    async fn file_placeholder_works_inside_an_argument() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a.txt", "");
        let formatters = formatters([custom(
            "echo",
            &["sh", "-c", "echo done > $FILE.out"],
            ".txt",
        )]);

        formatters.format(&path, dir.path()).await;

        let out = dir.path().join("a.txt.out");
        assert_eq!(std::fs::read_to_string(out).expect("read"), "done\n");
    }

    #[tokio::test]
    async fn failure_keeps_the_first_stderr_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a.txt", "");
        let script = "echo >&2; echo 'syntax error' >&2; echo more >&2; exit 2";
        let formatters = formatters([custom("bad", &["sh", "-c", script], ".txt")]);

        let outcomes = formatters.format(&path, dir.path()).await;

        assert_eq!(outcomes[0].result, Err("syntax error".into()));
    }

    #[tokio::test]
    async fn missing_program_is_a_failure_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a.txt", "");
        let formatters = formatters([custom("ghost", &["nth-no-such-program"], ".txt")]);

        let outcomes = formatters.format(&path, dir.path()).await;

        let err = outcomes[0].result.clone().expect_err("cannot spawn");
        assert!(err.starts_with("cannot run nth-no-such-program"), "{err}");
    }

    #[tokio::test]
    async fn slow_formatter_times_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a.txt", "");
        let formatters = formatters([custom("slow", &["sleep", "30"], ".txt")])
            .with_timeout(Duration::from_millis(200));

        let started = std::time::Instant::now();
        let outcomes = formatters.format(&path, dir.path()).await;

        assert_eq!(outcomes[0].result, Err("timed out after 200ms".into()));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn disabled_formatter_does_not_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a.txt", "a");
        let (name, mut entry) = custom("sed", &["sed", "-i", "s/a/b/", "$FILE"], ".txt");
        entry.disabled = true;
        let formatters = formatters([(name, entry)]);

        assert!(formatters.format(&path, dir.path()).await.is_empty());
        let status = formatters.status(dir.path()).await;
        let sed = status.iter().find(|s| s.name == "sed").expect("listed");
        assert_eq!(sed.command, Err("disabled in the config".into()));
    }

    #[tokio::test]
    async fn formatting_off_runs_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a.txt", "a");
        let mut config = FormatConfig {
            enabled: false,
            ..FormatConfig::default()
        };
        let (name, entry) = custom("sed", &["sed", "-i", "s/a/b/", "$FILE"], ".txt");
        config.formatters.insert(name, entry);
        let formatters = Formatters::new(&config);

        assert!(formatters.format(&path, dir.path()).await.is_empty());
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "a");
        assert!(
            formatters
                .status(dir.path())
                .await
                .iter()
                .all(|s| s.command.is_err())
        );
    }

    #[tokio::test]
    async fn marker_probe_looks_in_parent_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = fake_program(dir.path(), "clang-format");
        let project = dir.path().join("project/src");
        std::fs::create_dir_all(&project).expect("project dir");
        let formatters = formatters([]).with_search_path(&bin);

        let clang = |status: Vec<FormatterStatus>| {
            status
                .into_iter()
                .find(|s| s.name == "clang-format")
                .expect("clang-format is built in")
                .command
        };
        assert_eq!(
            clang(formatters.status(&project).await),
            Err("no .clang-format found".into())
        );

        std::fs::write(dir.path().join("project/.clang-format"), "").expect("marker");
        let fresh = Formatters::new(&FormatConfig::default()).with_search_path(&bin);
        let command = clang(fresh.status(&project).await).expect("marker found");
        assert_eq!(command[0], bin.join("clang-format").to_string_lossy());
        assert_eq!(command[1..], ["-i", "$FILE"]);
    }

    #[tokio::test]
    async fn probe_results_are_cached_per_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        let formatters = formatters([]).with_search_path(&bin);
        let rustfmt = |status: Vec<FormatterStatus>| {
            status
                .into_iter()
                .find(|s| s.name == "rustfmt")
                .expect("rustfmt is built in")
                .command
        };
        assert!(rustfmt(formatters.status(dir.path()).await).is_err());

        fake_program(dir.path(), "rustfmt");

        assert!(
            rustfmt(formatters.status(dir.path()).await).is_err(),
            "a failed probe is not repeated"
        );
        let other = tempfile::tempdir().expect("tempdir");
        assert!(rustfmt(formatters.status(other.path()).await).is_ok());
    }

    #[tokio::test]
    async fn uv_yields_to_ruff() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = fake_program(dir.path(), "ruff");
        std::fs::write(dir.path().join("ruff.toml"), "").expect("ruff config");
        let formatters = formatters([]).with_search_path(&bin);

        let status = formatters.status(dir.path()).await;

        let get = |name: &str| status.iter().find(|s| s.name == name).expect("listed");
        assert!(get("ruff").command.is_ok());
        assert_eq!(get("uv").command, Err("ruff is enabled".into()));
    }

    #[tokio::test]
    async fn config_command_replaces_a_builtin_probe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a.rs", "a");
        let formatters = Formatters::new(&config(
            r#"
            [rustfmt]
            command = ["sed", "-i", "s/a/b/", "$FILE"]
            "#,
        ))
        .with_search_path(&dir.path().join("empty"));

        let outcomes = formatters.format(&path, dir.path()).await;

        assert_eq!(outcomes.len(), 1);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "b");
    }

    #[tokio::test]
    async fn env_reaches_the_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a.txt", "");
        let (name, mut entry) = custom("env", &["sh", "-c", "echo $GREETING > $FILE"], ".txt");
        entry.env.insert("GREETING".into(), "hi".into());
        let formatters = formatters([(name, entry)]);

        formatters.format(&path, dir.path()).await;

        assert_eq!(std::fs::read_to_string(&path).expect("read"), "hi\n");
    }
}
