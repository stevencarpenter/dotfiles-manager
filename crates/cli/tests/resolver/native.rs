//! `explain` text for native documents: include order, siblings and per-value overlap.

use crate::harness::{examples, synthetic};
use dfm_cli::builtin_catalog;
use dfm_model::resolve::{explain, resolve};

fn text(set: &dfm_model::resolve::SourceSet, profile: &str, query: &str) -> String {
    let r = resolve(set, profile, &builtin_catalog()).unwrap();
    explain(&r, query).unwrap()
}

/// The lines of `out` from the first one containing `from` up to the next line that is not
/// indented deeper than `from`'s own indent.
fn block<'a>(out: &'a str, from: &str) -> Vec<&'a str> {
    let lines: Vec<&str> = out.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.contains(from))
        .unwrap_or_else(|| panic!("{from} not in:\n{out}"));
    let indent = lines[start].len() - lines[start].trim_start().len();
    lines[start + 1..]
        .iter()
        .copied()
        .take_while(|l| l.len() - l.trim_start().len() > indent)
        .collect()
}

#[test]
fn a_fragment_shows_its_position_condition_siblings_and_the_rule() {
    let out = text(
        &synthetic(),
        "profiles/machine-b.toml",
        "private-mod.local-overrides",
    );
    for line in [
        "native precedence: this adapter writes into a native document. Rule: later include entries override earlier ones for single-valued keys.",
        "included by core.git (.config/synthetic/gitconfig) at position 2 of 3, only when gitdir:~/projects/",
        "1. ~/.config/synthetic/editor.inc -> core.editor-defaults (module core)",
        "2. ~/.config/synthetic/local.inc only when gitdir:~/projects/ -> private-mod.local-overrides (module private-mod)",
        "3. ~/.config/synthetic/absent.inc -> no resource in this composition declares this file",
    ] {
        assert!(out.contains(line), "missing {line:?} in:\n{out}");
    }
    // The shared value names the other fragment and its position.
    let editor = block(&out, "/core/editor = \"ed\"");
    assert!(
        editor
            .iter()
            .any(|l| l.contains("also set by core.editor-defaults (include position 1)")),
        "{editor:?}"
    );
}

#[test]
fn the_earlier_fragment_names_the_later_one_and_the_base_lists_the_order() {
    let set = synthetic();
    let out = text(&set, "profiles/machine-b.toml", "core.editor-defaults");
    assert!(out.contains("at position 1 of 3\n"), "{out}");
    let editor = block(&out, "/core/editor = \"vim\"");
    assert!(
        editor
            .iter()
            .any(|l| l.contains("also set by private-mod.local-overrides (include position 2)")),
        "{editor:?}"
    );
    let base = text(&set, "profiles/machine-b.toml", "core.git");
    assert!(
        base.contains("includes, in the order the application reads them:"),
        "{base}"
    );
    assert!(base.contains("1. ~/.config/synthetic/editor.inc"), "{base}");
}

#[test]
fn without_a_second_fragment_no_value_is_marked_as_overlapping() {
    let out = text(
        &synthetic(),
        "profiles/machine-a.toml",
        "core.editor-defaults",
    );
    assert!(out.contains("at position 1 of 3"), "{out}");
    assert!(!out.contains("also set by"), "{out}");
    assert!(
        out.contains("3. ~/.config/synthetic/absent.inc -> no resource"),
        "{out}"
    );
}

#[test]
fn the_work_example_shows_the_conditional_identity_include() {
    let out = text(&examples(), "profiles/work-01.toml", "work.git-identity");
    assert!(
        out.contains(
            "included by core.git (.config/git/config) at position 3 of 3, only when gitdir:~/work/"
        ),
        "{out}"
    );
    assert!(
        out.contains("2. ~/.config/external-overlays/git/extra.inc -> no resource"),
        "{out}"
    );
}

#[test]
fn a_document_with_an_includer_shows_it_and_one_without_invents_no_order() {
    let out = text(&examples(), "profiles/server-01.toml", "core.git-defaults");
    assert!(out.contains("native precedence:"), "{out}");
    assert!(
        out.contains("included by core.git"),
        "core.git still includes it:\n{out}"
    );
    let agents = text(
        &examples(),
        "profiles/personal-dev-01.toml",
        "agents.claude-settings",
    );
    assert!(
        agents.contains("Rule: the application decides which value wins"),
        "{agents}"
    );
    assert!(!agents.contains("included by"), "{agents}");
}
