//! The Rust edition of the crate a file belongs to, for rustfmt's
//! `--edition`. Run on a lone file, rustfmt reads no `Cargo.toml` and
//! parses it as 2015, so `async fn` fails with E0670 and the style is not
//! the one `cargo fmt` gives.

use std::path::Path;

use toml::{Table, Value};

/// What a command writes for the edition.
pub(crate) const PLACEHOLDER: &str = "$RUST_EDITION";

/// Cargo's own default for a package that names no edition, and the answer
/// when none can be found, so the placeholder always resolves.
const DEFAULT: &str = "2015";

/// The edition of the package holding `file`, looking no higher than
/// `root`: a manifest above the project is someone else's.
pub(crate) async fn of(file: &Path, root: &Path) -> String {
    let dirs: Vec<&Path> = file
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(root))
        .collect();
    for (at, dir) in dirs.iter().enumerate() {
        let Some(manifest) = manifest(dir).await else {
            continue;
        };
        // A virtual workspace's manifest has no package, and its members'
        // files are not its own.
        let Some(package) = manifest.get("package") else {
            continue;
        };
        return match package.get("edition") {
            Some(Value::String(edition)) => Some(edition.clone()),
            // A package can be its own workspace root, so its own manifest
            // is the first place to look for the workspace.
            Some(edition) if inherits(edition) => workspace_edition(&dirs[at..]).await,
            _ => None,
        }
        .unwrap_or_else(|| DEFAULT.into());
    }
    DEFAULT.into()
}

/// `edition.workspace = true`, or the same written inline.
fn inherits(edition: &Value) -> bool {
    edition.get("workspace").and_then(Value::as_bool) == Some(true)
}

/// `workspace.package.edition` of the first workspace manifest in `dirs`.
async fn workspace_edition(dirs: &[&Path]) -> Option<String> {
    for dir in dirs {
        let Some(manifest) = manifest(dir).await else {
            continue;
        };
        if let Some(workspace) = manifest.get("workspace") {
            return workspace
                .get("package")?
                .get("edition")?
                .as_str()
                .map(String::from);
        }
    }
    None
}

/// The `Cargo.toml` in `dir`, or none when it is missing or will not parse:
/// either way it says nothing about the edition.
async fn manifest(dir: &Path) -> Option<Table> {
    let text = tokio::fs::read_to_string(dir.join("Cargo.toml"))
        .await
        .ok()?;
    text.parse().ok()
}
