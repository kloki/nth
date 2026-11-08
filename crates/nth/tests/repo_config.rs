//! Repository configuration through the real CLI, isolated from user settings.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    home: PathBuf,
    global: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let home = temp.path().join("home");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&home).unwrap();
        let global = temp.path().join("global.toml");
        fs::write(&global, "").unwrap();
        Self {
            _temp: temp,
            root,
            home,
            global,
        }
    }

    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(&self.root)
            .args(args)
            .env("HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repo_config(&self, content: &str) -> PathBuf {
        let dir = self.root.join(".nth");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config");
        fs::write(&path, content).unwrap();
        path
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_nth"))
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env_remove("NTH_CONFIG")
            .env_remove("NTH_MODEL")
            .arg("--config")
            .arg(&self.global)
            .args(args)
            .output()
            .unwrap()
    }

    fn success(&self, cwd: &Path, args: &[&str]) -> Output {
        let output = self.run(cwd, args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn config(&self, cwd: &Path, args: &[&str]) -> (toml::Value, String) {
        let output = self.success(cwd, args);
        (
            toml::from_str(std::str::from_utf8(&output.stdout).unwrap()).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }
}

#[test]
fn nested_repo_merges_global_and_project_and_cli_wins() {
    let f = Fixture::new();
    f.git(&["init", "--quiet"]);
    let nested = f.root.join("src/deep");
    fs::create_dir_all(&nested).unwrap();
    fs::write(&f.global, "model = 'global/model'\n[context]\nauto = true\nreserved = 1234\npreserve = 5678\n[tools.bash]\ndefault_timeout_ms = 123456\nrtk = false\n").unwrap();
    let repo = f.repo_config("model = 'repo/model'\n[context]\nauto = false\nreserved = 0\n");
    let (config, sources) = f.config(&nested, &["config"]);
    assert_eq!(config["model"].as_str(), Some("repo/model"));
    assert_eq!(config["context"]["auto"].as_bool(), Some(false));
    assert_eq!(config["context"]["reserved"].as_integer(), Some(0));
    assert_eq!(config["context"]["preserve"].as_integer(), Some(5678));
    assert_eq!(
        config["tools"]["bash"]["default_timeout_ms"].as_integer(),
        Some(123456)
    );
    assert_eq!(config["tools"]["bash"]["rtk"].as_bool(), Some(false));
    assert!(sources.contains(f.global.to_str().unwrap()), "{sources}");
    assert!(sources.contains(repo.to_str().unwrap()), "{sources}");
    let (config, _) = f.config(&nested, &["--model", "cli/model", "config"]);
    assert_eq!(config["model"].as_str(), Some("cli/model"));
}

#[test]
fn forbidden_and_invalid_repo_keys_are_reported_with_source() {
    let f = Fixture::new();
    f.git(&["init", "--quiet"]);
    for (content, key) in [
        (
            "[provider.example]\nbase_url = 'https://example.invalid'\napi_key_env = 'KEY'\n",
            "provider",
        ),
        ("[notify]\nenabled = false\n", "notify"),
        ("[chat]\nshow_thinking = false\n", "chat"),
        ("[layout]\nsplit_width = 120\n", "layout"),
        ("[tools.websearch]\nenabled = false\n", "tools"),
        ("[tools.bash]\nrtk = false\n", "tools"),
        (
            "[format.x]\ncommand = ['fmt']\nextensions = ['.x']\n",
            "format",
        ),
        (
            "[lsp.x]\ncommand = ['server']\nextensions = ['.x']\n",
            "lsp",
        ),
        ("[context]\nunknown_key = true\n", "unknown_key"),
        ("[context]\nauto = 'not a boolean'\n", "auto"),
    ] {
        let path = f.repo_config(content);
        let output = f.run(&f.root, &["config"]);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "accepted {content}");
        assert!(error.contains(path.to_str().unwrap()), "{error}");
        assert!(error.contains(key), "{error}");
    }
}

#[test]
fn skills_use_both_layer_path_bases_and_deduplicate() {
    let f = Fixture::new();
    f.git(&["init", "--quiet"]);
    let nested = f.root.join("nested");
    let global_skills = nested.join("global-skills");
    let repo_skills = f.root.join("team-skills");
    for (base, name) in [
        (&global_skills, "global-example"),
        (&repo_skills, "repo-example"),
    ] {
        let dir = base.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Example skill\n---\nInstructions.\n"),
        )
        .unwrap();
    }
    fs::write(&f.global, "[skills]\npaths = ['global-skills']\n").unwrap();
    f.repo_config("[skills]\npaths = ['team-skills', './team-skills']\n");
    let output = f.success(&nested, &["skills", "--json"]);
    let skills: Vec<serde_json::Value> = std::str::from_utf8(&output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for (name, base) in [
        ("global-example", &global_skills),
        ("repo-example", &repo_skills),
    ] {
        let found: Vec<_> = skills
            .iter()
            .filter(|skill| skill["name"] == name)
            .collect();
        assert_eq!(found.len(), 1, "{skills:?}");
        assert!(
            found[0]["path"]
                .as_str()
                .unwrap()
                .starts_with(base.to_str().unwrap())
        );
    }
    let (config, _) = f.config(&nested, &["config"]);
    let paths = config["skills"]["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert_eq!(
        std::path::Path::new(paths[0].as_str().unwrap()),
        global_skills
    );
    assert_eq!(
        std::path::Path::new(paths[1].as_str().unwrap()),
        repo_skills
    );
}

#[test]
fn linked_worktree_uses_main_checkout_config_and_paths() {
    let f = Fixture::new();
    f.git(&["init", "--quiet"]);
    f.git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "--quiet",
        "--allow-empty",
        "-m",
        "initial",
    ]);
    let worktree = f._temp.path().join("linked");
    f.git(&[
        "worktree",
        "add",
        "--quiet",
        "-b",
        "test-linked",
        worktree.to_str().unwrap(),
    ]);
    let main_config = f.repo_config("model = 'main/model'\n[skills]\npaths = ['team-skills']\n");
    fs::create_dir_all(f.root.join("team-skills")).unwrap();
    fs::create_dir_all(worktree.join(".nth")).unwrap();
    fs::write(worktree.join(".nth/config"), "model = 'wrong/model'\n").unwrap();
    let nested = worktree.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let (config, sources) = f.config(&nested, &["config"]);
    assert_eq!(config["model"].as_str(), Some("main/model"));
    assert_eq!(
        std::path::Path::new(config["skills"]["paths"][0].as_str().unwrap()),
        f.root.join("team-skills")
    );
    assert!(sources.contains(main_config.to_str().unwrap()), "{sources}");
}
