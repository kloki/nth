//! The global config and the repository's `.nth/config` layered into one
//! `Config`: which file each is, what a repository may set, its paths
//! resolved and kept under the repository root, and how the two merge.

use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use nth_context::Paths;
use nth_directories::Directories;

use crate::Config;

impl Config {
    /// Resolve the global config and project overlay for a session's cwd:
    /// `path`, else the config in `paths`' config directory, and the
    /// repository's, whose walk stops at `paths`' home.
    pub fn load_in(path: Option<&Path>, cwd: &Path, paths: &Paths) -> Result<Self> {
        Self::load_layers(path, cwd, paths).map(|(config, _)| config)
    }

    /// As `load_in`, also returning the files actually read, global first.
    fn load_layers(path: Option<&Path>, cwd: &Path, paths: &Paths) -> Result<(Self, Vec<PathBuf>)> {
        let home = paths.home.as_deref();
        let cwd = if cwd.is_absolute() {
            cwd.to_path_buf()
        } else {
            std::env::current_dir()?.join(cwd)
        };
        let global = path
            .map(Path::to_path_buf)
            .or_else(|| Config::default_path(paths));
        let mut sources = Vec::new();
        let mut value = toml::Value::Table(Default::default());
        if let Some(global) = global.as_deref().filter(|p| path.is_some() || p.exists()) {
            let mut layer = read_layer(global, false)?;
            normalize_paths(&mut layer, &cwd, home, false)?;
            merge(&mut value, layer, "");
            sources.push(global.to_path_buf());
        }
        if let Some((root, file)) = repo_config(&cwd, home) {
            let invalid = || format!("invalid config {}", file.display());
            // Where a refused path belongs, named even when that file does
            // not exist yet.
            let global = global.as_ref().map_or_else(
                || "the global config".to_string(),
                |p| p.display().to_string(),
            );
            let mut layer = read_layer(&file, true)?;
            normalize_paths(&mut layer, &root, home, true).with_context(invalid)?;
            paths_inside(&layer, &root, &global).with_context(invalid)?;
            merge(&mut value, layer, "");
            sources.push(file);
        }
        let text = toml::to_string(&value)?;
        let config = Self::parse(&text).with_context(|| {
            format!(
                "invalid merged config from {}",
                sources
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        Ok((config, sources))
    }
}

/// The one project config for `cwd` and the directory its paths are
/// relative to: the main checkout's in a Git repository, else the nearest
/// among the directories instructions are read from (up to home when under
/// it, else `cwd` alone).
pub fn repo_config(cwd: &Path, home: Option<&Path>) -> Option<(PathBuf, PathBuf)> {
    if let Some(root) = nth_directories::main_project_root(cwd) {
        let file = Directories::from_root(&root).config();
        return file.exists().then_some((root, file));
    }
    nth_context::search_dirs(cwd, home)
        .into_iter()
        .find_map(|root| {
            let file = Directories::from_root(root).config();
            file.exists().then(|| (root.to_path_buf(), file))
        })
}

fn read_layer(path: &Path, repo: bool) -> Result<toml::Value> {
    let result = || -> Result<toml::Value> {
        let text = std::fs::read_to_string(path).map_err(|e| match e.kind() {
            ErrorKind::NotFound => anyhow::anyhow!("config file {} not found", path.display()),
            _ => anyhow::Error::new(e).context(format!("cannot read {}", path.display())),
        })?;
        let value: toml::Value = toml::from_str(&text)?;
        let table = value.as_table().expect("TOML document is a table");
        if repo {
            for key in table.keys() {
                if ![
                    "model",
                    "profiles",
                    "mode",
                    "session",
                    "task",
                    "work",
                    "context",
                    "memory",
                    "skills",
                    "directories",
                ]
                .contains(&key.as_str())
                {
                    bail!("project config key `{key}` is not allowed");
                }
            }
        }
        let mut parsed: Config = value.clone().try_into()?;
        for (id, provider) in &mut parsed.provider {
            provider.complete(id)?;
        }
        for (index, profile) in parsed.profiles.iter().enumerate() {
            profile.complete(index)?;
        }
        Ok(value)
    };
    result().with_context(|| format!("invalid config {}", path.display()))
}

/// Resolves every `skills.paths` and `directories.paths` entry against
/// `base`, dropping duplicates. A path that cannot be resolved (a symlink
/// loop) refuses a project layer, whose paths must be checked, and stays as
/// written in the global one, where a broken entry only warns later.
fn normalize_paths(
    value: &mut toml::Value,
    base: &Path,
    home: Option<&Path>,
    project: bool,
) -> Result<()> {
    for section in ["skills", "directories"] {
        let Some(paths) = value.get_mut(section).and_then(|v| v.get_mut("paths")) else {
            continue;
        };
        let paths = paths.as_array_mut().expect("validated paths");
        let mut seen = std::collections::HashSet::new();
        let mut kept = Vec::with_capacity(paths.len());
        for value in paths.iter() {
            let written = value.as_str().expect("validated path");
            let path = skill_path(written, home);
            let path = if path.is_absolute() {
                path
            } else {
                base.join(path)
            };
            let path = match resolve_path(&path) {
                Ok(path) => path,
                Err(error) if project => {
                    return Err(error.context(format!("{section}.paths entry {written}")));
                }
                Err(_) => path,
            };
            if seen.insert(path.clone()) {
                kept.push(toml::Value::String(path.to_string_lossy().into_owned()));
            }
        }
        *paths = kept;
    }
    Ok(())
}

/// Refuses a project layer's normalized `skills.paths` or `directories.paths`
/// entry outside `root`: a cloned repository must not hand the session a
/// directory such as `~/.ssh` or load prompt text from elsewhere on disk.
/// `normalize_paths` resolved every existing component through `resolve_path`,
/// so a prefix check sees where a symlink really points.
///
/// The check holds only at load: a path missing then can become an outward
/// symlink before the session uses it, say after a branch switch. Outside Git a `~/.nth/config` is a project config whose root
/// is home, so it may name anything under home; that file is the user's own.
fn paths_inside(value: &toml::Value, root: &Path, global: &str) -> Result<()> {
    let root = resolve_path(root)?;
    for section in ["skills", "directories"] {
        let Some(paths) = value
            .get(section)
            .and_then(|v| v.get("paths"))
            .and_then(toml::Value::as_array)
        else {
            continue;
        };
        for path in paths.iter().filter_map(toml::Value::as_str) {
            if !Path::new(path).starts_with(&root) {
                bail!("{section}.paths entry {path} is outside the repository; put it in {global}");
            }
        }
    }
    Ok(())
}

/// Symlinks `resolve_path` follows by hand before calling it a loop, as
/// Linux's own limit.
const SYMLINK_HOPS: usize = 40;

/// `path` made absolute and free of `.` and `..`, each existing prefix
/// canonicalized as it is reached, so a symlink is followed before a later
/// `..` and wherever it sits, even after a missing folder (`missing/../link`).
/// A symlink that does not canonicalize (dangling, or in a loop) is followed
/// through its target as text, so the result names where it points even
/// before that exists; past `SYMLINK_HOPS` it is a loop and an error. Other
/// components that do not exist are kept as written.
fn resolve_path(path: &Path) -> Result<PathBuf> {
    use std::path::Component;
    // Components still to walk, the next one last, so a symlink's target
    // can be pushed in front of what followed it.
    let mut pending: Vec<PathBuf> = path
        .components()
        .rev()
        .map(|part| PathBuf::from(part.as_os_str()))
        .collect();
    let mut resolved = PathBuf::new();
    let mut hops = 0;
    while let Some(part) = pending.pop() {
        match part.components().next() {
            None | Some(Component::CurDir) => {}
            Some(Component::ParentDir) => {
                resolved.pop();
            }
            Some(_) => {
                resolved.push(&part);
                if let Ok(canonical) = std::fs::canonicalize(&resolved) {
                    resolved = canonical;
                    continue;
                }
                let dangling = std::fs::symlink_metadata(&resolved)
                    .is_ok_and(|meta| meta.file_type().is_symlink());
                if !dangling {
                    continue;
                }
                hops += 1;
                if hops > SYMLINK_HOPS {
                    bail!("{} is a symlink loop", path.display());
                }
                let target = std::fs::read_link(&resolved)
                    .with_context(|| format!("cannot read symlink {}", resolved.display()))?;
                // A relative target is from the link's folder; an absolute
                // one starts over at its root when pushed.
                resolved.pop();
                pending.extend(
                    target
                        .components()
                        .rev()
                        .map(|part| PathBuf::from(part.as_os_str())),
                );
            }
        }
    }
    Ok(resolved)
}

fn merge(base: &mut toml::Value, overlay: toml::Value, key: &str) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (name, value) in overlay {
                let path = if key.is_empty() {
                    name.clone()
                } else {
                    format!("{key}.{name}")
                };
                match base.get_mut(&name) {
                    Some(existing) => merge(existing, value, &path),
                    None => {
                        base.insert(name, value);
                    }
                }
            }
        }
        (toml::Value::Array(base), toml::Value::Array(overlay))
            if key == "skills.paths" || key == "directories.paths" =>
        {
            for item in overlay {
                if !base.contains(&item) {
                    base.push(item);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// A configured skill folder: `~/x` is `x` in the home directory.
/// Everything else, relative paths included, is left for the caller to
/// resolve.
pub(crate) fn skill_path(path: &str, home: Option<&Path>) -> PathBuf {
    nth_directories::expand_home(path, home).unwrap_or_else(|| PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_layers_merge_and_resolve_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".nth")).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let global = root.join("global.toml");
        std::fs::write(&global, "model = 'global'\n[skills]\npaths = ['../shared']\n[directories]\npaths = ['../docs']\n[format.mine]\ncommand = ['fmt']\nextensions = ['txt']\nenv = { A = 'a' }\n[lsp.mine]\ncommand = ['server']\nextensions = ['x']\n").unwrap();
        std::fs::write(root.join(".nth/config"), "model = ''\n[context]\nauto = false\nreserved = 0\n[skills]\npaths = ['shared', './shared']\n[directories]\npaths = ['docs', 'other']\n").unwrap();
        let (mut config, sources) =
            Config::load_layers(Some(&global), &root.join("sub"), &Paths::default()).unwrap();
        assert_eq!(sources, [global, root.join(".nth/config")]);
        assert_eq!(config.model, "");
        assert!(!config.context.auto);
        assert_eq!(config.context.reserved, 0);
        assert_eq!(config.skills.paths, [root.join("shared").to_string_lossy()]);
        assert_eq!(config.directories.paths.len(), 2);
        assert_eq!(config.format.formatters["mine"].env.len(), 1);
        assert_eq!(
            config.format.formatters["mine"]
                .extensions
                .as_ref()
                .unwrap(),
            &[".txt"]
        );
        assert_eq!(config.lsp.servers["mine"].command, ["server"]);
        config.set_model("cli".into());
        assert_eq!(config.model, "cli");
    }

    #[test]
    fn layers_reject_hidden_invalid_and_forbidden_values() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".nth")).unwrap();
        let global = root.join("global");
        let repo = root.join(".nth/config");
        for text in [
            "[tools.websearch]\nurl = 'x'",
            "[provider.x]\nbase_url = 'x'",
            "[session]\nmax_step = 2",
            "[format.mine]\ncommand = 3",
            "[format.x]\ncommand = ['fmt']\nextensions = ['.x']",
            "[lsp.x]\ncommand = ['server']\nextensions = ['.x']",
            "[tools.bash]\nrtk = false",
            "[tools.read]\nmax_lines = 10",
        ] {
            std::fs::write(&global, "").unwrap();
            std::fs::write(&repo, text).unwrap();
            let err = Config::load_in(Some(&global), root, &Paths::default()).unwrap_err();
            assert!(
                format!("{err:#}").contains(&repo.display().to_string()),
                "{text}: {err:#}"
            );
        }
        std::fs::write(&global, "[session]\nmax_steps = 'bad'").unwrap();
        std::fs::write(&repo, "[session]\nmax_steps = 5").unwrap();
        let err = Config::load_in(Some(&global), root, &Paths::default()).unwrap_err();
        assert!(format!("{err:#}").contains(&global.display().to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn a_repository_config_cannot_add_directories_outside_the_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let sibling = tmp.path().join("sibling");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".nth/worktrees/w")).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::os::unix::fs::symlink(&sibling, root.join("link")).unwrap();
        // Points outside at something that does not exist yet.
        std::os::unix::fs::symlink(sibling.join("later"), root.join("dangling")).unwrap();
        std::os::unix::fs::symlink("loop-b", root.join("loop-a")).unwrap();
        std::os::unix::fs::symlink("loop-a", root.join("loop-b")).unwrap();
        let global = tmp.path().join("global.toml");
        let repo = root.join(".nth/config");
        std::fs::write(&global, "").unwrap();
        let sibling = sibling.canonicalize().unwrap();
        for section in ["skills", "directories"] {
            for entry in [
                sibling.to_str().unwrap(),
                "../sibling",
                "link",
                "link/x",
                "missing/../link",
                "missing/../link/x",
                "dangling",
                "dangling/x",
                "missing/../dangling",
            ] {
                std::fs::write(&repo, format!("[{section}]\npaths = ['{entry}']")).unwrap();
                let err = Config::load_in(Some(&global), &root, &Paths::default()).unwrap_err();
                let message = format!("{err:#}");
                assert!(
                    message.starts_with(&format!("invalid config {}: ", repo.display())),
                    "{entry}: {message}"
                );
                assert!(
                    message.contains(&format!("{section}.paths entry {}", sibling.display())),
                    "{entry}: {message}"
                );
                assert!(
                    message.ends_with(&format!(
                        "is outside the repository; put it in {}",
                        global.display()
                    )),
                    "{entry}: {message}"
                );
            }
            // Paths under the root stay allowed, worktrees included.
            std::fs::write(
                &repo,
                format!("[{section}]\npaths = ['docs', '.nth/worktrees/w', 'sub/../x']"),
            )
            .unwrap();
            Config::load_in(Some(&global), &root, &Paths::default()).unwrap();
            // The global config may name any path.
            std::fs::write(&repo, "").unwrap();
            std::fs::write(
                &global,
                format!("[{section}]\npaths = ['{}']", sibling.display()),
            )
            .unwrap();
            Config::load_in(Some(&global), &root, &Paths::default()).unwrap();
            std::fs::write(&global, "").unwrap();
        }
        // A symlink loop is refused rather than kept as written, in a
        // project config only.
        for entry in ["loop-a", "loop-a/x", "missing/../loop-b"] {
            std::fs::write(&repo, format!("[directories]\npaths = ['{entry}']")).unwrap();
            let err = Config::load_in(Some(&global), &root, &Paths::default()).unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.starts_with(&format!(
                    "invalid config {}: directories.paths entry {entry}: ",
                    repo.display()
                )),
                "{entry}: {message}"
            );
            assert!(message.ends_with("is a symlink loop"), "{entry}: {message}");
        }
        std::fs::write(&repo, "").unwrap();
        std::fs::write(
            &global,
            format!(
                "[directories]\npaths = ['{}']",
                root.join("loop-a").display()
            ),
        )
        .unwrap();
        Config::load_in(Some(&global), &root, &Paths::default()).unwrap();
        std::fs::write(&global, "").unwrap();
        // Outside Git the root is the folder holding `.nth`.
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(plain.join(".nth")).unwrap();
        std::fs::create_dir_all(plain.join("docs")).unwrap();
        std::fs::write(plain.join(".nth/config"), "[directories]\npaths = ['docs']").unwrap();
        Config::load_layers(Some(&global), &plain, &Paths::default()).unwrap();
        std::fs::write(
            plain.join(".nth/config"),
            "[directories]\npaths = ['../sibling']",
        )
        .unwrap();
        let err = Config::load_layers(Some(&global), &plain, &Paths::default()).unwrap_err();
        assert!(
            format!("{err:#}").contains("is outside the repository"),
            "{err:#}"
        );
    }

    #[test]
    fn nearest_non_git_config_and_git_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".nth")).unwrap();
        std::fs::write(root.join(".nth/config"), "model = 'outer'").unwrap();
        std::fs::create_dir_all(root.join("nested/.nth")).unwrap();
        std::fs::write(root.join("nested/.nth/config"), "model = 'inner'").unwrap();
        std::fs::create_dir_all(root.join("nested/deeper")).unwrap();
        let global = root.join("global");
        std::fs::write(&global, "").unwrap();
        let model = |cwd: &Path, home: Option<&Path>| {
            let paths = Paths {
                home: home.map(Path::to_path_buf),
                ..Paths::default()
            };
            Config::load_layers(Some(&global), cwd, &paths)
                .unwrap()
                .0
                .model
        };
        assert_eq!(model(&root.join("nested/deeper"), Some(root)), "inner");
        std::fs::remove_file(root.join("nested/.nth/config")).unwrap();
        // Under home the walk goes up to and including home.
        assert_eq!(model(&root.join("nested/deeper"), Some(root)), "outer");
        // A config above home is ignored.
        assert_eq!(
            model(&root.join("nested/deeper"), Some(&root.join("nested"))),
            ""
        );
        // Outside home only cwd's own config counts.
        let home = root.join("home");
        assert_eq!(model(&root.join("nested/deeper"), Some(&home)), "");
        assert_eq!(model(root, Some(&home)), "outer");
        // A repository stops the walk at its root.
        std::fs::create_dir_all(root.join("nested/.git")).unwrap();
        assert_eq!(model(&root.join("nested"), Some(root)), "");
    }

    #[test]
    fn linked_worktree_uses_main_checkout_config() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("main");
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(root.join(".git/worktrees/w")).unwrap();
        std::fs::create_dir_all(root.join(".nth")).unwrap();
        std::fs::create_dir_all(worktree.join(".nth")).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}", root.join(".git/worktrees/w").display()),
        )
        .unwrap();
        std::fs::write(
            root.join(".nth/config"),
            "model = 'main'\n[skills]\npaths = ['skills']",
        )
        .unwrap();
        std::fs::write(worktree.join(".nth/config"), "model = 'worktree'").unwrap();
        let global = tmp.path().join("global");
        std::fs::write(&global, "").unwrap();
        let config = Config::load_in(Some(&global), &worktree, &Paths::default()).unwrap();
        assert_eq!(config.model, "main");
        assert_eq!(config.skills.paths, [root.join("skills").to_string_lossy()]);
    }

    #[test]
    fn skill_paths_expand_home() {
        let config = Config::parse(
            r#"
            [skills]
            paths = ["~/shared/skills", "tools/skills", "/opt/skills"]
            "#,
        )
        .expect("parses");
        let home = Path::new("/home/k");

        let paths: Vec<_> = config
            .skills
            .paths
            .iter()
            .map(|p| skill_path(p, Some(home)))
            .collect();

        assert_eq!(
            paths,
            [
                PathBuf::from("/home/k/shared/skills"),
                PathBuf::from("tools/skills"),
                PathBuf::from("/opt/skills"),
            ]
        );
    }

    #[test]
    fn explicit_path_must_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("config.toml");
        let err = Config::load_in(Some(&missing), dir.path(), &Paths::default())
            .expect_err("missing file");
        assert!(format!("{err:#}").contains("not found"), "{err:#}");

        std::fs::write(&missing, "[session]\nmax_steps = 3\n").expect("write");
        let config = Config::load_in(Some(&missing), dir.path(), &Paths::default()).expect("loads");
        assert_eq!(config.session.max_steps, 3);
    }

    #[cfg(unix)]
    #[test]
    fn path_resolution_follows_symlinks_before_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(shared.join("team")).unwrap();
        std::fs::create_dir_all(shared.join("docs")).unwrap();
        std::os::unix::fs::symlink(shared.join("team"), repo.join("link")).unwrap();
        assert_eq!(
            resolve_path(&repo.join("link/../docs")).unwrap(),
            shared.join("docs").canonicalize().unwrap()
        );
    }

    #[test]
    fn an_empty_global_formatter_command_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        std::fs::write(
            &global,
            "[format.custom]\ncommand = []\nextensions = ['.x']\n",
        )
        .unwrap();
        let error = Config::load_in(Some(&global), dir.path(), &Paths::default()).unwrap_err();
        assert!(
            format!("{error:#}").contains("command is empty"),
            "{error:#}"
        );
        assert!(format!("{error:#}").contains(&global.display().to_string()));
    }
}
