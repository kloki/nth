//! The language servers nth knows, ported from opencode's `lsp/server.ts` —
//! but for ruff, which nth adds itself, so Python files get the linter's
//! complaints next to pyright's type errors. Only servers already on PATH are
//! used: nth never downloads one, so those opencode can only run from a
//! download (eslint, razor) are left out.

mod root;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub use root::Scope;
use root::{Root, nearest, strict};
use serde_json::{Value, json};

use crate::{config::LspConfig, language};

const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "bun.lockb",
    "bun.lock",
    "pnpm-lock.yaml",
    "yarn.lock",
];
const JS: &[&str] = &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"];

struct Builtin {
    id: &'static str,
    extensions: &'static [&'static str],
    root: Root,
    /// Tried in order; the first whose program is on PATH runs.
    commands: &'static [&'static [&'static str]],
    init: Init,
}

/// Initialization options worked out from the project at spawn time.
#[derive(Debug, Clone, Copy)]
enum Init {
    None,
    /// An empty object, which some servers want rather than nothing.
    Empty,
    /// The virtualenv's python, when there is one.
    Pyright,
    /// The project's own `tsserver.js`; the server does not start without it.
    Typescript,
    /// Its `tsdk`, likewise required.
    Astro,
    Terraform,
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        id: "deno",
        extensions: &[".ts", ".tsx", ".js", ".jsx", ".mjs"],
        root: strict(&["deno.json", "deno.jsonc"]),
        commands: &[&["deno", "lsp"]],
        init: Init::None,
    },
    Builtin {
        id: "typescript",
        extensions: JS,
        root: Root::Nearest {
            markers: LOCKFILES,
            except: &["deno.json", "deno.jsonc"],
        },
        commands: &[&["typescript-language-server", "--stdio"]],
        init: Init::Typescript,
    },
    Builtin {
        id: "vue",
        extensions: &[".vue"],
        root: nearest(LOCKFILES),
        commands: &[&["vue-language-server", "--stdio"]],
        init: Init::Empty,
    },
    Builtin {
        id: "oxlint",
        extensions: &[
            ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue", ".astro",
            ".svelte",
        ],
        root: nearest(&[
            ".oxlintrc.json",
            "package-lock.json",
            "bun.lockb",
            "bun.lock",
            "pnpm-lock.yaml",
            "yarn.lock",
            "package.json",
        ]),
        commands: &[&["oxc_language_server"], &["oxlint", "--lsp"]],
        init: Init::None,
    },
    Builtin {
        id: "biome",
        extensions: &[
            ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".json", ".jsonc",
            ".vue", ".astro", ".svelte", ".css", ".graphql", ".gql", ".html",
        ],
        root: nearest(&[
            "biome.json",
            "biome.jsonc",
            "package-lock.json",
            "bun.lockb",
            "bun.lock",
            "pnpm-lock.yaml",
            "yarn.lock",
        ]),
        commands: &[&["biome", "lsp-proxy", "--stdio"]],
        init: Init::None,
    },
    Builtin {
        id: "gopls",
        extensions: &[".go"],
        root: Root::Go,
        commands: &[&["gopls"]],
        init: Init::None,
    },
    Builtin {
        id: "ruby-lsp",
        extensions: &[".rb", ".rake", ".gemspec", ".ru"],
        root: nearest(&["Gemfile"]),
        commands: &[
            &["ruby-lsp"],
            &["solargraph", "stdio"],
            &["rubocop", "--lsp"],
        ],
        init: Init::None,
    },
    Builtin {
        id: "pyright",
        extensions: &[".py", ".pyi"],
        root: nearest(&[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
            "Pipfile",
            "pyrightconfig.json",
        ]),
        commands: &[
            &["pyright-langserver", "--stdio"],
            &["basedpyright-langserver", "--stdio"],
        ],
        init: Init::Pyright,
    },
    Builtin {
        id: "ruff",
        extensions: &[".py", ".pyi"],
        root: nearest(&[
            "ruff.toml",
            ".ruff.toml",
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
            "Pipfile",
        ]),
        // `ruff server` since ruff 0.5; ruff-lsp for older installs.
        commands: &[&["ruff", "server"], &["ruff-lsp"]],
        init: Init::None,
    },
    Builtin {
        id: "elixir-ls",
        extensions: &[".ex", ".exs"],
        root: nearest(&["mix.exs", "mix.lock"]),
        commands: &[&["elixir-ls"], &["language_server.sh"]],
        init: Init::None,
    },
    Builtin {
        id: "zls",
        extensions: &[".zig", ".zon"],
        root: nearest(&["build.zig"]),
        commands: &[&["zls"]],
        init: Init::None,
    },
    Builtin {
        id: "csharp",
        extensions: &[".cs", ".csx"],
        root: nearest(&["*.slnx", "*.sln", "*.csproj", "global.json"]),
        commands: &[&["roslyn-language-server", "--stdio", "--autoLoadProjects"]],
        init: Init::None,
    },
    Builtin {
        id: "fsharp",
        extensions: &[".fs", ".fsi", ".fsx", ".fsscript"],
        root: nearest(&["*.slnx", "*.sln", "*.fsproj", "global.json"]),
        commands: &[&["fsautocomplete"]],
        init: Init::None,
    },
    Builtin {
        id: "sourcekit-lsp",
        extensions: &[".swift", ".objc", "objcpp"],
        root: nearest(&["Package.swift", "*.xcodeproj", "*.xcworkspace"]),
        commands: &[&["sourcekit-lsp"]],
        init: Init::None,
    },
    Builtin {
        id: "rust",
        extensions: &[".rs"],
        root: Root::Rust,
        commands: &[&["rust-analyzer"]],
        init: Init::None,
    },
    Builtin {
        id: "clangd",
        extensions: &[
            ".c", ".cpp", ".cc", ".cxx", ".c++", ".h", ".hpp", ".hh", ".hxx", ".h++",
        ],
        root: nearest(&["compile_commands.json", "compile_flags.txt", ".clangd"]),
        commands: &[&["clangd", "--background-index", "--clang-tidy"]],
        init: Init::None,
    },
    Builtin {
        id: "svelte",
        extensions: &[".svelte"],
        root: nearest(LOCKFILES),
        commands: &[&["svelteserver", "--stdio"]],
        init: Init::None,
    },
    Builtin {
        id: "astro",
        extensions: &[".astro"],
        root: nearest(LOCKFILES),
        commands: &[&["astro-ls", "--stdio"]],
        init: Init::Astro,
    },
    Builtin {
        id: "jdtls",
        extensions: &[".java"],
        root: Root::Java,
        commands: &[&["jdtls"]],
        init: Init::None,
    },
    Builtin {
        id: "kotlin-ls",
        extensions: &[".kt", ".kts"],
        root: Root::Kotlin,
        commands: &[&["kotlin-lsp", "--stdio"]],
        init: Init::None,
    },
    Builtin {
        id: "yaml-ls",
        extensions: &[".yaml", ".yml"],
        root: nearest(LOCKFILES),
        commands: &[&["yaml-language-server", "--stdio"]],
        init: Init::None,
    },
    Builtin {
        id: "lua-ls",
        extensions: &[".lua"],
        root: nearest(&[
            ".luarc.json",
            ".luarc.jsonc",
            ".luacheckrc",
            ".stylua.toml",
            "stylua.toml",
            "selene.toml",
            "selene.yml",
        ]),
        commands: &[&["lua-language-server"]],
        init: Init::None,
    },
    Builtin {
        id: "php intelephense",
        extensions: &[".php"],
        root: nearest(&["composer.json", "composer.lock", ".php-version"]),
        commands: &[&["intelephense", "--stdio"]],
        init: Init::None,
    },
    Builtin {
        id: "prisma",
        extensions: &[".prisma"],
        root: Root::Nearest {
            markers: &["schema.prisma", "prisma/schema.prisma", "prisma"],
            except: &["package.json"],
        },
        commands: &[&["prisma", "language-server"]],
        init: Init::None,
    },
    Builtin {
        id: "dart",
        extensions: &[".dart"],
        root: nearest(&["pubspec.yaml", "analysis_options.yaml"]),
        commands: &[&["dart", "language-server", "--lsp"]],
        init: Init::None,
    },
    Builtin {
        id: "ocaml-lsp",
        extensions: &[".ml", ".mli"],
        root: nearest(&["dune-project", "dune-workspace", ".merlin", "opam"]),
        commands: &[&["ocamllsp"]],
        init: Init::None,
    },
    Builtin {
        id: "bash",
        extensions: &[".sh", ".bash", ".zsh", ".ksh"],
        root: Root::Home,
        commands: &[&["bash-language-server", "start"]],
        init: Init::None,
    },
    Builtin {
        id: "terraform",
        extensions: &[".tf", ".tfvars"],
        root: nearest(&[".terraform.lock.hcl", "terraform.tfstate", "*.tf"]),
        commands: &[&["terraform-ls", "serve"]],
        init: Init::Terraform,
    },
    Builtin {
        id: "texlab",
        extensions: &[".tex", ".bib"],
        root: nearest(&[".latexmkrc", "latexmkrc", ".texlabroot", "texlabroot"]),
        commands: &[&["texlab"]],
        init: Init::None,
    },
    Builtin {
        id: "dockerfile",
        extensions: &[".dockerfile", "Dockerfile"],
        root: Root::Home,
        commands: &[&["docker-langserver", "--stdio"]],
        init: Init::None,
    },
    Builtin {
        id: "gleam",
        extensions: &[".gleam"],
        root: nearest(&["gleam.toml"]),
        commands: &[&["gleam", "lsp"]],
        init: Init::None,
    },
    Builtin {
        id: "clojure-lsp",
        extensions: &[".clj", ".cljs", ".cljc", ".edn"],
        root: nearest(&[
            "deps.edn",
            "project.clj",
            "shadow-cljs.edn",
            "bb.edn",
            "build.boot",
        ]),
        commands: &[&["clojure-lsp", "listen"]],
        init: Init::None,
    },
    Builtin {
        id: "nixd",
        extensions: &[".nix"],
        root: nearest(&["flake.nix"]),
        commands: &[&["nixd"]],
        init: Init::None,
    },
    Builtin {
        id: "tinymist",
        extensions: &[".typ", ".typc"],
        root: nearest(&["typst.toml"]),
        commands: &[&["tinymist"]],
        init: Init::None,
    },
    Builtin {
        id: "haskell-language-server",
        extensions: &[".hs", ".lhs"],
        root: nearest(&["stack.yaml", "cabal.project", "hie.yaml", "*.cabal"]),
        commands: &[&["haskell-language-server-wrapper", "--lsp"]],
        init: Init::None,
    },
    Builtin {
        id: "julials",
        extensions: &[".jl"],
        root: nearest(&["Project.toml", "Manifest.toml", "*.jl"]),
        commands: &[&[
            "julia",
            "--startup-file=no",
            "--history-file=no",
            "-e",
            "using LanguageServer; runserver()",
        ]],
        init: Init::None,
    },
];

