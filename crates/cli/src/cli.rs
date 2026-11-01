//! The command tree (clap builder API): the one definition that parses every command and flag.
//! The `run` functions get their `Args` from the matches; the checks across flags stay in each
//! command's `parse_args`.

use crate::usage;
use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Arg, ArgAction, ArgMatches, Command};
use dfm_model::Refusal;

/// A flag that takes one value. A value may start with `-`, as the hand-written parser allowed.
fn value(long: &'static str, name: &'static str) -> Arg {
    Arg::new(long)
        .long(long)
        .value_name(name)
        .action(ArgAction::Set)
        .allow_hyphen_values(true)
}

/// A flag that may be given more than once, each time with one value.
fn many(long: &'static str, name: &'static str) -> Arg {
    value(long, name).action(ArgAction::Append)
}

fn switch(long: &'static str) -> Arg {
    Arg::new(long).long(long).action(ArgAction::SetTrue)
}

fn root() -> Arg {
    value("root", "dir")
}

fn preview() -> Arg {
    switch("preview")
}

fn yes_plan() -> Arg {
    value("yes-plan", "digest").short('y')
}

fn allow_disruptive() -> Arg {
    many("allow-disruptive", "operation-id")
}

fn allow_adopt() -> Arg {
    many("allow-adopt", "operation-id")
}

/// `apply` and the scopes that share its flags, with the flags of the one scope in `extra`. A
/// flag a scope refuses with its own message (`dispatch`) is hidden: parsed (`parse`), but left
/// out of help and of the completion tree.
fn scope<const N: usize>(
    parse: bool,
    name: &'static str,
    about: &'static str,
    extra: [Arg; N],
) -> Command {
    Command::new(name)
        .about(about)
        .arg(root())
        .arg(preview())
        .arg(yes_plan())
        .args(extra.into_iter().filter(|a| parse || !a.is_hide_set()))
}

fn report(name: &'static str, about: &'static str) -> Command {
    Command::new(name)
        .about(about)
        .arg(root())
        .arg(switch("json"))
}

/// Every command `dispatch` accepts. The top level keeps its own help and version text, so
/// clap's help and version flags and its `help` subcommand are off there; each subcommand gets
/// `-h`/`--help` back with the help clap generates.
pub(crate) fn command() -> Command {
    tree(true)
}

/// The tree `dfm completions` prints: `command` without its hidden flags and commands, which
/// clap_complete would otherwise offer.
pub(crate) fn completion_command() -> Command {
    tree(false)
}

fn tree(parse: bool) -> Command {
    let cmd = Command::new("dfm")
        .disable_help_flag(true)
        .disable_version_flag(true)
        .disable_help_subcommand(true)
        .arg(switch("help").short('h'))
        .arg(switch("version").short('V'))
        .subcommand(Command::new("help").about("Print the usage text"))
        .subcommand(Command::new("version").about("Print the version and build commit"))
        .subcommand(
            Command::new("bind")
                .about("Bind this machine to one profile")
                .arg(value("profile", "file"))
                .arg(value("machine-id", "id"))
                .arg(root()),
        )
        .subcommand(Command::new("binding").about("Print the binding"))
        .subcommand(
            Command::new("state")
                .about("Check, recover or resolve the state store")
                .subcommand(Command::new("check").about("Check the state store"))
                .subcommand(Command::new("recover").about("Settle interrupted operations"))
                .subcommand(
                    Command::new("resolve")
                        .about("Settle one journal entry by hand")
                        .arg(Arg::new("journal-id").required(true))
                        .arg(value("as", "done|failed").required(true)),
                ),
        )
        .subcommand(
            Command::new("explain")
                .about("Explain one resource or output path")
                .arg(Arg::new("query").value_name("resource-or-target"))
                .arg(root()),
        )
        .subcommand(
            Command::new("plan")
                .about("Print the plan and its digest")
                .arg(root())
                .arg(switch("json"))
                .arg(value("against", "saved plan")),
        )
        .subcommand(report(
            "doctor",
            "Inspect the machine with detail and fix text",
        ))
        .subcommand(report("status", "Inspect the machine, one block per row"))
        .subcommand(scope(
            parse,
            "apply",
            "Publish the configuration of an approved plan",
            [allow_disruptive(), allow_adopt()],
        ))
        .subcommand(scope(
            parse,
            "secrets",
            "Render the secret resources",
            [allow_disruptive().hide(true), allow_adopt()],
        ))
        .subcommand(scope(
            parse,
            "packages",
            "Install the missing packages",
            [
                allow_disruptive().hide(true),
                allow_adopt().hide(true),
                switch("update"),
            ],
        ))
        .subcommand(scope(
            parse,
            "services",
            "Start the services that are not loaded",
            [
                allow_disruptive(),
                allow_adopt().hide(true),
                many("restart", "resource-id"),
            ],
        ))
        .subcommand(
            Command::new("trial")
                .about("Try a tool without a module")
                .arg(Arg::new("package"))
                .arg(value("id", "trial-id"))
                .arg(value("provider", "homebrew/v1|mise/v1"))
                .arg(many("config", "path"))
                .arg(preview())
                .arg(yes_plan()),
        )
        .subcommand(
            Command::new("adopt")
                .about("Promote a trial into a module")
                .arg(Arg::new("trial-id"))
                .arg(value("module", "module"))
                .arg(root())
                .arg(preview())
                .arg(yes_plan()),
        )
        .subcommand(
            Command::new("purge")
                .about("End a trial that is not adopted")
                .arg(Arg::new("trial-id"))
                .arg(preview())
                .arg(yes_plan()),
        )
        .subcommand(scope(
            parse,
            "sync",
            "Fetch the sources and run every scope",
            [
                allow_disruptive(),
                allow_adopt(),
                many("restart", "resource-id"),
                many("trust-origin", "origin"),
                switch("no-upgrade"),
            ],
        ))
        .subcommand(
            Command::new("self-update")
                .about("Rebuild and install the manager from a local checkout")
                .arg(value("source", "checkout"))
                .arg(value("commit", "sha")),
        )
        .subcommand(
            Command::new("completions")
                .about("Print the shell completion script for dfm")
                .arg(
                    Arg::new("shell")
                        .required(true)
                        .value_parser(clap::value_parser!(clap_complete::Shell)),
                ),
        )
        .subcommand(
            Command::new("self-check")
                .about("Check build controls, a fixture directory or a state home")
                .arg(value("build-controls", "staged tree"))
                .arg(value("fixture-dir", "dir"))
                .arg(value("state-home", "home")),
        );
    let cmd = if parse { test_only(cmd) } else { cmd };
    let help = Arg::new("help")
        .short('h')
        .long("help")
        .action(ArgAction::Help)
        .help("Print help");
    cmd.mut_subcommands(|s| {
        let s = s.arg(help.clone());
        s.mut_subcommands(|s| s.arg(help.clone()))
    })
}

