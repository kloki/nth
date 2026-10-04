//! What the agent is set up to do: plan, where it may only write its plan
//! file, or act, where it may change anything.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Plan,
    /// The default only for what has no mode of its own: sessions saved
    /// before modes existed ran unrestricted, so they load as act. A new
    /// chat starts in the config's `[mode] default`, which is plan.
    #[default]
    Act,
}

impl Mode {
    pub const ALL: [Mode; 2] = [Mode::Plan, Mode::Act];

    pub fn label(self) -> &'static str {
        match self {
            Mode::Plan => "plan",
            Mode::Act => "act",
        }
    }

    /// The other mode, which Tab switches to.
    pub fn toggled(self) -> Mode {
        match self {
            Mode::Plan => Mode::Act,
            Mode::Act => Mode::Plan,
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.label() == s)
            .ok_or_else(|| format!("unknown mode {s:?}, expected plan or act"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_its_labels() {
        assert_eq!("plan".parse(), Ok(Mode::Plan));
        assert_eq!("act".parse(), Ok(Mode::Act));
        assert!("build".parse::<Mode>().is_err());
        assert_eq!(Mode::Plan.toggled(), Mode::Act);
        assert_eq!(Mode::Act.toggled(), Mode::Plan);
    }

    #[test]
    fn serializes_in_lower_case() {
        assert_eq!(
            serde_json::to_string(&Mode::Plan).expect("serializes"),
            "\"plan\""
        );
    }
}