pub fn is_builtin(id: &str) -> bool {
    BUILTINS.iter().any(|b| b.id == id)
}

/// One server nth may start, after the config has had its say.
#[derive(Debug, Clone)]
pub struct Server {
    pub id: String,
    /// With the dot. Empty matches every file.
    pub extensions: Vec<String>,
    root: Root,
    commands: Vec<Vec<String>>,
    init: Init,
    env: BTreeMap<String, String>,
    initialization: Option<Value>,
}

/// Everything needed to start one server for one root.
#[derive(Debug, Clone, PartialEq)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub root: PathBuf,
    pub initialization: Option<Value>,
}

/// The built-in servers with the config applied: disabled ones dropped,
/// overrides merged in, custom ones added. Empty when LSP is off.
pub fn registry(config: &LspConfig) -> Vec<Server> {
    if !config.enabled {
        return Vec::new();
    }
    let mut servers = Vec::with_capacity(BUILTINS.len() + config.servers.len());
    for b in BUILTINS {
        servers.push(Server {
            id: b.id.into(),
            extensions: b.extensions.iter().map(|e| e.to_string()).collect(),
            root: b.root,
            commands: b
                .commands
                .iter()
                .map(|c| c.iter().map(|a| a.to_string()).collect())
                .collect(),
            init: b.init,
            env: BTreeMap::new(),
            initialization: None,
        });
    }
    for (id, cfg) in &config.servers {
        let existing = servers.iter().position(|s| s.id == *id);
        if cfg.disabled {
            if let Some(i) = existing {
                servers.remove(i);
            }
            continue;
        }
        let server = match existing {
            Some(i) => &mut servers[i],
            None => {
                servers.push(Server {
                    id: id.clone(),
                    extensions: Vec::new(),
                    root: Root::Home,
                    commands: Vec::new(),
                    init: Init::None,
                    env: BTreeMap::new(),
                    initialization: None,
                });
                let last = servers.len() - 1;
                &mut servers[last]
            }
        };
        if !cfg.command.is_empty() {
            server.commands = vec![cfg.command.clone()];
        }
        if !cfg.extensions.is_empty() {
            server.extensions = cfg.extensions.clone();
        }
        server.env = cfg.env.clone();
        if cfg.initialization.is_some() {
            server.initialization = cfg.initialization.clone();
            server.init = Init::None;
        }
    }
    servers
}

