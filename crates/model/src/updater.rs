//! The fixed set of updaters a profile can declare (docs/upgrade.md). A profile selects them by
//! name; no document can add a command.

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Updater {
    Agents,
    Brew,
    Claude,
    Codex,
    Opencode,
    Skills,
}

impl Updater {
    pub const ALL: [Updater; 6] = [
        Updater::Agents,
        Updater::Brew,
        Updater::Claude,
        Updater::Codex,
        Updater::Opencode,
        Updater::Skills,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Updater::Agents => "agents",
            Updater::Brew => "brew",
            Updater::Claude => "claude",
            Updater::Codex => "codex",
            Updater::Opencode => "opencode",
            Updater::Skills => "skills",
        }
    }

    pub fn parse(s: &str) -> Option<Updater> {
        Updater::ALL.into_iter().find(|u| u.name() == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_parses_back() {
        for u in Updater::ALL {
            assert_eq!(Updater::parse(u.name()), Some(u));
        }
        assert_eq!(Updater::parse("Brew"), None);
        assert_eq!(Updater::parse("agents"), Some(Updater::Agents));
        // Sync never replaces the running manager.
        assert_eq!(Updater::parse("dfm"), None);
    }
}
