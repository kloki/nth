use serde::{Deserialize, Serialize};

use crate::{Backend, Notifier, NotifySend};

/// The `[notify]` section.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotifyConfig {
    /// `false` turns notifications off.
    pub enabled: bool,
    pub backend: BackendKind,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: BackendKind::default(),
        }
    }
}

/// Where notifications go.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    #[default]
    NotifySend,
}

impl NotifyConfig {
    /// `None` when notifications are off.
    pub fn backend(&self) -> Option<Box<dyn Backend>> {
        if !self.enabled {
            return None;
        }
        Some(match self.backend {
            BackendKind::NotifySend => Box::new(NotifySend),
        })
    }

    /// Needs a Tokio runtime unless notifications are off.
    pub fn notifier(&self) -> Notifier {
        self.backend().map_or_else(Notifier::off, Notifier::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_backend_by_its_command_name() {
        let config: NotifyConfig = toml::from_str("backend = \"notify-send\"").unwrap();
        assert_eq!(config, NotifyConfig::default());
        assert_eq!(config.backend().unwrap().name(), "notify-send");
    }

    #[test]
    fn off_has_no_backend() {
        let config: NotifyConfig = toml::from_str("enabled = false").unwrap();
        assert!(config.backend().is_none());
    }

    #[test]
    fn rejects_unknown_keys_and_backends() {
        assert!(toml::from_str::<NotifyConfig>("sound = true").is_err());
        assert!(toml::from_str::<NotifyConfig>("backend = \"growl\"").is_err());
    }
}
