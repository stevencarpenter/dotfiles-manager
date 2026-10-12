//! `[sources]` in a bundle (DFM-53): the loader reads only the declared roots, skips excluded
//! paths without opening them, and the resolver refuses a source that points at either.

mod common;

use common::{BUNDLE, Fx, code, fingerprint, is_root, text};
use dfm_model::plan::Saved;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

const JUNK_IGNORED: &str = "**/.venv";
const PLUGINS: &str = "base/modules/core/files/plugins";

/// The standard fixture with `[sources]` appended to the bundle. `roots` and `exclude` are TOML
/// array bodies. The fixture is bound, so `plan` and `explain` can run.
fn scoped(roots: &str, exclude: &str) -> Fx {
    let fx = Fx::unbound();
    fx.put(
        "base/bundle.toml",
        &format!("{BUNDLE}\n[sources]\nroots = [{roots}]\nexclude = [{exclude}]\n"),
    );
    bind(&fx);
    fx
}

fn bind(fx: &Fx) {
    let p = fx.root.join("profiles/t-01.toml");
    let o = fx.run(&[
        "bind",
        "--profile",
        p.to_str().unwrap(),
        "--machine-id",
        "m5-test",
    ]);
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
}

fn plan(fx: &Fx) -> std::process::Output {
    fx.run(&["plan", "--root", fx.root_arg()])
}

fn tree(fx: &Fx) -> String {
    let o = fx.run(&["plan", "--root", fx.root_arg(), "--json"]);
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    let s = Saved::parse(&o.stdout).unwrap();
    s.plan.sources[0].tree.clone()
}

fn junk(fx: &Fx) {
    // An ignored virtualenv binary over the 1 MiB read limit, and a plugin checkout with its own
    // .git, as in the owner's live checkout.
    let venv = fx.root.join("base/modules/extra/files/.venv/bin");
    fs::create_dir_all(&venv).unwrap();
    fs::write(venv.join("x"), vec![7u8; 2 << 20]).unwrap();
    fx.put(&format!("{PLUGINS}/tpm/.git/config"), "[core]\n");
    fx.put(&format!("{PLUGINS}/tpm/tpm"), "#!/bin/sh\n");
}

