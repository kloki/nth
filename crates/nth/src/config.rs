//! `nth config` and `nth init`: the resolved config, and the template
//! written out.

use std::path::Path;

use anyhow::{Context, Result, bail};
use nth_config::{Config, TEMPLATE};
use nth_icons::icons;

use crate::runtime::Runtime;

/// `nth config`: the config in use with every default filled in, and on
/// stderr where it came from.
pub fn show(runtime: &Runtime, config: &Config) -> Result<()> {
    use owo_colors::OwoColorize;

    match runtime.config_file() {
        Some(path) if path.exists() => {
            eprintln!("{} {}", icons().ok.green().bold(), path.display().dimmed())
        }
        Some(path) => eprintln!(
            "{} {}",
            icons().to.cyan().bold(),
            format!("no {}, using defaults", path.display()).dimmed()
        ),
        None => eprintln!(
            "{} {}",
            icons().to.cyan().bold(),
            "no home dir, using defaults".dimmed()
        ),
    }
    let cwd = std::env::current_dir()?;
    if let Some((_, path)) = nth_config::repo_config(&cwd, runtime.env().home.as_deref()) {
        eprintln!("{} {}", icons().ok.green().bold(), path.display().dimmed());
    }
    print!("{}", config.to_toml()?);
    Ok(())
}

/// `nth init`: writes the template to the config path, unless a file is
/// there already and `force` is not set.
pub fn init(runtime: &Runtime, force: bool) -> Result<()> {
    use owo_colors::OwoColorize;

    let Some(path) = runtime.config_file() else {
        bail!("no home dir to put the config in; pass --config");
    };
    write_template(&path, force)?;
    eprintln!(
        "{} wrote {}",
        icons().ok.green().bold(),
        path.display().dimmed()
    );
    eprintln!(
        "{} {}",
        icons().to.cyan().bold(),
        "uncomment a [provider.<id>] in it, export its key and set model".dimmed()
    );
    Ok(())
}

fn write_template(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!("{} exists; --force overwrites it", path.display());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    std::fs::write(path, TEMPLATE).with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use nth_context::Paths;

    use super::*;

    #[test]
    fn init_writes_the_template_and_creates_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a/b/config.toml");

        write_template(&path, false).expect("writes");

        assert_eq!(
            Config::load_in(Some(&path), dir.path(), &Paths::default()).expect("loads"),
            Config::default()
        );
    }

    #[test]
    fn init_refuses_to_overwrite_without_force() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "mine").expect("write");

        let err = write_template(&path, false).expect_err("file exists");
        assert!(err.to_string().contains("--force"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "mine");

        write_template(&path, true).expect("overwrites");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), TEMPLATE);
    }
}
