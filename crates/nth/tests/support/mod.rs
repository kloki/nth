//! The one way a binary test runs `nth`: the real binary in an environment
//! of its own. The environment is cleared, so a developer's `NTH_CONFIG`,
//! `NTH_MODEL`, proxy or `GIT_DIR` cannot steer the child, and `HOME`,
//! `XDG_DATA_HOME` and `XDG_CONFIG_HOME` point into one temp dir, so no
//! test reads or writes the real `~/.config/nth`, `~/.local/share/nth` or a
//! project's `.nth`.

// Each test binary compiles this module on its own and uses only part of it.
#![allow(dead_code)]

use std::{
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;

/// How long one run may take before it is killed and the test fails: far
/// past any command's own deadline, so only a hang reaches it.
const TIMEOUT: Duration = Duration::from_secs(60);

/// An isolated home for the binary: the temp root and the directories in it
/// the child is pointed at, plus the variables a test adds.
pub struct Env {
    root: TempDir,
    home: PathBuf,
    data: PathBuf,
    config: PathBuf,
    vars: Vec<(OsString, OsString)>,
}

impl Env {
    pub fn new() -> Self {
        let root = tempfile::tempdir().expect("a temp dir");
        let env = Self {
            home: root.path().join("home"),
            data: root.path().join("data"),
            config: root.path().join("config"),
            root,
            vars: Vec::new(),
        };
        for dir in [&env.home, &env.data, &env.config] {
            std::fs::create_dir_all(dir).expect("creates the env's dirs");
        }
        env
    }

    /// The temp dir everything lives under; [`Env::redact`] names it `[root]`.
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// Keeps the temp dir past the test, for a developer to look into.
    pub fn keep(self) -> PathBuf {
        self.root.keep()
    }

    /// `HOME`.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// `XDG_DATA_HOME`.
    pub fn data(&self) -> &Path {
        &self.data
    }

    /// `XDG_CONFIG_HOME`.
    pub fn config(&self) -> &Path {
        &self.config
    }

    /// Where the child saves its sessions, for a test that saves one first.
    pub fn sessions(&self) -> PathBuf {
        self.data.join("nth/sessions")
    }

    /// A new directory `name` under the root, for a project or a repository.
    pub fn dir(&self, name: &str) -> PathBuf {
        let dir = self.root().join(name);
        std::fs::create_dir_all(&dir).expect("creates the dir");
        dir
    }

    /// One more variable every later run sees, such as a provider's key.
    pub fn set(&mut self, key: &str, value: &str) {
        self.vars.push((key.into(), value.into()));
    }

    /// The command, not yet started, for a test that drives the child
    /// itself (signals, a server it reads from); [`Env::run`] otherwise.
    pub fn command<S: AsRef<std::ffi::OsStr>>(&self, cwd: &Path, args: &[S]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nth"));
        command
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("NO_COLOR", "1")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .envs(self.vars.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null());
        // Under cargo-llvm-cov the binary is instrumented: without this its
        // runs count for nothing and it drops its profile in `cwd`, which a
        // test that checks its repository is clean would see.
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        command
    }

    /// Runs `nth args` in `cwd` to the end, killing it and failing the test
    /// if it outlives [`TIMEOUT`].
    pub fn run<S: AsRef<std::ffi::OsStr>>(&self, cwd: &Path, args: &[S]) -> Output {
        let mut child = self
            .command(cwd, args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawns nth");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        // Drained on their own threads, so a full pipe can never deadlock
        // the wait below.
        let out = thread::spawn(move || read_all(stdout));
        let err = thread::spawn(move || read_all(stderr));
        let status = wait(&mut child);
        Output {
            status,
            stdout: out.join().expect("the stdout reader"),
            stderr: err.join().expect("the stderr reader"),
        }
    }

    /// [`Env::run`], failing the test with the child's stderr unless it
    /// succeeded.
    pub fn success<S: AsRef<std::ffi::OsStr>>(&self, cwd: &Path, args: &[S]) -> Output {
        let output = self.run(cwd, args);
        assert!(
            output.status.success(),
            "nth {:?} failed: {:?}\nstderr: {}",
            args.iter().map(|a| a.as_ref()).collect::<Vec<_>>(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// A command that prints one JSON document: that document.
    pub fn json<S: AsRef<std::ffi::OsStr>>(&self, cwd: &Path, args: &[S]) -> Value {
        let output = self.success(cwd, args);
        serde_json::from_slice(&output.stdout).expect("stdout is one JSON document")
    }

    /// A command that prints JSON lines: each line, parsed.
    pub fn json_lines<S: AsRef<std::ffi::OsStr>>(&self, cwd: &Path, args: &[S]) -> Vec<Value> {
        let output = self.success(cwd, args);
        lines(&output.stdout)
    }

    /// `text` with the temp root, as given and as the kernel resolves it,
    /// replaced by `[root]`, so a snapshot holds only what the fixture
    /// decides.
    pub fn redact(&self, text: &str) -> String {
        let given = self.root().to_string_lossy().into_owned();
        let resolved = self
            .root()
            .canonicalize()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| given.clone());
        // The longer first, so one never leaves a tail of the other.
        let (first, second) = match resolved.len() > given.len() {
            true => (resolved, given),
            false => (given, resolved),
        };
        text.replace(&first, "[root]").replace(&second, "[root]")
    }
}

/// Every line of `stdout`, parsed as JSON.
pub fn lines(stdout: &[u8]) -> Vec<Value> {
    std::str::from_utf8(stdout)
        .expect("stdout is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line is JSON"))
        .collect()
}

fn read_all(mut reader: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).expect("reads the pipe");
    bytes
}

fn wait(child: &mut Child) -> ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("nth did not finish within {TIMEOUT:?}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}
