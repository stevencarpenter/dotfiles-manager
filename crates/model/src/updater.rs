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
    Lefthook,
    Mise,
    Npm,
    Opencode,
    Rustup,
    SideChannels,
    Skills,
    Uv,
}

impl Updater {
    pub const ALL: [Updater; 15] = [
        Updater::Agents,
        Updater::Brew,
        Updater::Cargo,
        Updater::Checkouts,
        Updater::Claude,
        Updater::Codex,
        Updater::Go,
        Updater::Lefthook,
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
            Updater::Lefthook => "lefthook",
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

    /// The updaters whose tools this one runs or whose files it reads (docs/upgrade.md,
    /// "Order"). brew installs the binaries of mise, uv, npm (node), lefthook and git, and codex
    /// refreshes its plugin marketplaces with git. go and skills run through `mise exec`. cargo
    /// is the rustup proxy and links libraries from brew when it builds. agents installs from
    /// the copy that `checkouts` pulls and runs the `uv` from brew. side-channels runs an
    /// arbitrary recipe, so it uses every other updater. The match is exhaustive, so a new
    /// updater does not compile until its row is written.
    pub fn uses(self) -> &'static [Updater] {
        match self {
            Updater::Agents => &[Updater::Brew, Updater::Checkouts],
            Updater::Cargo => &[Updater::Brew, Updater::Rustup],
            Updater::Checkouts
            | Updater::Codex
            | Updater::Lefthook
            | Updater::Mise
            | Updater::Npm
            | Updater::Uv => &[Updater::Brew],
            Updater::Go | Updater::Skills => &[Updater::Brew, Updater::Mise],
            Updater::SideChannels => &Updater::ALL,
            Updater::Brew | Updater::Claude | Updater::Opencode | Updater::Rustup => &[],
        }
    }

    /// Whether the two must not run at the same time: one uses the other ([`Updater::uses`]).
    /// Sync starts the later declared of a conflicting pair after the earlier one ends.
    pub fn conflicts(self, other: Updater) -> bool {
        self != other && (self.uses().contains(&other) || other.uses().contains(&self))
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
        assert_eq!(Updater::parse("lefthook"), Some(Updater::Lefthook));
        assert_eq!(Updater::ALL.len(), Updater::names().split(", ").count());
        // Sync never replaces the running manager.
        assert_eq!(Updater::parse("dfm"), None);
    }

    #[test]
    fn conflicts_are_symmetric_and_follow_the_tools() {
        for a in Updater::ALL {
            assert!(!a.conflicts(a));
            assert!(Updater::SideChannels == a || Updater::SideChannels.conflicts(a));
            for b in Updater::ALL {
                assert_eq!(a.conflicts(b), b.conflicts(a), "{a:?} {b:?}");
            }
        }
        assert!(Updater::Brew.conflicts(Updater::Mise));
        assert!(Updater::Skills.conflicts(Updater::Mise));
        assert!(Updater::Cargo.conflicts(Updater::Rustup));
        assert!(Updater::Agents.conflicts(Updater::Checkouts));
        assert!(!Updater::Brew.conflicts(Updater::Rustup));
        assert!(!Updater::Claude.conflicts(Updater::Codex));
        assert!(Updater::Codex.conflicts(Updater::Brew));
        assert!(!Updater::Mise.conflicts(Updater::Npm));
    }
}
