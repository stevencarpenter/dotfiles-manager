//! Read-only observation of HOME for the planner (docs/architecture.md section 8, probe
//! allowlist item 1: `lstat`, `readlink`, directory listing and bounded reads of declared
//! paths, all through descriptor-relative operations).
//!
//! The walk starts at the opened HOME and goes one component at a time with `O_NOFOLLOW`, so a
//! link in a parent is reported, never followed. Nothing is created, written, repaired or run.

use crate::fsys::{Dir, Peek, PeekTo, Sys};
use dfm_model::Refusal;
use dfm_model::digest::sha256_hex;
use dfm_model::plan::{
    LinkTo, Listing, MAX_HASHED_BYTES, MAX_LISTING, Node, Observations, Request, Seen,
};

fn lower(to: PeekTo) -> LinkTo {
    match to {
        PeekTo::Dir => LinkTo::Dir,
        PeekTo::File => LinkTo::File,
        PeekTo::Other => LinkTo::Other,
        PeekTo::Dangling => LinkTo::Dangling,
        PeekTo::Loop => LinkTo::Loop,
    }
}

/// Entry names of one directory, with the directory kept open for the next step.
struct Level {
    dir: Dir,
    names: Option<Vec<String>>,
}

fn list(dir: &Dir) -> Listing {
    match dir.list() {
        Ok(l) if l.odd == 0 && l.names.len() <= MAX_LISTING => Listing {
            names: l.names,
            truncated: false,
        },
        Ok(mut l) => {
            l.names.truncate(MAX_LISTING);
            Listing {
                names: l.names,
                truncated: true,
            }
        }
        Err(_) => Listing {
            names: Vec::new(),
            truncated: true,
        },
    }
}

/// The spelling on disk when it differs from `name` only by case.
fn alias(names: &Option<Vec<String>>, name: &str) -> Option<String> {
    let names = names.as_ref()?;
    if names.iter().any(|n| n == name) {
        return None;
    }
    let f = name.to_lowercase();
    names.iter().find(|n| n.to_lowercase() == f).cloned()
}

fn node(peek: Peek, dir: &Dir, name: &str, hash: bool) -> Result<Node, Refusal> {
    Ok(match peek {
        Peek::Missing => Node::Missing,
        Peek::Dir { mode, own } => Node::Dir { mode, own },
        Peek::File { mode, own, size } => {
            let sha256 = if !hash {
                None
            } else if size > MAX_HASHED_BYTES {
                return Ok(Node::Unknown {
                    reason: "file is larger than the 16 MiB hash bound".into(),
                });
            } else {
                dir.read_plain(name, MAX_HASHED_BYTES)?
                    .map(|b| sha256_hex(&b))
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
        Peek::Unknown(reason) => Node::Unknown { reason },
    })
}

/// Observes every request and every proper prefix of its path, below `home`. A prefix that is
/// not a directory ends the walk for that request.
pub fn observe(sys: &Sys, home: &Dir, requests: &[Request]) -> Result<Observations, Refusal> {
    let mut out = Observations::default();
    for req in requests {
        let comps: Vec<&str> = req.path.split('/').collect();
        let mut level = Level {
            dir: home.try_clone()?,
            names: None,
        };
        for (i, name) in comps.iter().enumerate() {
            let path = comps[..=i].join("/");
            let last = i + 1 == comps.len();
            let want_hash = last && req.hash;
            // A prefix seen earlier is kept, unless this request needs the hash it lacks.
            let known = out
                .seen
                .get(&path)
                .is_some_and(|s| !(want_hash && matches!(s.node, Node::File { sha256: None, .. })));
            // The listing of the parent is needed for the case check of every existing entry.
            if level.names.is_none() {
                let l = list(&level.dir);
                level.names = (!l.truncated).then_some(l.names);
            }
            let peek = level.dir.peek(sys, name)?;
            let exists = !matches!(peek, Peek::Missing);
            let is_dir = matches!(peek, Peek::Dir { .. });
            if !known {
                let al = if exists {
                    alias(&level.names, name)
                } else {
                    None
                };
                let n = node(peek, &level.dir, name, want_hash)?;
                // On a case-folding filesystem a lookup of the wrong case succeeds, so the
                // listing is what tells the two spellings apart. If the parent was too large
                // to list, the case question stays open and the entry is unknown.
                let n = if exists && level.names.is_none() && !matches!(n, Node::Unknown { .. }) {
                    Node::Unknown {
                        reason: "parent directory is too large to check for case aliases".into(),
                    }
                } else {
                    n
                };
                out.seen.insert(path.clone(), Seen { node: n, alias: al });
            }
            if !is_dir {
                break;
            }
            let Some(next) = level.dir.peek_dir(name)? else {
                break;
            };
            if last && req.list && !out.listings.contains_key(&path) {
                out.listings.insert(path.clone(), list(&next));
            }
            level = Level {
                dir: next,
                names: None,
            };
        }
    }
    Ok(out)
}

/// Adds the facts of `more` to `into`. A fact observed twice keeps the first one.
pub fn merge(into: &mut Observations, more: Observations) {
    for (k, v) in more.seen {
        into.seen.entry(k).or_insert(v);
    }
    for (k, v) in more.listings {
        into.listings.entry(k).or_insert(v);
    }
}
