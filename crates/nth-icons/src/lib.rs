//! Every icon nth draws, in two sets: `PLAIN`, Unicode any terminal font
//! has, and `NERD`, Nerd Font glyphs. The config's `nerdfonts` picks one at
//! start-up (`init`) and every front-end reads it through `icons()`.
//!
//! Each icon is one character one column wide, so swapping sets never moves
//! a layout. Separators, ellipses, bars and spinners are not icons and stay
//! where they are drawn.

use std::sync::OnceLock;

pub struct Icons {
    /// Something worked or is chosen.
    pub ok: &'static str,
    /// Something failed.
    pub fail: &'static str,
    /// The highlighted row in a list.
    pub pick: &'static str,
    /// Where something leads: a project root, a moved working directory.
    pub to: &'static str,
    /// The current one of a few, such as the mode.
    pub current: &'static str,
    /// A language server, coloured by its state.
    pub dot: &'static str,
    /// The share of input read from the prompt cache.
    pub cache: &'static str,
    /// How full the context window is.
    pub context: &'static str,
    /// Prompts waiting for the turn to end.
    pub queued: &'static str,
    /// The model's reasoning.
    pub thinking: &'static str,
    /// A finished turn.
    pub done: &'static str,
    /// A turn stopped before it finished.
    pub interrupted: &'static str,
    /// A request tried again.
    pub retry: &'static str,
    /// Your edits to the plan.
    pub plan_edits: &'static str,
    /// A subagent in the chat.
    pub subagent: &'static str,
    /// Lower and higher effort in the model picker.
    pub effort_less: &'static str,
    pub effort_more: &'static str,
    /// A question not yet answered, and one answered.
    pub unchecked: &'static str,
    pub checked: &'static str,
    pub tab: TabIcons,
    pub tool: ToolIcons,
    pub git: GitIcons,
}

/// In front of each tab's name in the header.
pub struct TabIcons {
    pub chat: &'static str,
    pub diagnostics: &'static str,
    pub plan: &'static str,
    pub monitor: &'static str,
    pub subagent: &'static str,
    pub usage: &'static str,
}

/// The mark a tool call's row starts with; `Icons::tool_icon` picks one by
/// the tool's name.
pub struct ToolIcons {
    pub read: &'static str,
    pub write: &'static str,
    pub edit: &'static str,
    pub apply_patch: &'static str,
    pub bash: &'static str,
    pub glob: &'static str,
    pub grep: &'static str,
    pub webfetch: &'static str,
    pub websearch: &'static str,
    pub skill: &'static str,
    pub question: &'static str,
    pub panel: &'static str,
    pub monitor: &'static str,
    pub task: &'static str,
    /// Any tool without its own.
    pub other: &'static str,
}

/// The git status on the status bar.
pub struct GitIcons {
    pub conflicted: &'static str,
    pub diverged: &'static str,
    pub stashed: &'static str,
    pub staged: &'static str,
    pub renamed: &'static str,
    pub deleted: &'static str,
    pub untracked: &'static str,
}

pub static PLAIN: Icons = Icons {
    ok: "✓",
    fail: "✗",
    pick: "→",
    to: "→",
    current: "▸",
    dot: "●",
    cache: "↻",
    context: "◘",
    queued: "⏵",
    thinking: "∴",
    done: "∎",
    interrupted: "⏹",
    retry: "⟳",
    plan_edits: "±",
    subagent: "↳",
    effort_less: "◂",
    effort_more: "▸",
    unchecked: "☐",
    checked: "☒",
    tab: TabIcons {
        chat: "›",
        diagnostics: "●",
        plan: "≡",
        monitor: "$",
        subagent: "@",
        usage: "∑",
    },
    tool: ToolIcons {
        read: "≡",
        write: ">",
        edit: "±",
        apply_patch: "Δ",
        bash: "$",
        glob: "*",
        grep: "/",
        webfetch: "↓",
        websearch: "?",
        skill: "✦",
        question: "¿",
        panel: "▣",
        monitor: "&",
        task: "↳",
        other: "•",
    },
    // Starship's defaults, but for those the counts' `+N` and `*N` use.
    git: GitIcons {
        conflicted: "=",
        diverged: "⇕",
        stashed: "$",
        staged: "✚",
        renamed: "»",
        deleted: "✘",
        untracked: "?",
    },
};

