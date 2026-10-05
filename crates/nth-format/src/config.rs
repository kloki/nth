use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::registry;

/// The `[format]` section: `enabled` plus one `[format.<name>]` table per
/// formatter to change or add. A table named after a built-in formatter
/// changes it; any other name adds a formatter, which then needs `command`
/// and `extensions`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(try_from = "RawFormatConfig")]
pub struct FormatConfig {
    /// `false` turns every formatter off.
    pub enabled: bool,
    #[serde(flatten)]
    pub formatters: BTreeMap<String, FormatterConfig>,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            formatters: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FormatterConfig {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// The program and its arguments; `$FILE` is the file to format. Set
    /// on a built-in formatter, it replaces the built-in check and command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// With the dot, as in `.rs`. Set on a built-in formatter, it replaces
    /// the built-in list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    /// Added to the environment the command runs in.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// What the TOML holds before the custom formatters are checked. `flatten`
/// rules out `deny_unknown_fields` here, but a stray key still fails as a
/// formatter table.
#[derive(Deserialize)]
#[serde(default)]
struct RawFormatConfig {
    enabled: bool,
    #[serde(flatten)]
    formatters: BTreeMap<String, FormatterConfig>,
}

impl Default for RawFormatConfig {
    fn default() -> Self {
        let FormatConfig {
            enabled,
            formatters,
        } = FormatConfig::default();
        Self {
            enabled,
            formatters,
        }
    }
}

impl TryFrom<RawFormatConfig> for FormatConfig {
    type Error = String;

    fn try_from(mut raw: RawFormatConfig) -> Result<Self, String> {
        for (name, formatter) in &mut raw.formatters {
            if formatter.command.as_ref().is_some_and(Vec::is_empty) {
                return Err(format!("format.{name}: command is empty"));
            }
            let custom = registry::find(name).is_none();
            // An empty list would quietly turn a built-in off; leaving it
            // out keeps the built-in's own, which is what was meant.
            if formatter.extensions.as_ref().is_some_and(Vec::is_empty) {
                if custom {
                    return Err(format!("format.{name}: extensions is empty"));
                }
                formatter.extensions = None;
            }
            if custom
                && !formatter.disabled
                && (formatter.command.is_none() || formatter.extensions.is_none())
            {
                return Err(format!(
                    "format.{name}: not a built-in formatter, so it needs command and extensions"
                ));
            }
            if let Some(extensions) = &mut formatter.extensions {
                for extension in extensions {
                    *extension = with_dot(extension);
                }
            }
        }
        Ok(Self {
            enabled: raw.enabled,
            formatters: raw.formatters,
        })
    }
}

/// `rs` meant `.rs`: the files are matched on the dot, and the slip is an
/// easy one to make.
fn with_dot(extension: &str) -> String {
    match extension.starts_with('.') {
        true => extension.to_string(),
        false => format!(".{extension}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<FormatConfig, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn empty_section_is_the_default() {
        assert_eq!(parse("").expect("parses"), FormatConfig::default());
    }

    #[test]
    fn named_tables_become_formatters() {
        let config = parse(
            r#"
            enabled = true

            [rustfmt]
            disabled = true

            [sed]
            command = ["sed", "-i", "s/a/b/", "$FILE"]
            extensions = [".txt"]
            env = { LC_ALL = "C" }
            "#,
        )
        .expect("parses");
        assert!(config.formatters["rustfmt"].disabled);
        let sed = &config.formatters["sed"];
        assert_eq!(sed.extensions.as_deref(), Some(&[".txt".to_string()][..]));
        assert_eq!(sed.env["LC_ALL"], "C");
    }

    #[test]
    fn custom_formatter_needs_command_and_extensions() {
        let err = parse("[mine]\ncommand = [\"x\", \"$FILE\"]").expect_err("no extensions");
        assert!(err.to_string().contains("format.mine"), "{err}");
        assert!(parse("[mine]\ndisabled = true").is_ok());
    }

    #[test]
    fn empty_command_is_an_error() {
        assert!(parse("[rustfmt]\ncommand = []").is_err());
    }

    #[test]
    fn extensions_get_their_dot_and_an_empty_list_keeps_the_default() {
        let config = parse(
            r#"
            [gofmt]
            extensions = []

            [sed]
            command = ["sed", "-i", "s/a/b/", "$FILE"]
            extensions = ["txt", ".md", "html.erb"]
            "#,
        )
        .expect("parses");
        assert_eq!(config.formatters["gofmt"].extensions, None);
        assert_eq!(
            config.formatters["sed"].extensions.as_deref(),
            Some(&[".txt".to_string(), ".md".into(), ".html.erb".into()][..])
        );

        let err = parse("[mine]\ncommand = [\"x\"]\nextensions = []").expect_err("no extensions");
        assert!(err.to_string().contains("extensions is empty"), "{err}");
    }

    #[test]
    fn unknown_formatter_keys_are_errors() {
        let err = parse("[rustfmt]\ncomand = [\"x\"]").expect_err("typo is rejected");
        assert!(err.to_string().contains("comand"), "{err}");
    }

    #[test]
    fn config_round_trips_through_toml() {
        let config = parse("enabled = false\n[gofmt]\nextensions = [\".go\"]").expect("parses");
        let text = toml::to_string(&config).expect("serializes");
        assert_eq!(parse(&text).expect("parses back"), config);
    }
}
