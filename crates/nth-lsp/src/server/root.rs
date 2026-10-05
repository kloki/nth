//! Where a language server's project starts for a given file. Ported from
//! the root functions in opencode's `lsp/server.ts`. Blocking file IO.

use std::path::{Path, PathBuf};

use nth_context::project_root;

/// How far up a search may go. opencode stops at the session's directory;
/// nth's pool is shared across sessions, so it stops at the file's git
/// checkout instead, or the filesystem root outside of one.
#[derive(Debug, Clone, PartialEq)]
pub struct Scope {
    stop: Option<PathBuf>,
    /// The root when nothing more specific is found: the checkout, or the
    /// file's own directory.
    pub home: PathBuf,
}

impl Scope {
    pub fn of(file: &Path) -> Self {
        let dir = file.parent().unwrap_or(file);
        let stop = project_root(dir);
        let home = stop.clone().unwrap_or_else(|| dir.to_path_buf());
        Self { stop, home }
    }

    /// The directories from `start` up to and including the stop.
    fn up<'a>(&'a self, start: &'a Path) -> impl Iterator<Item = &'a Path> {
        let mut done = false;
        start.ancestors().take_while(move |d| {
            if done {
                return false;
            }
            done = self.stop.as_deref() == Some(*d);
            true
        })
    }

    fn contains(&self, dir: &Path) -> bool {
        self.stop
            .as_deref()
            .is_none_or(|stop| dir.starts_with(stop))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Root {
    /// The nearest directory holding one of the markers, else the scope's
    /// home. No root at all when an `except` marker is found first.
    Nearest {
        markers: &'static [&'static str],
        except: &'static [&'static str],
    },
    /// Like `Nearest`, but no root when no marker is found.
    Strict {
        markers: &'static [&'static str],
        except: &'static [&'static str],
    },
    /// Always the scope's home, for servers that are fine with any folder.
    Home,
    Rust,
    Go,
    Kotlin,
    Java,
}

pub const fn nearest(markers: &'static [&'static str]) -> Root {
    Root::Nearest {
        markers,
        except: &[],
    }
}

pub const fn strict(markers: &'static [&'static str]) -> Root {
    Root::Strict {
        markers,
        except: &[],
    }
}

impl Root {
    pub fn find(&self, file: &Path, scope: &Scope) -> Option<PathBuf> {
        let start = file.parent()?;
        match *self {
            Root::Nearest { markers, except } => {
                if find_up(except, start, scope).is_some() {
                    return None;
                }
                Some(find_up(markers, start, scope).unwrap_or_else(|| scope.home.clone()))
            }
            Root::Strict { markers, except } => {
                if find_up(except, start, scope).is_some() {
                    return None;
                }
                find_up(markers, start, scope)
            }
            Root::Home => Some(scope.home.clone()),
            Root::Rust => rust(start, scope),
            // opencode chains `NearestRoot`s here, which never come back
            // empty, so only the first ever counted; these are the intended
            // fallbacks.
            Root::Go => find_up(&["go.work"], start, scope)
                .or_else(|| find_up(&["go.mod", "go.sum"], start, scope))
                .or_else(|| Some(scope.home.clone())),
            Root::Kotlin => [
                &["settings.gradle.kts", "settings.gradle"][..],
                &["gradlew", "gradlew.bat"],
                &["build.gradle.kts", "build.gradle"],
                &["pom.xml"],
            ]
            .iter()
            .find_map(|markers| find_up(markers, start, scope))
            .or_else(|| Some(scope.home.clone())),
            Root::Java => java(start, scope),
        }
    }
}

/// The nearest crate, or the workspace above it when there is one.
fn rust(start: &Path, scope: &Scope) -> Option<PathBuf> {
    let krate =
        find_up(&["Cargo.toml", "Cargo.lock"], start, scope).unwrap_or_else(|| scope.home.clone());
    let workspace = krate
        .ancestors()
        .take_while(|d| scope.contains(d))
        .find(|d| {
            std::fs::read_to_string(d.join("Cargo.toml"))
                .is_ok_and(|toml| toml.contains("[workspace]"))
        })
        .map(Path::to_path_buf);
    Some(workspace.unwrap_or(krate))
}

/// Gradle first, then a chain of Maven modules, then an Eclipse project.
fn java(start: &Path, scope: &Scope) -> Option<PathBuf> {
    const SETTINGS: &[&str] = &["settings.gradle", "settings.gradle.kts"];
    if find_up(SETTINGS, start, scope).is_none()
        && let Some(root) = find_up(&["gradlew", "gradlew.bat"], start, scope)
    {
        return Some(root);
    }
    if let Some(root) = find_up(SETTINGS, start, scope)
        .or_else(|| find_up(&["build.gradle", "build.gradle.kts"], start, scope))
    {
        return Some(root);
    }
    let poms: Vec<&Path> = scope
        .up(start)
        .filter(|d| d.join("pom.xml").is_file())
        .collect();
    if let Some((first, parents)) = poms.split_first() {
        let mut root = *first;
        for parent in parents {
            let declared = std::fs::read_to_string(parent.join("pom.xml"))
                .is_ok_and(|pom| declares_module(&pom, root, parent));
            if !declared {
                break;
            }
            root = parent;
        }
        return Some(root.to_path_buf());
    }
    find_up(&[".project", ".classpath"], start, scope)
}