pub static NERD: Icons = Icons {
    ok: "",          // fa-check
    fail: "",        // fa-xmark
    pick: "",        // fa-caret_right
    to: "",          // fa-arrow_right
    current: "",     // fa-caret_right
    dot: "",         // fa-circle
    cache: "",       // fa-refresh
    context: "",     // fa-pie_chart
    queued: "",      // fa-play
    thinking: "",    // oct-light_bulb
    done: "",        // fa-flag_checkered
    interrupted: "", // fa-stop
    retry: "",       // fa-refresh
    plan_edits: "",  // fa-pencil
    subagent: "󰚩 ",   // md-robot
    effort_less: "", // fa-caret_left
    effort_more: "", // fa-caret_right
    unchecked: " ",  // fa-square_o
    checked: " ",    // fa-check_square
    tab: TabIcons {
        chat: "> ",        // fa-comments
        diagnostics: " ", // fa-stethoscope
        plan: " ",        // oct-checklist
        monitor: "& ",     // cod-pulse
        subagent: "󰚩 ",    // md-robot
        usage: " ",       // fa-bar_chart
    },
    tool: ToolIcons {
        read: "",        // fa-file_text_o
        write: "",       // cod-new_file
        edit: "",        // cod-edit
        apply_patch: "", // cod-diff
        bash: "",        // oct-terminal
        glob: "",        // fa-folder_open
        grep: "",        // fa-search
        webfetch: "",    // fa-download
        websearch: "",   // fa-globe
        skill: "",       // fa-magic
        question: "",    // fa-question_circle
        panel: "",       // cod-layout
        monitor: "",     // cod-pulse
        task: "󰚩",        // md-robot
        other: "",       // fa-wrench
    },
    git: GitIcons {
        conflicted: "", // fa-warning
        diverged: "󰱮",   // md-source_branch_sync
        stashed: "",    // fa-archive
        staged: "󰊐",     // md-plus_box_multiple
        renamed: "",    // fa-exchange
        deleted: "",    // fa-trash
        untracked: "",  // fa-question
    },
};

static SET: OnceLock<&'static Icons> = OnceLock::new();

/// Picks the set for the rest of the process, from the config's
/// `nerdfonts`. Only the first call counts.
pub fn init(nerdfonts: bool) {
    let _ = SET.set(if nerdfonts { &NERD } else { &PLAIN });
}

/// The set `init` picked; `PLAIN` before it, so tests and errors printed
/// before the config loads read plain.
pub fn icons() -> &'static Icons {
    SET.get().copied().unwrap_or(&PLAIN)
}

impl Icons {
    /// The mark for a call to the tool named `name`.
    pub fn tool_icon(&self, name: &str) -> &'static str {
        let tool = &self.tool;
        match name {
            "read" => tool.read,
            "write" => tool.write,
            "edit" => tool.edit,
            "apply_patch" => tool.apply_patch,
            "bash" => tool.bash,
            "glob" => tool.glob,
            "grep" => tool.grep,
            "webfetch" => tool.webfetch,
            "websearch" => tool.websearch,
            "skill" => tool.skill,
            "question" => tool.question,
            "panel" => tool.panel,
            "monitor" | "monitor_stop" => tool.monitor,
            "task" => tool.task,
            _ => tool.other,
        }
    }

    #[cfg(test)]
    fn all(&self) -> Vec<&'static str> {
        let Icons {
            ok,
            fail,
            pick,
            to,
            current,
            dot,
            cache,
            context,
            queued,
            thinking,
            done,
            interrupted,
            retry,
            plan_edits,
            subagent,
            effort_less,
            effort_more,
            unchecked,
            checked,
            tab,
            tool,
            git,
        } = self;
        let mut all = vec![
            *ok,
            *fail,
            *pick,
            *to,
            *current,
            *dot,
            *cache,
            *context,
            *queued,
            *thinking,
            *done,
            *interrupted,
            *retry,
            *plan_edits,
            *subagent,
            *effort_less,
            *effort_more,
            *unchecked,
            *checked,
        ];
        all.extend([
            tab.chat,
            tab.diagnostics,
            tab.plan,
            tab.monitor,
            tab.subagent,
            tab.usage,
        ]);
        all.extend([
            tool.read,
            tool.write,
            tool.edit,
            tool.apply_patch,
            tool.bash,
            tool.glob,
            tool.grep,
            tool.webfetch,
            tool.websearch,
            tool.skill,
            tool.question,
            tool.panel,
            tool.monitor,
            tool.task,
            tool.other,
        ]);
        all.extend([
            git.conflicted,
            git.diverged,
            git.stashed,
            git.staged,
            git.renamed,
            git.deleted,
            git.untracked,
        ]);
        all
    }
}

#[cfg(test)]
mod tests {
    use unicode_width::UnicodeWidthStr;

    use super::*;

    #[test]
    fn every_icon_is_one_column() {
        for set in [&PLAIN, &NERD] {
            for icon in set.all() {
                assert_eq!(icon.chars().count(), 1, "{icon:?}");
                assert_eq!(icon.width(), 1, "{icon:?}");
            }
        }
    }

    #[test]
    fn tools_have_their_own_icon() {
        assert_eq!(PLAIN.tool_icon("read"), "≡");
        assert_eq!(PLAIN.tool_icon("monitor_stop"), "&");
        assert_eq!(PLAIN.tool_icon("mystery"), "•");
        assert_eq!(NERD.tool_icon("bash"), "");
    }

    #[test]
    fn plain_until_picked() {
        assert!(std::ptr::eq(icons(), &PLAIN));
    }
}
