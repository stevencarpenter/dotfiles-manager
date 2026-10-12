//! Read-only observation of HOME for the planner (docs/architecture.md section 8, probe
//! allowlist item 1: `lstat`, `readlink`, directory listing and bounded reads of declared
//! paths, all through descriptor-relative operations).
//!
//! The walk starts at the opened HOME and goes one component at a time with `O_NOFOLLOW`, so a
//! link in a parent is reported, never followed. Nothing is created, written, repaired or run.
//! A path that cannot be read is reported as `Unknown`, never as a failure of the walk.

use crate::fsys::{Dir, Peek, PeekTo, Sys};
use dfm_model::Refusal;
use dfm_model::fold;
use dfm_model::plan::{
    Incomplete, LinkTo, Listing, MAX_HASHED_BYTES, MAX_LISTING, Node, Observations, Request, Seen,
    file_fingerprint,
};
use std::collections::BTreeMap;
use std::rc::Rc;

fn lower(to: PeekTo) -> LinkTo {
    match to {
        PeekTo::Dir => LinkTo::Dir,
        PeekTo::File => LinkTo::File,
        PeekTo::Other => LinkTo::Other,
        PeekTo::Dangling => LinkTo::Dangling,
        PeekTo::Loop => LinkTo::Loop,
    }
}

fn list(dir: &Dir) -> Listing {
    match dir.list() {
        Ok(l) if l.odd > 0 => Listing {
            names: l.names,
            incomplete: Some(Incomplete::NonUtf8),
        },
        Ok(mut l) if l.names.len() > MAX_LISTING => {
            l.names.truncate(MAX_LISTING);
            Listing {
                names: l.names,
                incomplete: Some(Incomplete::TooMany),
            }
        }
        Ok(l) => Listing {
            names: l.names,
            incomplete: None,
        },
        Err(_) => Listing {
            names: Vec::new(),
            incomplete: Some(Incomplete::Failed),
        },
    }
}

fn unknown(reason: &str) -> Node {
    Node::Unknown {
        reason: reason.into(),
    }
}

fn node(peek: Peek, dir: &Dir, name: &str, hash: bool) -> Node {
    match peek {
        Peek::Missing => Node::Missing {},
        Peek::Dir { mode, own } => Node::Dir { mode, own },
        Peek::File { mode, own, size } => {
            let sha256 = if !hash {
                None
            } else if size > MAX_HASHED_BYTES {
                return unknown("file is larger than the 16 MiB hash bound");
            } else {
                match dir.read_plain(name, MAX_HASHED_BYTES) {
                    Ok(Some(b)) => Some(file_fingerprint(&b)),
                    Ok(None) => return unknown("file changed while it was read"),
                    Err(_) => return unknown("file cannot be read; check its permissions"),
                }
            };
            Node::File {
                mode,
                own,
                size,
                sha256,
            }
        }
        Peek::Link { target, to, own } => Node::Link {
            target,
            to: lower(to),
            own,
        },
        Peek::Other { mode, own } => Node::Other { mode, own },
        Peek::Unknown(kind) => Node::Unknown {
            reason: format!("cannot be examined ({kind})"),
        },
    }
}

/// Observes every request and every proper prefix of its path, below `home`. A prefix that is
/// not a directory ends the walk for that request. A directory is listed once, however many
/// requests pass through it.
pub fn observe(sys: &Sys, home: &Dir, requests: &[Request]) -> Result<Observations, Refusal> {
    let mut out = Observations::default();
    // Entry names of each directory, by its HOME-relative path, for the case check.
    // `Rc` avoids cloning a listing per node; the saving is not measured.
    let mut names: BTreeMap<String, Rc<Listing>> = BTreeMap::new();
    for req in requests {
        let comps: Vec<&str> = req.path.split('/').collect();
        let mut dir = home.try_clone()?;
        let mut parent = String::new();
        for (i, name) in comps.iter().enumerate() {
            let path = comps[..=i].join("/");
            let last = i + 1 == comps.len();
            let want_hash = last && req.hash;
            // A prefix seen earlier is kept, unless this request needs the hash it lacks.
            let known = out
                .seen
                .get(&path)
                .is_some_and(|s| !(want_hash && matches!(s.node, Node::File { sha256: None, .. })));
            let peek = dir.peek(sys, name)?;
            let exists = !matches!(peek, Peek::Missing);
            let is_dir = matches!(peek, Peek::Dir { .. });
            if !known {
                // On a case-folding filesystem a lookup of the wrong case succeeds, so the
                // listing of the parent is what tells the two spellings apart.
                let siblings = Rc::clone(
                    names
                        .entry(parent.clone())
                        .or_insert_with(|| Rc::new(list(&dir))),
                );
                let al = if exists && !siblings.names.iter().any(|n| n == name) {
                    let f = fold(name);
                    siblings.names.iter().find(|n| fold(n) == f).cloned()
                } else {
                    None
                };
                let mut n = node(peek, &dir, name, want_hash);
                if exists && siblings.incomplete.is_some() && !matches!(n, Node::Unknown { .. }) {
                    n = unknown(
                        "parent directory cannot be fully listed to check for case aliases",
                    );
                }
                out.seen.insert(path.clone(), Seen { node: n, alias: al });
            }
            if !is_dir {
                break;
            }
            let next = match dir.peek_dir(name) {
                Ok(Some(d)) => d,
                // A directory that cannot be opened (no read permission) ends the walk. Below
                // it nothing can be seen, so the directory itself is the unknown fact, unless
                // it is the requested path and only its listing was wanted.
                Ok(None) | Err(_) => {
                    if last && req.list {
                        out.listings.entry(path.clone()).or_insert(Listing {
                            names: Vec::new(),
                            incomplete: Some(Incomplete::Failed),
                        });
                    } else if !last {
                        out.seen.insert(
                            path.clone(),
                            Seen {
                                node: unknown("directory cannot be opened; check its permissions"),
                                alias: None,
                            },
                        );
                    }
                    break;
                }
            };
            if last && req.list && !out.listings.contains_key(&path) {
                let l = names
                    .entry(path.clone())
                    .or_insert_with(|| Rc::new(list(&next)));
                out.listings.insert(path.clone(), Listing::clone(l));
            }
            dir = next;
            parent = path;
        }
    }
    Ok(out)
}

/// Adds the facts of `more` to `into`. A fact observed twice keeps the first one, so a later
/// pass can never add a hash to a path the first pass chose not to hash.
pub fn merge(into: &mut Observations, more: Observations) {
    for (k, v) in more.seen {
        into.seen.entry(k).or_insert(v);
    }
    for (k, v) in more.listings {
        into.listings.entry(k).or_insert(v);
    }
}
