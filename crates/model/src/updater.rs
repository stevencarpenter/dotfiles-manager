//! The fixed set of updaters a profile can declare (docs/upgrade.md). A profile selects them by
//! name; no document can add a command.

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Updater {
    Agents,
    Brew,
    Cargo,
    Checkouts,
    Claude,
    Codex,
    Go,
    Mise,
    Npm,
    Opencode,
    Rustup,
    SideChannels,
    Skills,
    Uv,
}

impl Updater {
    pub const ALL: [Updater; 14] = [
        Updater::Agents,
        Updater::Brew,
        Updater::Cargo,
        Updater::Checkouts,
        Updater::Claude,
        Updater::Codex,
        Updater::Go,
        Updater::Mise,
        Updater::Npm,
        Updater::Opencode,
        Updater::Rustup,
        Updater::SideChannels,
        Updater::Skills,
        Updater::Uv,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Updater::Agents => "agents",
            Updater::Brew => "brew",
            Updater::Cargo => "cargo",
            Updater::Checkouts => "checkouts",
            Updater::Claude => "claude",
            Updater::Codex => "codex",
            Updater::Go => "go",
            Updater::Mise => "mise",
            Updater::Npm => "npm",
            Updater::Opencode => "opencode",
            Updater::Rustup => "rustup",
            Updater::SideChannels => "side-channels",
            Updater::Skills => "skills",
            Updater::Uv => "uv",
        }
    }

    /// The names, comma-separated, for the resolver's hint.
    pub fn names() -> String {
        Updater::ALL
            .iter()
            .map(|u| u.name())
            .collect::<Vec<_>>()
            .join(", ")
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
