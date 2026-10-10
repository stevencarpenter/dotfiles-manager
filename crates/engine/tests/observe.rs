//! Read-only observation of HOME for the planner: topology, hashing, listing, case aliases and
//! the guarantee that nothing is created or changed.

mod common;

use common::*;
use dfm_engine::{Dir, observe};
use dfm_model::digest::sha256_hex;
use dfm_model::plan::{LinkTo, MAX_HASHED_BYTES, Node, Request};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

fn req(path: &str, hash: bool, list: bool) -> Request {
    Request {
        path: path.into(),
        hash,
        list,
    }
}

fn seen(h: &Home, reqs: &[Request]) -> dfm_model::plan::Observations {
    let s = sys(h);
    let dir = Dir::open_home(&s, &h.0).unwrap();
    observe(&s, &dir, reqs).unwrap()
}

#[test]
fn every_prefix_is_observed_and_a_missing_one_ends_the_walk() {
    let h = Home::new();
    fs::create_dir_all(h.0.join("a/b")).unwrap();
    let o = seen(&h, &[req("a/b/c/d", false, false)]);
    let keys: Vec<_> = o.seen.keys().map(String::as_str).collect();
    assert_eq!(keys, ["a", "a/b", "a/b/c"]);
    assert!(matches!(o.seen["a/b"].node, Node::Dir { own: true, .. }));
    assert_eq!(o.seen["a/b/c"].node, Node::Missing);
}

#[test]
fn links_are_reported_and_never_followed() {
    let h = Home::new();
    fs::create_dir_all(h.0.join("real")).unwrap();
    fs::write(h.0.join("real/inside"), "x").unwrap();
    fs::write(h.0.join("file"), "x").unwrap();
    symlink("real", h.0.join("to-dir")).unwrap();
    symlink("file", h.0.join("to-file")).unwrap();
    symlink("gone", h.0.join("dangling")).unwrap();
    symlink("loop2", h.0.join("loop1")).unwrap();
    symlink("loop1", h.0.join("loop2")).unwrap();
    let o = seen(
        &h,
        &[
            req("to-dir/inside", true, false),
            req("to-file", false, false),
            req("dangling", false, false),
            req("loop1", false, false),
        ],
    );
    let to = |p: &str| match &o.seen[p].node {
        Node::Link { to, .. } => *to,
        n => panic!("{p}: {n:?}"),
    };
    assert_eq!(to("to-dir"), LinkTo::Dir);
    assert_eq!(to("to-file"), LinkTo::File);
    assert_eq!(to("dangling"), LinkTo::Dangling);
    assert_eq!(to("loop1"), LinkTo::Loop);
    // The walk stopped at the link: the file behind it was not observed.
    assert!(!o.seen.contains_key("to-dir/inside"));
}

#[test]
fn files_are_hashed_only_on_request_and_within_the_bound() {
    let h = Home::new();
    fs::write(h.0.join("f"), "content").unwrap();
    let big = fs::File::create(h.0.join("big")).unwrap();
    big.set_len(MAX_HASHED_BYTES + 1).unwrap();
    let o = seen(&h, &[req("f", true, false), req("big", true, false)]);
    match &o.seen["f"].node {
        Node::File { sha256, size, .. } => {
            assert_eq!(sha256.as_deref(), Some(sha256_hex(b"content").as_str()));
            assert_eq!(*size, 7);
        }
        n => panic!("{n:?}"),
    }
    assert!(matches!(o.seen["big"].node, Node::Unknown { .. }));
    let o = seen(&h, &[req("f", false, false)]);
    assert!(matches!(o.seen["f"].node, Node::File { sha256: None, .. }));
    // A later request that needs the hash upgrades an earlier unhashed observation.
    let o = seen(&h, &[req("f", false, false), req("f", true, false)]);
    assert!(matches!(
        o.seen["f"].node,
        Node::File {
            sha256: Some(_),
            ..
        }
    ));
}

#[test]
fn a_directory_is_listed_on_request() {
    let h = Home::new();
    fs::create_dir_all(h.0.join("d")).unwrap();
    for n in ["b", "a", "c"] {
        fs::write(h.0.join("d").join(n), n).unwrap();
    }
    let o = seen(&h, &[req("d", false, true)]);
    assert_eq!(o.listings["d"].names, ["a", "b", "c"]);
    assert!(!o.listings["d"].truncated);
    let o = seen(&h, &[req("d", false, false)]);
    assert!(o.listings.is_empty());
}

#[test]
fn a_wrong_case_spelling_is_reported_as_an_alias_when_the_filesystem_folds_case() {
    let h = Home::new();
    fs::create_dir_all(h.0.join("Config")).unwrap();
    let folds = h.0.join("config").exists();
    let o = seen(&h, &[req("config/x", false, false)]);
    if folds {
        assert_eq!(o.seen["config"].alias.as_deref(), Some("Config"));
    } else {
        eprintln!("SKIPPED: this volume is case sensitive");
        assert_eq!(o.seen["config"].node, Node::Missing);
    }
    // The exact spelling is never an alias.
    let o = seen(&h, &[req("Config", false, false)]);
    assert_eq!(o.seen["Config"].alias, None);
}

#[test]
fn ownership_is_reported_as_a_flag_and_observation_changes_nothing() {
    let h = Home::new();
    fs::create_dir_all(h.0.join("d")).unwrap();
    fs::write(h.0.join("d/f"), "x").unwrap();
    fs::set_permissions(h.0.join("d"), fs::Permissions::from_mode(0o500)).unwrap();
    let before = tree(&h.0);
    let o = seen(&h, &[req("d/f", true, true)]);
    assert!(matches!(
        o.seen["d"].node,
        Node::Dir {
            mode: 0o500,
            own: true
        }
    ));
    assert_eq!(tree(&h.0), before);
    fs::set_permissions(h.0.join("d"), fs::Permissions::from_mode(0o700)).unwrap();
}

fn tree(root: &std::path::Path) -> Vec<(String, u32, u64)> {
    fn go(d: &std::path::Path, out: &mut Vec<(String, u32, u64)>) {
        for e in fs::read_dir(d).unwrap() {
            let e = e.unwrap();
            let m = fs::symlink_metadata(e.path()).unwrap();
            out.push((
                e.path().display().to_string(),
                m.permissions().mode(),
                m.modified()
                    .unwrap()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64,
            ));
            if m.is_dir() {
                go(&e.path(), out);
            }
        }
    }
    let mut v = Vec::new();
    go(root, &mut v);
    v.sort();
    v
}
