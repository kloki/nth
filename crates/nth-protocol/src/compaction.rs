//! How a session keeps itself inside the model's context window. The policy
//! lives here, beside the messages and providers it shapes, so anything
//! carrying a conversation — a tool that starts a child session, a
//! front-end — can carry it too; running a compaction is the session
//! loop's, `nth_session::Turn::compact`.

use serde::{Deserialize, Serialize};

/// The buffer kept below the window for a reply's own tokens, when neither
/// the config nor the model's output limit says otherwise.
const DEFAULT_RESERVED: u64 = 20_000;

/// The smallest and largest verbatim tail a compaction keeps.
const MIN_PRESERVE: u64 = 2_000;
const MAX_PRESERVE: u64 = 15_000;

/// How a session keeps itself inside the model's window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compaction {
    /// Whether a reply that reaches the usable window is compacted before
    /// the turn's next step.
    #[serde(default = "default_auto")]
    pub auto: bool,
    /// The buffer kept below the window; `0` means `min(20_000, output)`.
    #[serde(default)]
    pub reserved: u64,
    /// Tokens a compaction keeps verbatim; `0` means a quarter of the
    /// usable window, clamped to 2_000–15_000.
    #[serde(default)]
    pub preserve: u64,
}

impl Default for Compaction {
    fn default() -> Self {
        Self::DEFAULT
    }
}

fn default_auto() -> bool {
    true
}

impl Compaction {
    /// The defaults, for the const routes tests build.
    pub const DEFAULT: Compaction = Compaction {
        auto: true,
        reserved: 0,
        preserve: 0,
    };

    /// The buffer kept below the window: the configured one, or
    /// `min(20_000, output)` by default.
    pub fn reserved(&self, output: Option<u64>) -> u64 {
        match self.reserved {
            0 => output.unwrap_or(DEFAULT_RESERVED).min(DEFAULT_RESERVED),
            reserved => reserved,
        }
    }

    /// The tokens a conversation may fill before compaction fires: the
    /// window less the reserved buffer. `None` when no window is known, in
    /// which case nothing fires on its own.
    pub fn usable(&self, context_window: Option<u64>, output: Option<u64>) -> Option<u64> {
        Some(context_window?.saturating_sub(self.reserved(output)))
    }

    /// The verbatim tail budget under a usable window: the configured one,
    /// or a quarter of the window clamped to 2_000–15_000; the largest
    /// default when there is no window to size it against.
    pub fn preserve(&self, usable: Option<u64>) -> u64 {
        if self.preserve > 0 {
            return self.preserve;
        }
        match usable {
            Some(usable) => (usable / 4).clamp(MIN_PRESERVE, MAX_PRESERVE),
            None => MAX_PRESERVE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_follow_the_model_limits() {
        let compaction = Compaction::default();
        assert!(compaction.auto);
        // min(20k, output).
        assert_eq!(compaction.reserved(None), 20_000);
        assert_eq!(compaction.reserved(Some(8_192)), 8_192);
        assert_eq!(
            compaction.usable(Some(200_000), Some(128_000)),
            Some(180_000)
        );
        assert_eq!(compaction.usable(None, None), None);
        // A quarter of the usable window, clamped.
        assert_eq!(compaction.preserve(Some(1_000_000)), 15_000);
        assert_eq!(compaction.preserve(Some(40_000)), 10_000);
        assert_eq!(compaction.preserve(Some(4_000)), 2_000);
        assert_eq!(compaction.preserve(None), 15_000);

        let configured = Compaction {
            auto: false,
            reserved: 1_000,
            preserve: 500,
        };
        assert_eq!(configured.reserved(None), 1_000);
        assert_eq!(configured.preserve(Some(1_000_000)), 500);
    }
}