/// `--home` and the harness commands exist only in the isolated build, hidden.
#[cfg(feature = "test-isolation")]
fn test_only(cmd: Command) -> Command {
    cmd.arg(value("home", "dir").hide(true))
        .subcommand(
            Command::new("__record-op").hide(true).args(
                [
                    "resource",
                    "adapter",
                    "output",
                    "content",
                    "op-id",
                    "sensitive",
                    "note",
                    "secret",
                    "at",
                ]
                .map(|n| value(n, "value")),
            ),
        )
        .subcommand(
            Command::new("__record-trial")
                .hide(true)
                .args(["id", "tool", "baseline-path"].map(|n| value(n, "value"))),
        )
        .subcommand(Command::new("__reconcile").hide(true))
}

#[cfg(not(feature = "test-isolation"))]
fn test_only(cmd: Command) -> Command {
    cmd
}

/// The value of a flag, `None` when it was not given or the command does not define it.
pub(crate) fn one(m: &ArgMatches, id: &str) -> Option<String> {
    m.try_get_one::<String>(id).ok().flatten().cloned()
}

/// Every value of a repeatable flag, in the order given.
pub(crate) fn all(m: &ArgMatches, id: &str) -> Vec<String> {
    m.try_get_many::<String>(id)
        .ok()
        .flatten()
        .map(|v| v.cloned().collect())
        .unwrap_or_default()
}

/// Whether a switch was given; `false` when the command does not define it.
pub(crate) fn flag(m: &ArgMatches, id: &str) -> bool {
    m.try_get_one::<bool>(id)
        .ok()
        .flatten()
        .copied()
        .unwrap_or(false)
}

/// The commands that take no arguments: any parse error after one of them says so.
const NO_ARGS: [&str; 7] = [
    "help",
    "--help",
    "-h",
    "version",
    "--version",
    "-V",
    "binding",
];

/// A clap parse error as one usage line (`E_USAGE`), with the wording of the hand-written
/// parser where it had one: no clap usage block, no color. `cmd` is the first command word.
pub(crate) fn usage_error(e: &clap::Error, cmd: Option<&str>) -> Refusal {
    if let Some(c) = cmd.filter(|c| NO_ARGS.contains(c)) {
        return usage(format!("{c} takes no arguments"));
    }
    let text = |k: ContextKind| match e.get(k) {
        Some(ContextValue::String(s)) => Some(s.clone()),
        Some(ContextValue::Strings(v)) => Some(v.join(", ")),
        _ => None,
    };
    let given = text(ContextKind::InvalidArg);
    // The same flag twice is a conflict with itself.
    let twice = given.is_some() && given == text(ContextKind::PriorArg);
    // clap names a flag with its value: `--root <dir>`.
    let arg = given.map(|a| a.split(' ').next().unwrap_or_default().to_string());
    let value = text(ContextKind::InvalidValue);
    match (e.kind(), arg) {
        (ErrorKind::UnknownArgument, Some(a)) => usage(format!("unknown argument {a}")),
        (ErrorKind::InvalidSubcommand, _) => usage(format!(
            "unknown command or argument {}; try --help",
            text(ContextKind::InvalidSubcommand).unwrap_or_default()
        )),
        (ErrorKind::ArgumentConflict, Some(a)) if twice => usage(format!("{a} given twice")),
        (ErrorKind::InvalidValue, Some(a)) if value.as_deref() == Some("") => {
            usage(format!("{a} needs a value"))
        }
        (ErrorKind::MissingRequiredArgument, Some(a)) => usage(format!("{a} is required")),
        _ => {
            // Any other kind: the first line of clap's text, without its `error: ` prefix.
            let shown = e.render().to_string();
            let line = shown.lines().next().unwrap_or("invalid arguments");
            usage(line.strip_prefix("error: ").unwrap_or(line).to_string())
        }
    }
}

