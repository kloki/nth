//! Skills shipped inside nth, about using nth itself. Each is a whole
//! `SKILL.md` compiled into the binary, so it is there in every project;
//! a skill file of the same name replaces it.

use super::{Location, Skill, Source};
use crate::frontmatter;

const SKILLS: [&str; 4] = [
    include_str!("browser/SKILL.md"),
    include_str!("init/SKILL.md"),
    include_str!("setup-worktrees/SKILL.md"),
    include_str!("work-plan/SKILL.md"),
];

/// Every internal skill. They are part of nth, so one that does not parse
/// is a bug the tests catch, not something to warn about.
pub(super) fn skills() -> impl Iterator<Item = Skill> {
    SKILLS.iter().filter_map(|text| {
        let (front, _) = frontmatter::split(text).ok()?;
        Some(Skill {
            name: front.name?,
            description: front.description,
            location: Location::Internal(text),
            source: Source::Internal,
            root: None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_internal_skill_parses() {
        assert_eq!(skills().count(), SKILLS.len());
        for skill in skills() {
            assert!(skill.description.is_some(), "{}", skill.name);
            assert!(!skill.name.contains(char::is_whitespace), "{}", skill.name);
        }
    }

    #[test]
    fn agents_are_named_as_opencode_names_them() {
        for text in SKILLS {
            for claude_code in ["general-purpose", "Explore", "Agent tool"] {
                assert!(!text.contains(claude_code), "{claude_code}");
            }
        }
    }
}