impl Server {
    pub fn handles(&self, file: &Path) -> bool {
        let key = language::extension_key(file);
        self.extensions.is_empty() || self.extensions.contains(&key)
    }

    /// The project root this server would run in for `file`, if any.
    pub fn root(&self, file: &Path, scope: &Scope) -> Option<PathBuf> {
        self.root.find(file, scope)
    }

    /// The first command whose program is on PATH, with the program
    /// resolved. Blocking.
    pub fn program(&self) -> Option<(PathBuf, Vec<String>)> {
        self.commands.iter().find_map(|command| {
            let (program, args) = command.split_first()?;
            Some((which(program)?, args.to_vec()))
        })
    }

    /// How to start this server in `root`, or `None` when it cannot run
    /// there. Blocking.
    pub fn launch(&self, root: &Path) -> Option<Launch> {
        let (program, args) = self.program()?;
        let initialization = match self.init {
            Init::None => self.initialization.clone(),
            Init::Empty => Some(json!({})),
            Init::Pyright => {
                let venvs = std::env::var_os("VIRTUAL_ENV")
                    .map(PathBuf::from)
                    .into_iter()
                    .chain([root.join(".venv"), root.join("venv")]);
                let python = venvs
                    .map(|venv| venv.join("bin/python"))
                    .find(|python| python.exists());
                Some(match python {
                    Some(python) => json!({ "pythonPath": python }),
                    None => json!({}),
                })
            }
            Init::Typescript => Some(json!({ "tsserver": { "path": tsserver(root)? } })),
            Init::Astro => {
                let tsdk = tsserver(root)?.parent()?.to_path_buf();
                Some(json!({ "typescript": { "tsdk": tsdk } }))
            }
            Init::Terraform => Some(json!({
                "experimentalFeatures": { "prefillRequiredFields": true, "validateOnSave": true }
            })),
        };
        Some(Launch {
            program,
            args,
            env: self.env.clone(),
            root: root.to_path_buf(),
            initialization,
        })
    }
}

