//! C07 and C08: two synthetic targets share one core and one reusable module source.

use crate::common::repo_root;
use crate::harness::{edit, load, synthetic, text, val};
use dfm_cli::builtin_catalog;
use dfm_model::resolve::{Entry, Resolved, SourceSet, resolve};

const A: &str = "profiles/machine-a.toml";
const B: &str = "profiles/machine-b.toml";
const TOOL: &str = "reusable.tool-config";

fn go(set: &SourceSet, profile: &str) -> Resolved {
    resolve(set, profile, &builtin_catalog()).unwrap_or_else(|e| panic!("{profile}: {e}"))
}

/// The set a public checkout would hold: no private bundle, module or profile.
fn public_only(full: &SourceSet) -> SourceSet {
    let mut out = SourceSet::new();
    for p in full.paths().filter(|p| p.starts_with("shared/") || *p == A) {
        match full.get(p).unwrap() {
            Entry::File(b) => out.insert_file(p, b.clone()),
            Entry::Link(t) => out.insert_link(p, t.clone()),
        }
    }
    out
}

#[test]
fn c07_two_targets_reference_the_same_core_and_reusable_source() {
    let set = synthetic();
    let (a, b) = (go(&set, A), go(&set, B));
    assert_eq!(
        (a.profile.as_str(), b.profile.as_str()),
        ("machine-a", "machine-b")
    );
    for id in [
        "core.launcher",
        TOOL,
        "reusable.tool-settings",
        "reusable.agent-settings",
    ] {
        let (ra, rb) = (&a.resources[id], &b.resources[id]);
        assert_eq!(ra.declared_in, rb.declared_in, "{id} has one declaration");
        assert_eq!(ra.module, rb.module);
    }
    let shared = |r: &Resolved| r.resources[TOOL].provenance["/source"].clone();
    assert_eq!(shared(&a), shared(&b), "both resolve the same source file");
    assert_eq!(
        shared(&a).source_path.as_deref(),
        Some("shared/reusable/files/tool.conf")
    );
}

#[test]
fn c07_the_declarations_exist_once_and_profiles_hold_none() {
    let set = synthetic();
    let count = |needle: &str| {
        set.paths()
            .filter(|p| p.ends_with(".toml"))
            .filter(|p| text(&set, p).contains(needle))
            .count()
    };
    for decl in [
        "id = \"tool-config\"",
        "id = \"tool-settings\"",
        "id = \"editor-defaults\"",
    ] {
        assert_eq!(count(decl), 1, "{decl}");
    }
    for profile in [A, B] {
        let t = text(&set, profile);
        assert!(
            !t.contains("[[resources]]") && !t.contains("kind ="),
            "{profile} copies a declaration"
        );
    }
    // The checked-in files are the fixtures the test reads.
    assert!(
        load(&repo_root().join("tests/fixtures/resolver"))
            .get(A)
            .is_some()
    );
}

#[test]
fn c08_one_change_to_the_shared_source_changes_both_resolutions() {
    let before = synthetic();
    let (a0, b0) = (go(&before, A), go(&before, B));

    let mut after = synthetic();
    after.insert_file(
        "shared/reusable/files/tool.conf",
        "shared tool configuration, revised\n",
    );
    edit(
        &mut after,
        "shared/reusable/module.toml",
        "replace",
        "/resources/2/value/tool/timeout",
        Some("60"),
    );
    edit(
        &mut after,
        "shared/core/module.toml",
        "replace",
        "/resources/0/value/core/editor",
        Some("\"emacs\""),
    );
    let (a1, b1) = (go(&after, A), go(&after, B));

    for (old, new, who) in [(&a0, &a1, "machine-a"), (&b0, &b1, "machine-b")] {
        let id = |r: &Resolved| r.resources[TOOL].provenance["/source"].content_id.clone();
        assert_ne!(id(old), id(new), "{who}: shared source content");
        let timeout =
            |r: &Resolved| r.resources["reusable.tool-settings"].value["tool"]["timeout"].clone();
        assert_eq!(
            (timeout(old), timeout(new)),
            (val("30"), val("60")),
            "{who}: shared default"
        );
    }
    // The default editor moved for the machine that has no override of it.
    let editor = |r: &Resolved| r.resources["core.editor-defaults"].value["core"]["editor"].clone();
    assert_eq!(
        (editor(&a0), editor(&a1)),
        (val("\"nvim\""), val("\"emacs\""))
    );
    // The private override is preserved, with its origin.
    assert_eq!(editor(&b0), val("\"vim\""));
    assert_eq!(editor(&b1), val("\"vim\""));
    let p = &b1.resources["core.editor-defaults"].provenance["/core/editor"];
    assert_eq!(p.changer.as_deref(), Some("private-mod"));
    assert_eq!(p.original, Some(val("\"emacs\"")));
    // The profile override is preserved too.
    let retries = &b1.resources["reusable.tool-settings"].value["tool"]["retries"];
    assert_eq!(retries, &val("5"));
}

#[test]
fn c08_public_resolution_does_not_need_private_files() {
    let full = synthetic();
    let public = public_only(&full);
    assert!(public.paths().all(|p| !p.starts_with("private/") && p != B));
    // Resolving without any private file gives the same answer as resolving with all of them.
    assert_eq!(go(&public, A), go(&full, A));
    // And the private target does need them: the same public set cannot resolve it.
    assert!(resolve(&public, B, &builtin_catalog()).is_err());
    // Nothing private leaks into the public resolution.
    let a = go(&full, A);
    assert!(!a.modules.iter().any(|m| m == "private-mod"));
    assert!(a.resources.keys().all(|k| !k.starts_with("private-mod.")));
    let dump = format!("{a:?}");
    assert!(!dump.contains("private"), "{dump}");
}