/// The matches of one subcommand as `dispatch` parses them, with its usage errors (unit tests).
#[cfg(test)]
pub(crate) fn parse(cmd: &str, args: &[&str]) -> Result<ArgMatches, Refusal> {
    let argv = ["dfm", cmd].into_iter().chain(args.iter().copied());
    let mut m = command()
        .try_get_matches_from(argv)
        .map_err(|e| usage_error(&e, Some(cmd)))?;
    Ok(m.remove_subcommand().expect("a subcommand was given").1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command words `run_command` matches, read from its source.
    fn dispatched() -> std::collections::BTreeSet<String> {
        let src = include_str!("lib.rs");
        let body = src
            .split("fn run_command(")
            .nth(1)
            .and_then(|r| r.split("\n}\n").next())
            .expect("lib.rs defines run_command");
        body.lines()
            .map(str::trim_start)
            .filter(|l| l.starts_with('"') && l.contains("=>"))
            .flat_map(|l| l.split("=>").next().unwrap_or_default().split('|'))
            .map(|w| w.trim().trim_matches('"').to_string())
            .collect()
    }

    /// The tree is the single source for parsing and completions: every command `run_command`
    /// accepts is in it, and it has no command `run_command` would not run.
    #[test]
    fn the_command_tree_names_every_dispatched_command() {
        let tree: std::collections::BTreeSet<String> = command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        let mut want = dispatched();
        // The harness arms are compiled only in the isolated build, as their tree entries are.
        if !cfg!(feature = "test-isolation") {
            want.retain(|w| !w.starts_with("__"));
        }
        assert!(want.contains("plan") && want.contains("doctor"), "{want:?}");
        assert_eq!(tree, want);
    }

    #[cfg(feature = "test-isolation")]
    #[test]
    fn the_test_commands_and_home_are_hidden() {
        let cmd = command();
        for name in ["__record-op", "__record-trial", "__reconcile"] {
            assert!(cmd.find_subcommand(name).unwrap().is_hide_set(), "{name}");
        }
        let home = cmd.get_arguments().find(|a| a.get_id() == "home").unwrap();
        assert!(home.is_hide_set());
    }

    /// Nothing hidden reaches the completion script, in any build.
    #[test]
    fn the_completion_tree_has_nothing_hidden() {
        fn walk(c: &Command) {
            assert!(!c.is_hide_set(), "{}", c.get_name());
            for a in c.get_arguments() {
                assert!(!a.is_hide_set(), "{} {}", c.get_name(), a.get_id());
            }
            c.get_subcommands().for_each(walk);
        }
        let tree = completion_command();
        walk(&tree);
        let names = |c: &Command| {
            c.get_subcommands()
                .map(|s| s.get_name().to_string())
                .collect::<Vec<_>>()
        };
        let parsed = command();
        let visible: Vec<_> = names(&parsed)
            .into_iter()
            .filter(|n| !parsed.find_subcommand(n).unwrap().is_hide_set())
            .collect();
        assert_eq!(names(&tree), visible);
        assert!(visible.contains(&"completions".to_string()));
    }

    #[test]
    fn usage_errors_keep_the_hand_written_wording() {
        let err = |cmd: &str, args: &[&str]| parse(cmd, args).unwrap_err().detail;
        assert_eq!(
            err("plan", &["--root", "a", "--root", "b"]),
            "--root given twice"
        );
        assert_eq!(err("plan", &["--json", "--json"]), "--json given twice");
        assert_eq!(err("plan", &["--root"]), "--root needs a value");
        assert_eq!(err("plan", &["--nope"]), "unknown argument --nope");
        assert_eq!(err("binding", &["--json"]), "binding takes no arguments");
        assert_eq!(err("state", &["resolve", "x"]), "--as is required");
        let e = command().try_get_matches_from(["dfm", "frob"]).unwrap_err();
        assert_eq!(
            usage_error(&e, Some("frob")).detail,
            "unknown command or argument frob; try --help"
        );
    }

    #[test]
    fn a_flag_value_may_start_with_a_hyphen() {
        let m = parse("plan", &["--root", "-r", "--against", "--json"]).unwrap();
        assert_eq!(one(&m, "root").as_deref(), Some("-r"));
        assert_eq!(one(&m, "against").as_deref(), Some("--json"));
        assert!(!flag(&m, "json"));
    }
}