#[test]
fn without_sources_a_large_ignored_file_and_a_nested_git_are_refused_as_before() {
    let fx = Fx::new();
    junk(&fx);
    let o = plan(&fx);
    assert_eq!(code(&o), 3, "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("E_SOURCE_UNTRUSTED"),
        "{}",
        text(&o.stderr)
    );
    fs::remove_dir_all(fx.root.join(PLUGINS)).unwrap();
    let o = plan(&fx);
    assert_eq!(code(&o), 3);
    assert!(
        text(&o.stderr).contains("E_LIMIT_EXCEEDED"),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn excluded_junk_plans_clean_and_explain_uses_the_same_loader() {
    let fx = scoped(
        r#""base", "profiles""#,
        &format!(r#""{PLUGINS}", "{JUNK_IGNORED}""#),
    );
    junk(&fx);
    let o = plan(&fx);
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    let o = fx.run(&["explain", "core.git", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    // The same exclusion does not hide a nested .git that is not excluded.
    fx.put("base/modules/core/files/other/.git/config", "x");
    let o = plan(&fx);
    assert_eq!(code(&o), 3);
    assert!(
        text(&o.stderr).contains("E_SOURCE_UNTRUSTED"),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn a_source_in_an_excluded_path_is_refused_with_the_pattern_named() {
    let fx = scoped(r#""base", "profiles""#, &format!(r#""{JUNK_IGNORED}""#));
    fx.put(
        "base/modules/extra/files/.venv/tmux.conf",
        "set -g mouse on\n",
    );
    let m = fs::read_to_string(fx.root.join("base/modules/extra/module.toml")).unwrap();
    fx.put(
        "base/modules/extra/module.toml",
        &m.replace("files/tmux.conf", "files/.venv/tmux.conf"),
    );
    let o = plan(&fx);
    let err = text(&o.stderr);
    assert_eq!(code(&o), 3, "{err}");
    assert!(err.contains("E_SOURCE_ESCAPE"), "{err}");
    assert!(err.contains(&format!("`{JUNK_IGNORED}`")), "{err}");
    assert!(err.contains("base/modules/extra/files/.venv"), "{err}");
}

#[test]
fn a_manifest_outside_the_roots_is_refused_and_names_the_roots() {
    // The extra module's manifest is not under a root and not next to the bundle.
    let fx = scoped(r#""base/modules/core", "profiles""#, "");
    let o = plan(&fx);
    let err = text(&o.stderr);
    assert_eq!(code(&o), 3, "{err}");
    assert!(err.contains("E_SOURCE_ESCAPE"), "{err}");
    assert!(err.contains("base/modules/extra"), "{err}");
    assert!(err.contains("base/modules/core, profiles"), "{err}");
    // The profile is a manifest too.
    let fx = scoped(r#""base""#, "");
    let o = plan(&fx);
    let err = text(&o.stderr);
    assert_eq!(code(&o), 3, "{err}");
    assert!(err.contains("E_SOURCE_ESCAPE"), "{err}");
    assert!(err.contains("profiles/t-01.toml"), "{err}");
}

fn many(dir: &Path, n: usize) {
    fs::create_dir_all(dir).unwrap();
    for i in 0..n {
        fs::write(dir.join(format!("f{i}")), "").unwrap();
    }
}

#[test]
fn the_entry_cap_applies_to_the_loaded_set_and_names_the_largest_directories() {
    let fx = scoped(r#""base", "profiles""#, "");
    let core = fx.root.join("base/modules/core/files");
    many(&core.join("cache"), 22_000);
    many(&core.join("logs"), 2_000);
    many(&core.join("tmp"), 900);
    many(&core.join("small"), 10);
    let o = plan(&fx);
    let err = text(&o.stderr);
    assert_eq!(code(&o), 3, "{err}");
    assert!(err.contains("E_LIMIT_EXCEEDED"), "{err}");
    let (c, l, m) = (
        err.find("files/cache (22000 entries)"),
        err.find("files/logs (2000 entries)"),
        err.find("files/tmp (900 entries)"),
    );
    assert!(c < l && l < m && c.is_some(), "{err}");
    assert!(!err.contains("files/small"), "{err}");
    assert!(err.contains("[sources]"), "{err}");
    // 25,000 entries on disk plan clean once the big directories are excluded.
    fx.put(
        "base/bundle.toml",
        &format!(
            "{BUNDLE}\n[sources]\nroots = [\"base\", \"profiles\"]\nexclude = [\"**/cache\", \"**/logs\", \"**/tmp\"]\n"
        ),
    );
    let o = plan(&fx);
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
}

#[test]
fn the_cap_message_names_the_top_three_without_sources_too() {
    let fx = Fx::new();
    let core = fx.root.join("base/modules/core/files");
    many(&core.join("cache"), 21_000);
    many(&core.join("logs"), 5);
    let o = plan(&fx);
    let err = text(&o.stderr);
    assert_eq!(code(&o), 3, "{err}");
    assert!(err.contains("base/modules/core/files/cache"), "{err}");
}

#[test]
fn the_digest_follows_loaded_files_and_ignores_excluded_ones() {
    let fx = scoped(r#""base", "profiles""#, &format!(r#""{JUNK_IGNORED}""#));
    fx.put("base/modules/extra/files/.venv/a", "1");
    let t0 = tree(&fx);
    fx.put("base/modules/extra/files/.venv/a", "2");
    fx.put("base/modules/extra/files/.venv/b", "new");
    assert_eq!(tree(&fx), t0, "an excluded file changed the digest");
    fx.put("base/modules/extra/files/zshrc", "export A=2\n");
    let t1 = tree(&fx);
    assert_ne!(t1, t0, "a loaded file did not change the digest");
    // A file outside every root is not loaded either.
    fx.put("outside/readme", "x");
    assert_eq!(tree(&fx), t1);
}

struct Restore(PathBuf, u32);

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(self.1));
    }
}

#[test]
fn an_excluded_path_is_never_opened_or_listed_by_a_non_root_user() {
    if is_root() {
        return;
    }
    let fx = scoped(
        r#""base", "profiles""#,
        r#""base/modules/core/files/vault", "**/secret""#,
    );
    // Opening or listing either of these fails for a non-root user, so a clean plan proves that
    // the loader did neither.
    fx.put("base/modules/core/files/vault/inside", "x");
    fx.put("base/modules/core/files/secret", "x");
    let dir = fx.root.join("base/modules/core/files/vault");
    let file = fx.root.join("base/modules/core/files/secret");
    let _a = Restore(dir.clone(), 0o755);
    let _b = Restore(file.clone(), 0o644);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
    let o = plan(&fx);
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    // Without the exclusion the same tree fails, so the check above is not vacuous.
    let bare = scoped(r#""base", "profiles""#, "");
    bare.put("base/modules/core/files/secret", "x");
    let f = bare.root.join("base/modules/core/files/secret");
    let _c = Restore(f.clone(), 0o644);
    fs::set_permissions(&f, fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(code(&plan(&bare)), 2);
    // The same for a directory: listing it fails for a non-root user.
    let dir_fx = scoped(r#""base", "profiles""#, "");
    dir_fx.put("base/modules/core/files/vault/inside", "x");
    let d = dir_fx.root.join("base/modules/core/files/vault");
    let _d = Restore(d.clone(), 0o755);
    fs::set_permissions(&d, fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(code(&plan(&dir_fx)), 2);
}

#[test]
fn a_root_must_be_a_real_directory() {
    let fx = scoped(r#""base", "profiles", "nope""#, "");
    let o = plan(&fx);
    let err = text(&o.stderr);
    assert_eq!(code(&o), 3, "{err}");
    assert!(
        err.contains("E_MANIFEST_INVALID") && err.contains("`nope`"),
        "{err}"
    );
    let fx = scoped(r#""base", "profiles", "alias""#, "");
    symlink("base", fx.root.join("alias")).unwrap();
    let o = plan(&fx);
    let err = text(&o.stderr);
    assert_eq!(code(&o), 3, "{err}");
    assert!(
        err.contains("E_SOURCE_ESCAPE") && err.contains("`alias`"),
        "{err}"
    );
}

#[test]
fn only_the_bundle_the_profile_names_may_declare_sources() {
    let fx = scoped(r#""base", "profiles", "ext""#, "");
    fx.put(
        "ext/bundle.toml",
        "schema = 1\nid = \"ext\"\nextends = \"../base/bundle.toml\"\n\n[modules]\n",
    );
    let p = fs::read_to_string(fx.root.join("profiles/t-01.toml")).unwrap();
    fx.put(
        "profiles/t-01.toml",
        &p.replace("../base/bundle.toml", "../ext/bundle.toml"),
    );
    let o = plan(&fx);
    let err = text(&o.stderr);
    assert_eq!(code(&o), 3, "{err}");
    assert!(err.contains("E_MANIFEST_INVALID"), "{err}");
    assert!(err.contains("base/bundle.toml"), "{err}");
}

#[test]
fn an_invalid_declaration_is_manifest_invalid_with_exit_3() {
    for body in [
        "roots = []",
        "roots = [\"../x\"]",
        "roots = [\"base\"]\nexclude = [\"a/../b\"]",
    ] {
        let fx = Fx::unbound();
        fx.put(
            "base/bundle.toml",
            &format!("{BUNDLE}\n[sources]\n{body}\n"),
        );
        bind(&fx);
        let o = plan(&fx);
        assert_eq!(code(&o), 3, "{body}: {}", text(&o.stderr));
        assert!(text(&o.stderr).contains("E_MANIFEST_INVALID"), "{body}");
    }
}

fn mkfifo(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    assert!(
        Command::new("/usr/bin/mkfifo")
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn an_excluded_special_file_is_skipped_and_an_unexcluded_one_is_refused() {
    // Works for root too: no permission bit is involved.
    let fx = scoped(r#""base", "profiles""#, r#""**/pipe""#);
    mkfifo(&fx.root.join("base/modules/core/files/pipe"));
    assert_eq!(code(&plan(&fx)), 0);
    let bare = scoped(r#""base", "profiles""#, "");
    mkfifo(&bare.root.join("base/modules/core/files/pipe"));
    let o = plan(&bare);
    assert_eq!(code(&o), 3);
    assert!(
        text(&o.stderr).contains("E_SOURCE_UNTRUSTED"),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn an_excluded_entry_with_a_control_or_non_utf8_name_does_not_abort_the_load() {
    let fx = scoped(r#""base", "profiles""#, r#""**/bad*""#);
    fx.put("base/modules/core/files/bad\u{1}name", "x");
    // APFS refuses a non-UTF-8 name; where the filesystem allows one, it must be skipped too.
    let raw = fx
        .root
        .join("base/modules/core/files")
        .join(std::ffi::OsStr::from_bytes(b"bad\xff"));
    let _ = fs::write(raw, "x");
    assert_eq!(code(&plan(&fx)), 0, "{}", text(&plan(&fx).stderr));
    let bare = scoped(r#""base", "profiles""#, "");
    bare.put("base/modules/core/files/bad\u{1}name", "x");
    let o = plan(&bare);
    assert_eq!(code(&o), 3);
    assert!(
        text(&o.stderr).contains("E_SOURCE_UNTRUSTED"),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn editing_only_the_sources_table_changes_the_digest() {
    let fx = scoped(r#""base", "profiles""#, r#""**/never-matches""#);
    let t0 = tree(&fx);
    fx.put(
        "base/bundle.toml",
        &format!(
            "{BUNDLE}\n[sources]\nroots = [\"base\", \"profiles\"]\nexclude = [\"**/also-never-matches\"]\n"
        ),
    );
    assert_ne!(tree(&fx), t0);
}

const PLUGS: &str = r#"
[[resources]]
id = "plugs"
kind = "link"
adapter = "fs-link/v1"
target = ".config/plugs"
[resources.value]
source = "files/plugdir"
directory = true
"#;

fn with_plugs(fx: &Fx) {
    let m = fs::read_to_string(fx.root.join("base/modules/extra/module.toml")).unwrap();
    fx.put("base/modules/extra/module.toml", &format!("{m}{PLUGS}"));
    fx.put("base/modules/extra/files/plugdir/keep", "k");
    fx.put("base/modules/extra/files/plugdir/.venv/lib", "x");
    fx.put("base/modules/extra/files/plugdir/node_modules/m", "x");
}

#[test]
fn a_directory_link_with_excluded_content_is_noted_and_counted() {
    let fx = scoped(r#""base", "profiles""#, r#""**/.venv", "**/node_modules""#);
    with_plugs(&fx);
    let o = plan(&fx);
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert_eq!(
        out.matches("note: contains 2 excluded entries, not hashed")
            .count(),
        1,
        "{out}"
    );
    let o = fx.run(&["plan", "--root", fx.root_arg(), "--json"]);
    let s = Saved::parse(&o.stdout).unwrap();
    let ops: Vec<_> = s
        .plan
        .operations
        .iter()
        .filter(|o| o.excluded.is_some())
        .collect();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].resource, "extra.plugs");
    assert_eq!(ops[0].excluded, Some(2));
    // A plan with nothing pruned has no note and no field.
    let plain = scoped(r#""base", "profiles""#, "");
    let o = plan(&plain);
    assert!(!text(&o.stdout).contains("note:"));
}

/// The operation that links `extra.plugs`, from a JSON plan.
fn plugs_op(fx: &Fx) -> dfm_model::plan::Operation {
    let o = fx.run(&["plan", "--root", fx.root_arg(), "--json"]);
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    Saved::parse(&o.stdout)
        .unwrap()
        .plan
        .operations
        .into_iter()
        .find(|o| o.resource == "extra.plugs")
        .expect("the plugs operation")
}

fn add_plugs_module(fx: &Fx) {
    let m = fs::read_to_string(fx.root.join("base/modules/extra/module.toml")).unwrap();
    fx.put("base/modules/extra/module.toml", &format!("{m}{PLUGS}"));
}

#[test]
fn an_empty_directory_source_plans_as_a_directory_link() {
    // No entry at all below the directory.
    let fx = scoped(r#""base", "profiles""#, "");
    add_plugs_module(&fx);
    fs::create_dir_all(fx.root.join("base/modules/extra/files/plugdir")).unwrap();
    let op = plugs_op(&fx);
    assert_eq!(op.action, dfm_model::plan::Action::CreateLink);
    assert!(op.inputs[0].path.ends_with("files/plugdir"), "{op:?}");
    assert!(op.inputs[0].content_id.is_some());
    // Every child excluded: the directory is still a directory, and the plan counts the entry.
    let fx = scoped(r#""base", "profiles""#, r#""**/.venv""#);
    add_plugs_module(&fx);
    fx.put("base/modules/extra/files/plugdir/.venv/lib", "x");
    let op = plugs_op(&fx);
    assert_eq!(op.action, dfm_model::plan::Action::CreateLink);
    assert_eq!(op.excluded, Some(1));
}

#[test]
fn an_empty_subdirectory_is_part_of_the_directory_identity_and_the_tree_digest() {
    let fx = scoped(r#""base", "profiles""#, "");
    with_plugs(&fx);
    let (id0, t0) = (plugs_op(&fx).inputs[0].content_id.clone(), tree(&fx));
    fs::create_dir(fx.root.join("base/modules/extra/files/plugdir/empty")).unwrap();
    let (id1, t1) = (plugs_op(&fx).inputs[0].content_id.clone(), tree(&fx));
    assert_ne!(id0, id1, "directory identity");
    assert_ne!(t0, t1, "source tree digest");
    // A nested empty directory changes it again.
    fs::create_dir(fx.root.join("base/modules/extra/files/plugdir/empty/inner")).unwrap();
    assert_ne!(plugs_op(&fx).inputs[0].content_id, id1);
}

#[test]
fn a_link_hop_into_an_excluded_path_is_refused() {
    let fx = scoped(r#""base", "profiles""#, &format!(r#""{JUNK_IGNORED}""#));
    fx.put(
        "base/modules/extra/files/.venv/tmux.conf",
        "set -g mouse on\n",
    );
    symlink(
        ".venv/tmux.conf",
        fx.root.join("base/modules/extra/files/hop"),
    )
    .unwrap();
    let m = fs::read_to_string(fx.root.join("base/modules/extra/module.toml")).unwrap();
    fx.put(
        "base/modules/extra/module.toml",
        &m.replace("files/tmux.conf", "files/hop"),
    );
    let o = plan(&fx);
    let err = text(&o.stderr);
    assert_eq!(code(&o), 3, "{err}");
    assert!(
        err.contains("E_SOURCE_ESCAPE") && err.contains(JUNK_IGNORED),
        "{err}"
    );
}

#[test]
fn similar_root_names_a_link_loop_and_siblings_load_independently() {
    let fx = scoped(r#""base", "profiles", "home", "home2""#, "");
    fx.put("home/a", "1");
    fx.put("home2/b", "1");
    symlink("loop2", fx.root.join("home/loop1")).unwrap();
    symlink("loop1", fx.root.join("home/loop2")).unwrap();
    let t0 = tree(&fx);
    fx.put("home2/b", "2");
    assert_ne!(tree(&fx), t0, "home2 was not loaded");
    let t1 = tree(&fx);
    fx.put("home3/c", "x");
    assert_eq!(tree(&fx), t1, "home3 must not load next to home and home2");
}

#[test]
fn a_root_below_an_unreadable_directory_reports_the_real_error() {
    if is_root() {
        return;
    }
    let fx = scoped(r#""base", "profiles", "locked/inner""#, "");
    fx.put("locked/inner/x", "1");
    let dir = fx.root.join("locked");
    let _r = Restore(dir.clone(), 0o755);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
    let o = plan(&fx);
    let err = text(&o.stderr);
    assert_eq!(code(&o), 2, "{err}");
    assert!(
        err.contains("E_REPORT_UNAVAILABLE") && err.contains("permission denied"),
        "{err}"
    );
}

#[test]
fn the_loader_and_the_resolver_must_reach_the_same_bundle() {
    // The loader collapses `..` lexically and reads base/bundle.toml. The resolver follows the
    // link and reads real/bundle.toml. A scope that describes the other file must be refused.
    let decoy = BUNDLE;
    let fx = Fx::unbound();
    fx.put(
        "base/bundle.toml",
        &format!("{BUNDLE}\n[sources]\nroots = [\"base\", \"profiles\", \"real\"]\n"),
    );
    fx.put("real/deep/keep", "k");
    symlink("../real/deep", fx.root.join("base/lnk")).unwrap();
    fx.put("real/bundle.toml", decoy);
    let p = fs::read_to_string(fx.root.join("profiles/t-01.toml")).unwrap();
    fx.put(
        "profiles/t-01.toml",
        &p.replace("../base/bundle.toml", "../base/lnk/../bundle.toml"),
    );
    bind(&fx);
    let o = plan(&fx);
    let err = text(&o.stderr);
    assert_eq!(code(&o), 3, "{err}");
    assert!(err.contains("E_MANIFEST_INVALID"), "{err}");
    assert!(
        err.contains("base/bundle.toml") && err.contains("real/bundle.toml"),
        "{err}"
    );
    // The other way round: the loader found no scope, the resolved bundle declares one.
    fx.put("base/bundle.toml", BUNDLE);
    fx.put(
        "real/bundle.toml",
        &format!("{BUNDLE}\n[sources]\nroots = [\"base\", \"profiles\", \"real\"]\n"),
    );
    let o = plan(&fx);
    let err = text(&o.stderr);
    assert_eq!(code(&o), 3, "{err}");
    assert!(
        err.contains("E_MANIFEST_INVALID") && err.contains("real/bundle.toml"),
        "{err}"
    );
}

#[test]
fn git_metadata_beside_the_bundle_is_skipped_but_below_a_root_is_refused() {
    let fx = scoped(r#""base/modules", "profiles""#, "");
    fx.put("base/.git/config", "[core]\n");
    assert_eq!(code(&plan(&fx)), 0, "{}", text(&plan(&fx).stderr));
    let held = scoped(r#""base", "profiles""#, "");
    held.put("base/.git/config", "[core]\n");
    let o = plan(&held);
    assert_eq!(code(&o), 3);
    assert!(
        text(&o.stderr).contains("E_SOURCE_UNTRUSTED"),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn an_exclude_that_names_the_bundle_directory_is_refused() {
    let fx = scoped(r#""profiles""#, r#""base""#);
    fx.put("base/big", &"x".repeat(2 << 20));
    let o = plan(&fx);
    let err = text(&o.stderr);
    assert_eq!(code(&o), 3, "{err}");
    assert!(
        err.contains("E_MANIFEST_INVALID") && err.contains("bundle directory"),
        "{err}"
    );
}

/// Refuses `plan` with `want` in stderr and exit 3, and proves HOME (and with it the state
/// store) is byte for byte what it was.
fn plan_refused(fx: &Fx, want: &str) -> String {
    let before = fingerprint(fx.home.path());
    let o = plan(fx);
    let err = text(&o.stderr);
    assert_eq!(code(&o), 3, "{err}");
    assert!(err.contains(want), "{want} not in: {err}");
    assert_eq!(fingerprint(fx.home.path()), before, "HOME changed");
    err
}

const PLUGDIR: &str = "base/modules/extra/files/plugdir";

#[test]
fn a_link_to_a_directory_inside_a_linked_tree_must_not_lead_out_of_the_root() {
    // plugdir/ln -> ../x (a directory in the module root), and x/esc -> /etc.
    let fx = scoped(r#""base", "profiles""#, "");
    add_plugs_module(&fx);
    fx.put(&format!("{PLUGDIR}/keep"), "k");
    fx.put("base/modules/extra/files/x/inside", "i");
    let x = fx.root.join("base/modules/extra/files/x");
    symlink("../x", fx.root.join(PLUGDIR).join("ln")).unwrap();
    symlink("/etc", x.join("esc")).unwrap();
    plan_refused(&fx, "E_SOURCE_ESCAPE");
    // Control: the same layout with the inner link kept inside the root plans.
    fs::remove_file(x.join("esc")).unwrap();
    symlink("inside", x.join("esc")).unwrap();
    assert_eq!(code(&plan(&fx)), 0, "{}", text(&plan(&fx).stderr));
}

#[test]
fn an_excluded_link_that_leaves_the_root_is_still_refused_in_a_directory_source() {
    let fx = scoped(r#""base", "profiles""#, r#""**/esc""#);
    add_plugs_module(&fx);
    fx.put(&format!("{PLUGDIR}/keep"), "k");
    let esc = fx.root.join(PLUGDIR).join("esc");
    symlink("/etc", &esc).unwrap();
    plan_refused(&fx, "E_SOURCE_ESCAPE");
    // A relative target that climbs out of the root is refused the same way.
    fs::remove_file(&esc).unwrap();
    symlink("../../../../../../../../etc", &esc).unwrap();
    plan_refused(&fx, "E_SOURCE_ESCAPE");
    // Control: an excluded link that stays inside the root plans and is still not hashed.
    fs::remove_file(&esc).unwrap();
    symlink("keep", &esc).unwrap();
    let o = plan(&fx);
    assert_eq!(code(&o), 0, "{}", text(&o.stderr));
    assert!(text(&o.stdout).contains("note: contains 1 excluded entries, not hashed"));
}

#[test]
fn a_link_target_with_a_control_character_is_refused_like_a_name() {
    let fx = scoped(r#""base", "profiles""#, "");
    add_plugs_module(&fx);
    fx.put(&format!("{PLUGDIR}/keep"), "k");
    symlink("keep\u{1b}[2J", fx.root.join(PLUGDIR).join("ln")).unwrap();
    let err = plan_refused(&fx, "E_SOURCE_UNTRUSTED");
    assert!(err.contains("control character"), "{err}");
    assert!(!err.contains('\u{1b}'), "{err:?}");
}

#[test]
fn a_backslash_in_a_refused_path_is_escaped_once() {
    let fx = scoped(r#""base", "profiles""#, "");
    mkfifo(&fx.root.join("base/modules/core/files/pi\\npe"));
    let err = plan_refused(&fx, "E_SOURCE_UNTRUSTED");
    assert!(err.contains("special file at"), "{err}");
    // One backslash in the name is two in the output, never four, and not a bare `\n`.
    assert!(err.contains("files/pi\\\\npe"), "{err}");
    assert!(!err.contains("pi\\\\\\\\npe"), "{err}");
    // `doctor` puts the same text in a row, which escapes it itself.
    let before = fingerprint(fx.home.path());
    let o = fx.run(&["doctor", "--root", fx.root_arg()]);
    let all = format!("{}{}", text(&o.stdout), text(&o.stderr));
    assert!(all.contains(r"files/pi\\npe"), "{all}");
    assert!(!all.contains(r"pi\\\\npe"), "{all}");
    assert_eq!(fingerprint(fx.home.path()), before, "HOME changed");
}

/// Every path below `dir` (the lock file apart, which every mutating command rewrites with its
/// holder note), the content of the binding and of the whole state store.
fn home_state(fx: &Fx) -> (Vec<PathBuf>, String, String) {
    fn paths(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.file_name().unwrap() == "dotfiles-manager.lock" {
                continue;
            }
            out.push(p.clone());
            if p.symlink_metadata().unwrap().is_dir() {
                paths(&p, out);
            }
        }
    }
    let mut all = Vec::new();
    paths(fx.home.path(), &mut all);
    (
        all,
        fingerprint(&fx.h(".config")),
        fingerprint(&fx.h(".local/state/dotfiles-manager")),
    )
}

/// Applies the plan and expects a refusal that names `want`. The first refusal may create the
/// state store. A second one must then leave HOME and the whole store as they were, and no link
/// may have been published.
fn apply_refused(fx: &Fx, digest: &str, want: &str) -> String {
    let apply = || {
        let o = fx.run(&["apply", "--root", fx.root_arg(), "--yes-plan", digest]);
        let all = format!("{}{}", text(&o.stdout), text(&o.stderr));
        assert_ne!(code(&o), 0, "{all}");
        assert!(all.contains(want), "{want} not in: {all}");
        all
    };
    apply();
    let before = home_state(fx);
    let all = apply();
    assert_eq!(home_state(fx), before, "HOME or the state store changed");
    assert!(
        fs::symlink_metadata(fx.h(".config/plugs")).is_err(),
        "the directory link was published"
    );
    all
}

#[test]
fn apply_refuses_a_link_planted_in_excluded_content_after_the_plan() {
    let fx = scoped(r#""base", "profiles""#, r#""**/.venv""#);
    with_plugs(&fx);
    let o = fx.run(&["plan", "--root", fx.root_arg(), "--json"]);
    let digest = Saved::parse(&o.stdout).unwrap().plan_digest;
    // The excluded directory is neither listed nor hashed, so the plan digest does not change.
    let venv = fx.root.join(PLUGDIR).join(".venv");
    symlink("/etc", venv.join("esc")).unwrap();
    apply_refused(&fx, &digest, "E_SOURCE_ESCAPE");
    // Control: a link that stays inside the source root applies.
    fs::remove_file(venv.join("esc")).unwrap();
    symlink("lib", venv.join("ok")).unwrap();
    let o = fx.run(&["apply", "--root", fx.root_arg(), "--yes-plan", &digest]);
    assert_eq!(code(&o), 0, "{}{}", text(&o.stdout), text(&o.stderr));
    assert!(fs::symlink_metadata(fx.h(".config/plugs")).is_ok());
}

#[test]
fn a_backslash_in_a_refused_apply_message_is_escaped_once() {
    let fx = scoped(r#""base", "profiles""#, r#""**/.venv""#);
    let m = fs::read_to_string(fx.root.join("base/modules/extra/module.toml")).unwrap();
    let plugs = PLUGS.replace("files/plugdir", r"files/plug\\dir");
    fx.put("base/modules/extra/module.toml", &format!("{m}{plugs}"));
    let dir = fx.root.join(r"base/modules/extra/files/plug\dir");
    fx.put(r"base/modules/extra/files/plug\dir/.venv/lib", "x");
    let o = fx.run(&["plan", "--root", fx.root_arg(), "--json"]);
    let digest = Saved::parse(&o.stdout).unwrap().plan_digest;
    symlink("/etc", dir.join(".venv/esc")).unwrap();
    let all = apply_refused(&fx, &digest, "E_SOURCE_ESCAPE");
    assert!(all.contains(r"files/plug\\dir"), "{all}");
    assert!(!all.contains(r"plug\\\\dir"), "{all}");
}

#[test]
fn apply_holds_links_in_excluded_content_to_the_directory_source_not_the_root() {
    let fx = scoped(r#""base", "profiles""#, r#""**/.venv""#);
    with_plugs(&fx);
    let o = fx.run(&["plan", "--root", fx.root_arg(), "--json"]);
    let digest = Saved::parse(&o.stdout).unwrap().plan_digest;
    // The target stays under --root (in the core module) but leaves the directory source.
    let venv = fx.root.join(PLUGDIR).join(".venv");
    let out = "../../../../core/files/gitconfig";
    assert!(venv.join(out).exists(), "the target must exist: {out}");
    symlink(out, venv.join("up")).unwrap();
    apply_refused(&fx, &digest, "E_SOURCE_ESCAPE");
    // A chain: the first link stays inside, the second climbs from where the first points.
    fs::remove_file(venv.join("up")).unwrap();
    symlink("..", venv.join("l1")).unwrap();
    symlink("l1/../x", venv.join("l3")).unwrap();
    apply_refused(&fx, &digest, "E_SOURCE_ESCAPE");
}

#[test]
fn an_excluded_link_target_with_a_control_character_is_refused_like_a_name() {
    // Exclusion hides the link from hashing, not from the checks on its target text.
    let fx = scoped(r#""base", "profiles""#, r#""**/esc""#);
    add_plugs_module(&fx);
    fx.put(&format!("{PLUGDIR}/keep"), "k");
    symlink("keep\u{1b}[2J", fx.root.join(PLUGDIR).join("esc")).unwrap();
    let err = plan_refused(&fx, "E_SOURCE_UNTRUSTED");
    assert!(err.contains("control character"), "{err}");
    assert!(!err.contains('\u{1b}'), "{err:?}");
}
