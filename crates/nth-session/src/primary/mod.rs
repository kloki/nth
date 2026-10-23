//! Primary agents: a session runs as one in place of plan or act. Its
//! prompt replaces the model's persona in the system prompt, which only
//! takes effect for what follows, so the model is told when it enters an
//! agent or leaves one mid-conversation.

const ENTER: &str = include_str!("enter.md");
const LEAVE: &str = include_str!("leave.md");

/// What the model needs to hear before a turn as `agent` after one as
/// `last` (`None` for plan or act): that it left the last agent, then that
/// it runs as the new one. Nothing before the first turn, whose system
/// prompt already is the agent's, or while the agent stays.
pub(crate) fn reminder(agent: Option<&str>, last: Option<&str>, first: bool) -> Option<String> {
    if first || agent == last {
        return None;
    }
    let leave = last.map(|last| LEAVE.trim_end().replace("{agent}", last));
    let enter = agent.map(|agent| ENTER.trim_end().replace("{agent}", agent));
    let both: Vec<String> = leave.into_iter().chain(enter).collect();
    Some(both.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_on_the_first_turn_or_while_the_agent_stays() {
        assert_eq!(reminder(Some("research"), None, true), None);
        assert_eq!(reminder(Some("research"), Some("research"), false), None);
        assert_eq!(reminder(None, None, false), None);
    }

    #[test]
    fn entering_leaving_and_switching() {
        let enter = reminder(Some("research"), None, false).expect("entering");
        assert!(
            enter.contains("now running as the research agent"),
            "{enter}"
        );

        let leave = reminder(None, Some("research"), false).expect("leaving");
        assert!(
            leave.contains("no longer running as the research agent"),
            "{leave}"
        );
        assert!(!leave.contains("now running"), "{leave}");

        let switch = reminder(Some("review"), Some("research"), false).expect("switching");
        let left = switch
            .find("no longer running as the research")
            .expect("left");
        let entered = switch.find("now running as the review").expect("entered");
        assert!(left < entered, "{switch}");
    }
}
