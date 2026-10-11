//! `[sources]` in a bundle (DFM-53): the loader reads only the declared roots, skips excluded
//! paths without opening them, and the resolver refuses a source that points at either.

mod common;

use common::{BUNDLE, Fx, code, is_root, text};
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