/// The project's TypeScript, found the way Node resolves a module.
fn tsserver(root: &Path) -> Option<PathBuf> {
    root.ancestors()
        .map(|d| d.join("node_modules/typescript/lib/tsserver.js"))
        .find(|p| p.is_file())
}

/// A program by name on PATH, or as given when it is a path already.
pub fn which(program: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        let path = PathBuf::from(program);
        return executable(&path).then_some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|p| executable(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;

    fn find<'a>(servers: &'a [Server], id: &str) -> Option<&'a Server> {
        servers.iter().find(|s| s.id == id)
    }

    #[test]
    fn config_disables_overrides_and_adds() {
        let mut config = LspConfig::default();
        config.servers.insert(
            "pyright".into(),
            ServerConfig {
                disabled: true,
                ..Default::default()
            },
        );
        config.servers.insert(
            "rust".into(),
            ServerConfig {
                command: vec!["ra-multiplex".into()],
                ..Default::default()
            },
        );
        config.servers.insert(
            "mine".into(),
            ServerConfig {
                command: vec!["mine".into()],
                extensions: vec![".foo".into()],
                ..Default::default()
            },
        );
        let servers = registry(&config);

        assert!(find(&servers, "pyright").is_none());
        let rust = find(&servers, "rust").unwrap();
        assert_eq!(rust.commands, [["ra-multiplex"]]);
        assert_eq!(rust.extensions, [".rs"]);
        let mine = find(&servers, "mine").unwrap();
        assert!(mine.handles(Path::new("/a/b.foo")));
        assert!(!mine.handles(Path::new("/a/b.rs")));
    }

    #[test]
    fn nothing_when_disabled() {
        let config = LspConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(registry(&config).is_empty());
    }

    #[test]
    fn which_needs_an_executable() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-program-nth").is_none());
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain");
        std::fs::write(&plain, "").unwrap();
        assert!(which(plain.to_str().unwrap()).is_none());
    }

    #[test]
    fn dockerfiles_match_by_name() {
        let servers = registry(&LspConfig::default());
        let docker = find(&servers, "dockerfile").unwrap();
        assert!(docker.handles(Path::new("/p/Dockerfile")));
        assert!(!docker.handles(Path::new("/p/Makefile")));
    }

    #[test]
    fn ruff_lints_python_alongside_pyright() {
        let servers = registry(&LspConfig::default());
        let ruff = find(&servers, "ruff").unwrap();
        assert_eq!(ruff.extensions, [".py", ".pyi"]);
        assert!(ruff.handles(Path::new("/p/a.py")));
        assert!(!ruff.handles(Path::new("/p/a.rs")));
        assert_eq!(ruff.commands, [vec!["ruff", "server"], vec!["ruff-lsp"]]);
        assert!(find(&servers, "pyright").is_some());
    }
}
