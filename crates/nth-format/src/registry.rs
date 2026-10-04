//! The built-in formatters, ported from opencode's `format/formatter.ts`.
//! Each runs only when its program is already installed; nth never
//! downloads one.

/// A formatter nth knows without any config.
#[derive(Debug)]
pub struct Builtin {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    /// The program, then its arguments, with `$FILE` for the file. The
    /// program is looked up as `probe` says.
    pub command: &'static [&'static str],
    pub probe: Probe,
    /// Skipped when this other formatter is enabled, since both would
    /// format the same files the same way.
    pub yields_to: Option<&'static str>,
}

/// How a formatter decides it applies to a project. Files are looked for
/// in the project directory and every directory above it.
#[derive(Debug)]
pub enum Probe {
    /// The program is on `PATH`.
    OnPath,
    /// The program is on `PATH` and one of these files exists.
    Marker(&'static [&'static str]),
    /// A `package.json` lists this package as a dependency, and the
    /// program is in `node_modules/.bin` or on `PATH`.
    NodeDep(&'static str),
    /// One of these files exists, and the program is in
    /// `node_modules/.bin` or on `PATH`.
    NodeMarker(&'static [&'static str]),
    /// ruff is on `PATH` and the project uses it: a `ruff.toml`, a
    /// `[tool.ruff]` table in `pyproject.toml`, or ruff in its dependencies.
    Ruff,
    /// The program is on `PATH`, running it with these arguments succeeds,
    /// and the first line of its output has each of these words, which
    /// tells it apart from other programs of the same name.
    Help {
        args: &'static [&'static str],
        first_line: &'static [&'static str],
    },
}

const WEB: &[&str] = &[
    ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".html", ".htm", ".css", ".scss",
    ".sass", ".less", ".vue", ".svelte", ".json", ".jsonc", ".yaml", ".yml", ".toml", ".xml",
    ".md", ".mdx", ".graphql", ".gql",
];

const RUBY: &[&str] = &[".rb", ".rake", ".gemspec", ".ru"];

const fn on_path(
    name: &'static str,
    extensions: &'static [&'static str],
    command: &'static [&'static str],
) -> Builtin {
    Builtin {
        name,
        extensions,
        command,
        probe: Probe::OnPath,
        yields_to: None,
    }
}

/// In the order they run when several match one file.
pub static BUILTINS: &[Builtin] = &[
    on_path("gofmt", &[".go"], &["gofmt", "-w", "$FILE"]),
    on_path(
        "mix",
        &[".ex", ".exs", ".eex", ".heex", ".leex", ".neex", ".sface"],
        &["mix", "format", "$FILE"],
    ),
    Builtin {
        name: "prettier",
        extensions: WEB,
        command: &["prettier", "--write", "$FILE"],
        probe: Probe::NodeDep("prettier"),
        yields_to: None,
    },
    Builtin {
        name: "biome",
        extensions: WEB,
        command: &["biome", "format", "--write", "$FILE"],
        probe: Probe::NodeMarker(&["biome.json", "biome.jsonc"]),
        yields_to: None,
    },
    on_path("zig", &[".zig", ".zon"], &["zig", "fmt", "$FILE"]),
    Builtin {
        name: "clang-format",
        extensions: &[
            ".c", ".cc", ".cpp", ".cxx", ".c++", ".h", ".hh", ".hpp", ".hxx", ".h++", ".ino", ".C",
            ".H",
        ],
        command: &["clang-format", "-i", "$FILE"],
        probe: Probe::Marker(&[".clang-format"]),
        yields_to: None,
    },
    on_path("ktlint", &[".kt", ".kts"], &["ktlint", "-F", "$FILE"]),
    Builtin {
        name: "ruff",
        extensions: &[".py", ".pyi"],
        command: &["ruff", "format", "$FILE"],
        probe: Probe::Ruff,
        yields_to: None,
    },
    Builtin {
        name: "air",
        extensions: &[".R"],
        command: &["air", "format", "$FILE"],
        probe: Probe::Help {
            args: &["--help"],
            first_line: &["R language", "formatter"],
        },
        yields_to: None,
    },
    Builtin {
        name: "uv",
        extensions: &[".py", ".pyi"],
        command: &["uv", "format", "--", "$FILE"],
        probe: Probe::Help {
            args: &["format", "--help"],
            first_line: &[],
        },
        yields_to: Some("ruff"),
    },
    on_path("rubocop", RUBY, &["rubocop", "--autocorrect", "$FILE"]),
    on_path("standardrb", RUBY, &["standardrb", "--fix", "$FILE"]),
    on_path(
        "htmlbeautifier",
        &[".erb", ".html.erb"],
        &["htmlbeautifier", "$FILE"],
    ),
    on_path("dart", &[".dart"], &["dart", "format", "$FILE"]),
    Builtin {
        name: "ocamlformat",
        extensions: &[".ml", ".mli"],
        command: &["ocamlformat", "-i", "$FILE"],
        probe: Probe::Marker(&[".ocamlformat"]),
        yields_to: None,
    },
    on_path(
        "terraform",
        &[".tf", ".tfvars"],
        &["terraform", "fmt", "$FILE"],
    ),
    on_path(
        "latexindent",
        &[".tex"],
        &["latexindent", "-w", "-s", "$FILE"],
    ),
    on_path("gleam", &[".gleam"], &["gleam", "format", "$FILE"]),
    on_path("shfmt", &[".sh", ".bash"], &["shfmt", "-w", "$FILE"]),
    on_path("nixfmt", &[".nix"], &["nixfmt", "$FILE"]),
    on_path("rustfmt", &[".rs"], &["rustfmt", "$FILE"]),
    on_path("ormolu", &[".hs"], &["ormolu", "-i", "$FILE"]),
    on_path(
        "cljfmt",
        &[".clj", ".cljs", ".cljc", ".edn"],
        &["cljfmt", "fix", "--quiet", "$FILE"],
    ),
    on_path("dfmt", &[".d"], &["dfmt", "-i", "$FILE"]),
];

pub fn find(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_commands_take_the_file() {
        for (i, builtin) in BUILTINS.iter().enumerate() {
            assert!(
                BUILTINS[..i].iter().all(|b| b.name != builtin.name),
                "{} listed twice",
                builtin.name
            );
            assert!(builtin.command.contains(&"$FILE"), "{}", builtin.name);
        }
    }

    #[test]
    fn yielding_formatters_yield_to_known_ones() {
        for name in BUILTINS.iter().filter_map(|b| b.yields_to) {
            assert!(find(name).is_some(), "{name}");
        }
    }
}