fn declares_module(pom: &str, child: &Path, parent: &Path) -> bool {
    let Ok(rel) = child.strip_prefix(parent) else {
        return false;
    };
    let rel = rel.to_string_lossy();
    if rel.is_empty() {
        return false;
    }
    let mut rest = pom;
    while let Some(open) = rest.find("<module>") {
        rest = &rest[open + "<module>".len()..];
        let Some(close) = rest.find("</module>") else {
            break;
        };
        let decl = rest[..close].trim();
        let decl = decl
            .strip_prefix("./")
            .unwrap_or(decl)
            .trim_end_matches('/');
        if decl == rel {
            return true;
        }
        rest = &rest[close..];
    }
    false
}

/// The nearest directory, from `start` up, that holds any of `markers`.
fn find_up(markers: &[&str], start: &Path, scope: &Scope) -> Option<PathBuf> {
    if markers.is_empty() {
        return None;
    }
    scope
        .up(start)
        .find(|dir| markers.iter().any(|m| has_marker(dir, m)))
        .map(Path::to_path_buf)
}

/// A marker is a relative path, or `*.ext` for any file with that suffix.
fn has_marker(dir: &Path, marker: &str) -> bool {
    match marker.strip_prefix('*') {
        Some(suffix) => std::fs::read_dir(dir).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(suffix))
        }),
        None => dir.join(marker).exists(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn touch(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn rust_prefers_the_workspace_above_the_crate() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        fs::create_dir(repo.join(".git")).unwrap();
        touch(
            &repo.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        );
        touch(
            &repo.join("crates/a/Cargo.toml"),
            "[package]\nname = \"a\"\n",
        );
        let file = repo.join("crates/a/src/lib.rs");
        touch(&file, "");

        let scope = Scope::of(&file);
        assert_eq!(scope.home, repo);
        assert_eq!(Root::Rust.find(&file, &scope).unwrap(), repo);
    }

    #[test]
    fn rust_without_a_workspace_is_the_crate() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        fs::create_dir(repo.join(".git")).unwrap();
        touch(
            &repo.join("tools/x/Cargo.toml"),
            "[package]\nname = \"x\"\n",
        );
        let file = repo.join("tools/x/src/main.rs");
        touch(&file, "");

        let root = Root::Rust.find(&file, &Scope::of(&file)).unwrap();
        assert_eq!(root, repo.join("tools/x"));
    }

    #[test]
    fn the_search_stops_at_the_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        // A marker above the checkout must not count.
        touch(&tmp.path().join("go.mod"), "");
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let file = repo.join("pkg/main.go");
        touch(&file, "");

        let scope = Scope::of(&file);
        assert_eq!(Root::Go.find(&file, &scope).unwrap(), repo);
        assert_eq!(strict(&["go.mod"]).find(&file, &scope), None);
    }

    #[test]
    fn nearest_falls_back_and_except_vetoes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        fs::create_dir(repo.join(".git")).unwrap();
        touch(&repo.join("web/package-lock.json"), "");
        let file = repo.join("web/src/app.ts");
        touch(&file, "");
        let scope = Scope::of(&file);

        assert_eq!(
            nearest(&["package-lock.json"]).find(&file, &scope).unwrap(),
            repo.join("web")
        );
        assert_eq!(nearest(&["yarn.lock"]).find(&file, &scope).unwrap(), repo);

        touch(&repo.join("deno.json"), "{}");
        let ts = Root::Nearest {
            markers: &["package-lock.json"],
            except: &["deno.json"],
        };
        assert_eq!(ts.find(&file, &scope), None);
        assert_eq!(strict(&["deno.json"]).find(&file, &scope).unwrap(), repo);
    }

    #[test]
    fn glob_markers_match_suffixes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        fs::create_dir(repo.join(".git")).unwrap();
        touch(&repo.join("hs/thing.cabal"), "");
        let file = repo.join("hs/src/Main.hs");
        touch(&file, "");
        let root = strict(&["*.cabal"]).find(&file, &Scope::of(&file)).unwrap();
        assert_eq!(root, repo.join("hs"));
    }

    #[test]
    fn java_follows_maven_modules_up() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        fs::create_dir(repo.join(".git")).unwrap();
        touch(
            &repo.join("pom.xml"),
            "<modules><module>./core/</module></modules>",
        );
        touch(&repo.join("core/pom.xml"), "");
        touch(&repo.join("lone/pom.xml"), "");
        let core = repo.join("core/src/A.java");
        let lone = repo.join("lone/src/B.java");
        touch(&core, "");
        touch(&lone, "");

        assert_eq!(Root::Java.find(&core, &Scope::of(&core)).unwrap(), repo);
        assert_eq!(
            Root::Java.find(&lone, &Scope::of(&lone)).unwrap(),
            repo.join("lone")
        );
    }
}
