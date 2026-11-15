//! `nth models`: the models the providers serve, with their limits,
//! efforts and whether they take images, the one in use marked, and on
//! stderr the providers that could not be asked.

use std::io::Write;

use anyhow::{Result, anyhow};
use nth_config::Config;
use nth_icons::icons;
use nth_protocol::{ModelInfo, Provider};
use owo_colors::OwoColorize;

use crate::{
    output::{self, Report},
    runtime::Runtime,
};

pub async fn run(json: bool, config: Config, runtime: &Runtime) -> Result<()> {
    let providers = runtime.providers(&config)?;
    let listing = providers.models().await.map_err(|e| anyhow!(e))?;
    for failed in &listing.failed {
        eprintln!(
            "{} {}: {}",
            icons().fail.red().bold(),
            failed.origin.name.bold(),
            failed.error
        );
    }
    let report = ModelsReport {
        models: listing.models,
        current: providers.qualify(&config.model),
    };
    output::print(&report, json)
}

/// The models the providers listed, and the qualified id of the one in use.
struct ModelsReport {
    models: Vec<ModelInfo>,
    current: String,
}

impl Report for ModelsReport {
    fn text(&self, out: &mut dyn Write) -> Result<()> {
        // One group per provider, its name above its models.
        let width = self
            .models
            .iter()
            .map(|m| m.wire_id().len())
            .max()
            .unwrap_or(0);
        let mut shown: Option<&str> = None;
        for model in &self.models {
            let origin = model.origin.as_ref().map(|o| o.name.as_str());
            if origin.is_some() && origin != shown {
                writeln!(out, "{}", origin.unwrap_or_default().bold())?;
                shown = origin;
            }
            let efforts: Vec<&str> = model.efforts.iter().map(|e| e.name()).collect();
            let images = if model.takes_images == Some(true) {
                "images"
            } else {
                ""
            };
            let limits = [model.limits(), efforts.join("/"), images.to_string()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · ")
                .dimmed()
                .to_string();
            let id = model.wire_id();
            if model.id == self.current {
                writeln!(
                    out,
                    "{} {:width$}  {limits}",
                    icons().to.cyan().bold(),
                    id.bold()
                )?;
            } else {
                writeln!(out, "  {id:width$}  {limits}")?;
            }
        }
        Ok(())
    }

    fn json(&self, out: &mut dyn Write) -> Result<()> {
        for model in &self.models {
            writeln!(out, "{}", serde_json::to_string(model)?)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::{Effort, Origin};
    use serde_json::Value;

    use super::*;
    use crate::output::testing;

    fn origin(id: &str, name: &str) -> Option<Origin> {
        Some(Origin {
            id: id.into(),
            name: name.into(),
        })
    }

    /// A bare model of `provider`, known by nothing but its id.
    fn model(provider: &str, name: &str, id: &str) -> ModelInfo {
        ModelInfo {
            id: format!("{provider}/{id}"),
            name: None,
            context: None,
            output: None,
            efforts: Vec::new(),
            origin: origin(provider, name),
            cost: None,
            takes_images: None,
        }
    }

    /// Two providers; the first's model in use, with limits, efforts and
    /// images, the second's with none of them.
    fn report() -> ModelsReport {
        let full = ModelInfo {
            context: Some(1_000_000),
            output: Some(128_000),
            efforts: vec![Effort::Low, Effort::High],
            takes_images: Some(true),
            ..model("zen", "OpenCode Zen", "claude-opus")
        };
        ModelsReport {
            models: vec![
                full,
                model("zen", "OpenCode Zen", "kimi"),
                model("go", "OpenCode Go", "glm-long-name"),
            ],
            current: "zen/claude-opus".into(),
        }
    }

    #[test]
    fn models_group_under_their_provider_with_the_current_one_marked() {
        insta::assert_snapshot!(testing::text(&report()));
    }

    #[test]
    fn each_model_is_a_json_line_of_its_listing() {
        let lines: Vec<Value> = testing::json(&report())
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON line"))
            .collect();

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["id"], "zen/claude-opus");
        assert_eq!(lines[0]["efforts"], serde_json::json!(["low", "high"]));
        assert_eq!(lines[2]["origin"]["name"], "OpenCode Go");
        insta::assert_json_snapshot!(lines[0]);
    }
}
