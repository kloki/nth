//! The `[lsp]` config section, shaped like opencode's `lsp` config:
//!
//! ```toml
//! [lsp]
//! enabled = true
//!
//! [lsp.pyright]
//! disabled = true
//!
//! [lsp.my-server]
//! command = ["my-server", "--stdio"]
//! extensions = [".foo"]
//! env = { RUST_LOG = "info" }
//! initialization = { someOption = true }
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::server;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawLspConfig")]
pub struct LspConfig {
    /// `false` turns every language server off.
    pub enabled: bool,
    /// Keyed by server id. A built-in id changes that server; any other id
    /// adds one, and then `command` and `extensions` are required.
    #[serde(flatten)]
    pub servers: BTreeMap<String, ServerConfig>,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            servers: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(skip_serializing_if = "is_false")]
    pub disabled: bool,
    /// Program and arguments. For a built-in id, empty keeps its own command.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// With the dot, as in `.rs`. For a built-in id, empty keeps its own.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Sent as `initializationOptions`, and answers `workspace/configuration`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initialization: Option<serde_json::Value>,
}

fn is_false(b: &bool) -> bool {
    !b
}

#[derive(Deserialize)]
struct RawLspConfig {
    #[serde(default = "enabled_by_default")]
    enabled: bool,
    #[serde(flatten)]
    servers: BTreeMap<String, ServerConfig>,
}

fn enabled_by_default() -> bool {
    true
}

impl TryFrom<RawLspConfig> for LspConfig {
    type Error = String;

    fn try_from(raw: RawLspConfig) -> Result<Self, String> {
        for (id, server) in &raw.servers {
            if server.disabled || server::is_builtin(id) {
                continue;
            }
            if server.command.is_empty() || server.extensions.is_empty() {
                return Err(format!(
                    "lsp.{id}: a custom server needs both `command` and `extensions`"
                ));
            }
        }
        Ok(Self {
            enabled: raw.enabled,
            servers: raw.servers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct File {
        #[serde(default)]
        lsp: LspConfig,
    }

    #[test]
    fn missing_section_is_enabled_with_no_overrides() {
        let file: File = toml::from_str("").unwrap();
        assert_eq!(file.lsp, LspConfig::default());
        assert!(file.lsp.enabled);
    }

    #[test]
    fn reads_servers_as_subtables() {
        let file: File = toml::from_str(
            r#"
            [lsp]
            enabled = true

            [lsp.pyright]
            disabled = true

            [lsp.mine]
            command = ["mine", "--stdio"]
            extensions = [".foo"]
            env = { A = "1" }
            initialization = { nested = { x = 1 } }
            "#,
        )
        .unwrap();
        assert!(file.lsp.servers["pyright"].disabled);
        let mine = &file.lsp.servers["mine"];
        assert_eq!(mine.command, ["mine", "--stdio"]);
        assert_eq!(mine.extensions, [".foo"]);
        assert_eq!(mine.env["A"], "1");
        assert_eq!(
            mine.initialization,
            Some(serde_json::json!({"nested": {"x": 1}}))
        );
    }

    #[test]
    fn round_trips_through_toml() {
        let mut lsp = LspConfig {
            enabled: false,
            ..Default::default()
        };
        lsp.servers.insert(
            "rust".into(),
            ServerConfig {
                command: vec!["ra-multiplex".into()],
                ..Default::default()
            },
        );
        let file = File { lsp };
        let text = toml::to_string(&file).unwrap();
        assert!(text.contains("[lsp.rust]"), "{text}");
        assert_eq!(toml::from_str::<File>(&text).unwrap(), file);
    }

    #[test]
    fn custom_servers_need_extensions() {
        let err = toml::from_str::<File>("[lsp.mine]\ncommand = [\"mine\"]\n").unwrap_err();
        assert!(err.to_string().contains("lsp.mine"), "{err}");
        // A built-in id may leave them out, and a disabled one needs nothing.
        toml::from_str::<File>("[lsp.rust]\ncommand = [\"ra\"]\n[lsp.other]\ndisabled = true\n")
            .unwrap();
    }

    #[test]
    fn disabling_everything() {
        let file: File = toml::from_str("[lsp]\nenabled = false\n").unwrap();
        assert!(!file.lsp.enabled);
    }
}
