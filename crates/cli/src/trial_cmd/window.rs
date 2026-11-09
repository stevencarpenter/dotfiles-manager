//! The observation window around a script trial: baseline before, diff after.

use crate::read_fs::read_regular_bounded;
use dfm_model::digest::sha256_hex;
use dfm_model::display::visible;
use dfm_model::state::{Observed, ScriptBinary, TrialRecord, WindowEntry, canonical};
use dfm_model::{Code, Refusal};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Children one watched directory may hold before the window refuses to record it.
pub(super) const MAX_DIR_ENTRIES: usize = 10_000;
/// Entries one observation pass may hold in total. The serialized size is bounded separately,
/// by `MAX_RECORD_BYTES`.
pub(super) const MAX_OBSERVED: usize = 15_000;
/// Serialized bytes the trial record may hold: below the store's 4 MiB write cap
/// (`dfm_model::state::MAX_WRITE_BYTES`), with headroom for the fields around the window. The
/// record with `before` may use half, so a window shrunk to nothing always fits.
pub(super) const MAX_RECORD_BYTES: usize = 3 << 20;
/// The `TrialRecord.unknown` token of a record whose window `shrink_window` cut.
pub(super) const WINDOW_TRUNCATED: &str = "window-truncated";
/// Largest file the window hashes; a bigger file is recorded without a digest.
pub(crate) const HASH_CAP: u64 = 16 * 1024 * 1024;

const ZSH_FILES: [&str; 5] = [".zshenv", ".zprofile", ".zshrc", ".zlogin", ".zlogout"];

/// One place the window watches.
pub(super) enum Watch {
    /// A single path, recorded with its content digest.
    File(PathBuf),
    /// The children of a directory, each recorded with a digest or link target.
    Dir(PathBuf),
    /// The children of a directory, recorded by name and kind only.
    Listing(PathBuf),
}

/// The watch set of the spec table, from HOME and the script environment (`ZDOTDIR` when set).
/// Order: files, then directories, then listings. `observe` keeps the first record of a path,
/// so a file that a listing also names keeps its digest.
pub(super) fn watch_set(home: &Path, env: &[(String, String)]) -> Vec<Watch> {
    let mut files: Vec<PathBuf> = [".profile", ".bash_profile", ".bash_login", ".bashrc"]
        .iter()
        .chain(&ZSH_FILES)
        .map(|n| home.join(n))
        .collect();
    let zdotdir = env.iter().find(|(k, _)| k == "ZDOTDIR").map(|(_, v)| v);
    if let Some(z) = zdotdir.map(Path::new).filter(|z| *z != home) {
        files.extend(ZSH_FILES.iter().map(|n| z.join(n)));
    }
    files.push(home.join(".config/fish/config.fish"));
    let dirs = [
        home.join(".config/fish/conf.d"),
        home.join(".local/bin"),
        home.join("bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
    ];
    let listings = [
        home.to_path_buf(),
        home.join(".config"),
        home.join(".local"),
        home.join(".local/share"),
        home.join(".local/state"),
        home.join(".cache"),
        home.join("Library/Application Support"),
        home.join("Library/LaunchAgents"),
    ];
    files
        .into_iter()
        .map(Watch::File)
        .chain(dirs.into_iter().map(Watch::Dir))
        .chain(listings.into_iter().map(Watch::Listing))
        .collect()
}

/// A shown form of `s` that keeps the record valid: control characters become visible escapes.
fn plain(s: &str) -> String {
    if s.chars().any(char::is_control) {
        visible(s)
    } else {
        s.to_string()
    }
}

/// SHA-256 of a regular file up to `HASH_CAP`; `None` when it is bigger, unreadable or not
/// a regular file. The read does not follow a link, so callers pass a resolved path.
pub(crate) fn hash(path: &Path) -> Option<String> {
    let refuse = || Refusal::new(Code::IoFailed, "unreadable");
    let bytes = read_regular_bounded(path, HASH_CAP, |_| refuse(), refuse).ok()?;
    (bytes.len() as u64 <= HASH_CAP).then(|| sha256_hex(&bytes))
}

/// One path as the window records it. `detail` adds the digest and link target.
fn entry(path: &Path, detail: bool) -> Observed {
    let shown = path.to_string_lossy();
    let name_is_odd = shown.chars().any(char::is_control);
    let other = |path: String| Observed {
        path,
        kind: "other".into(),
        sha256: None,
        target: None,
    };
    let Ok(meta) = fs::symlink_metadata(path) else {
        return other(plain(&shown));
    };
    if name_is_odd {
        return other(plain(&shown));
    }
    let ft = meta.file_type();
    let kind = if ft.is_symlink() {
        "link"
    } else if ft.is_dir() {
        "dir"
    } else if ft.is_file() {
        "file"
    } else {
        "other"
    };
    let (mut sha256, mut target) = (None, None);
    if detail {
        if ft.is_file() {
            sha256 = hash(path);
        } else if ft.is_symlink() {
            target = fs::read_link(path)
                .ok()
                .map(|t| plain(&t.to_string_lossy()));
            // The digest is of the content behind the link, so a changed target file shows.
            let regular = fs::metadata(path)
                .ok()
                .filter(|m| m.is_file() && m.len() <= HASH_CAP);
            if regular.is_some() {
                sha256 = fs::canonicalize(path).ok().and_then(|real| hash(&real));
            }
        }
    }
    Observed {
        path: shown.into_owned(),
        kind: kind.into(),
        sha256,
        target,
    }
}

fn limit(what: String) -> Refusal {
    Refusal::new(Code::LimitExceeded, what)
}

/// One pass over the watch set, sorted by path. A watched path that is absent, and a watched
/// directory that cannot be read, yield nothing: the window is evidence, not authority. More
/// than `MAX_DIR_ENTRIES` children in one directory, or `MAX_OBSERVED` entries in total, is
/// `E_LIMIT_EXCEEDED`.
pub(super) fn observe(watch: &[Watch]) -> Result<Vec<Observed>, Refusal> {
    let mut seen: BTreeMap<String, Observed> = BTreeMap::new();
    for w in watch {
        let (dir, detail) = match w {
            Watch::File(p) => {
                if fs::symlink_metadata(p).is_ok() {
                    let o = entry(p, true);
                    seen.entry(o.path.clone()).or_insert(o);
                }
                continue;
            }
            Watch::Dir(p) => (p, true),
            Watch::Listing(p) => (p, false),
        };
        let Ok(children) = fs::read_dir(dir) else {
            continue;
        };
        for (n, child) in children.enumerate() {
            if n >= MAX_DIR_ENTRIES {
                return Err(limit(format!(
                    "{} holds more than {MAX_DIR_ENTRIES} entries",
                    visible(&dir.to_string_lossy())
                )));
            }
            let Ok(child) = child else { continue };
            let o = entry(&child.path(), detail);
            seen.entry(o.path.clone()).or_insert(o);
            if seen.len() > MAX_OBSERVED {
                return Err(limit(format!(
                    "the window holds more than {MAX_OBSERVED} entries"
                )));
            }
        }
    }
    Ok(seen.into_values().collect())
}

/// What changed between two passes, sorted by path. `created`: in `after` only. `removed`: in
/// `before` only. `kind-changed`: the kinds differ. `modified`: same kind and the digest or
/// link target differs. Both inputs must be sorted by path, as `observe` returns them.
pub(super) fn diff(before: &[Observed], after: &[Observed]) -> Vec<WindowEntry> {
    let entry = |o: &Observed, change: &str, old: Option<&Observed>| WindowEntry {
        path: o.path.clone(),
        change: change.into(),
        kind: o.kind.clone(),
        sha256: o.sha256.clone(),
        target: o.target.clone(),
        before: old.cloned(),
    };
    let (mut b, mut a) = (before.iter().peekable(), after.iter().peekable());
    let mut out = Vec::new();
    loop {
        match (b.peek(), a.peek()) {
            (None, None) => break,
            (Some(old), None) => {
                out.push(entry(old, "removed", None));
                b.next();
            }
            (None, Some(new)) => {
                out.push(entry(new, "created", None));
                a.next();
            }
            (Some(old), Some(new)) => match old.path.cmp(&new.path) {
                std::cmp::Ordering::Less => {
                    out.push(entry(old, "removed", None));
                    b.next();
                }
                std::cmp::Ordering::Greater => {
                    out.push(entry(new, "created", None));
                    a.next();
                }
                std::cmp::Ordering::Equal => {
                    if old.kind != new.kind {
                        out.push(entry(new, "kind-changed", Some(old)));
                    } else if old.sha256 != new.sha256 || old.target != new.target {
                        out.push(entry(new, "modified", Some(old)));
                    }
                    b.next();
                    a.next();
                }
            },
        }
    }
    out
}

/// Refuses a record whose `before` observation already takes more than half of
/// `MAX_RECORD_BYTES`. The caller runs it before anything is written.
pub(super) fn check_before_run(record: &TrialRecord) -> Result<(), Refusal> {
    let (bytes, cap) = (canonical(record)?.len(), MAX_RECORD_BYTES / 2);
    if bytes > cap {
        return Err(limit(format!(
            "the watch set is too large to record ({bytes} bytes, cap {cap}); nothing was written"
        )));
    }
    Ok(())
}

/// What `shrink_window` removed from the window.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Shrunk {
    pub dropped_before: bool,
    pub dropped_entries: usize,
}

impl Shrunk {
    pub(super) fn any(&self) -> bool {
        self.dropped_before || self.dropped_entries > 0
    }
}

/// Shrinks the script window until the record serializes to at most `budget` bytes. First the
/// `before` of every entry goes (it repeats `script.before`), then entries from the end of the
/// path order. A record that changed gets the `WINDOW_TRUNCATED` token in `unknown`; one under
/// the budget is untouched. Only the window shrinks, so the result can still be over the budget
/// when the rest of the record is.
pub(super) fn shrink_window(record: &mut TrialRecord, budget: usize) -> Result<Shrunk, Refusal> {
    fn window(r: &mut TrialRecord) -> Option<&mut Vec<WindowEntry>> {
        r.script.as_mut().and_then(|s| s.window.as_mut())
    }
    let mut shrunk = Shrunk::default();
    let mut over = canonical(record)?.len().saturating_sub(budget);
    if over == 0 {
        return Ok(shrunk);
    }
    for e in window(record).into_iter().flatten() {
        shrunk.dropped_before |= e.before.take().is_some();
    }
    over = canonical(record)?.len().saturating_sub(budget);
    while over > 0 {
        let Some(w) = window(record).filter(|w| !w.is_empty()) else {
            break;
        };
        // An entry frees its own bytes and one separator; the record is measured again after.
        let mut freed = 0;
        while freed < over
            && let Some(e) = w.pop()
        {
            freed += canonical(&e)?.len() + 1;
            shrunk.dropped_entries += 1;
        }
        over = canonical(record)?.len().saturating_sub(budget);
    }
    if shrunk.any() && !record.unknown.iter().any(|u| u == WINDOW_TRUNCATED) {
        record.unknown.push(WINDOW_TRUNCATED.into());
    }
    Ok(shrunk)
}

/// The first executable file named `tool` on `path_var` and then on the provider path, as
/// found (not resolved). Nothing is read. Relative and empty PATH entries are skipped.
pub(crate) fn locate(home: &Path, path_var: &OsStr, tool: &str) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .chain(crate::provider_path(home))
        .filter(|d| d.is_absolute())
        .map(|d| d.join(tool))
        .find(|c| fs::metadata(c).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// The executable `locate` finds: `path` is the candidate as found, `real_path` its canonical
/// form, `sha256` the digest of the real file. `None` when no executable is found, and also
/// when the real file is over `HASH_CAP` or unreadable, because a record without a valid digest
/// cannot be saved; the caller reports "not on PATH or cannot be hashed".
pub(super) fn binary(home: &Path, path_var: &OsStr, tool: &str) -> Option<ScriptBinary> {
    let candidate = locate(home, path_var, tool)?;
    let real = fs::canonicalize(&candidate).ok()?;
    Some(ScriptBinary {
        path: candidate.to_string_lossy().into_owned(),
        real_path: real.to_string_lossy().into_owned(),
        sha256: hash(&real)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dfm_model::Code;
    use dfm_model::digest::sha256_hex;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dfm-window-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(&dir).unwrap()
    }

    fn watch_path(w: &Watch) -> &Path {
        match w {
            Watch::File(p) | Watch::Dir(p) | Watch::Listing(p) => p,
        }
    }

    /// The watch set without the system directories, so a test never reads outside its home.
    fn home_watch(home: &Path) -> Vec<Watch> {
        watch_set(home, &[])
            .into_iter()
            .filter(|w| watch_path(w).starts_with(home))
            .collect()
    }

    fn s(p: &Path) -> String {
        p.to_str().unwrap().to_string()
    }

    #[test]
    fn the_watch_set_follows_home_and_zdotdir() {
        let w = watch_set(
            Path::new("/h"),
            &[("ZDOTDIR".into(), "/h/.config/zsh".into())],
        );
        assert!(
            w.iter()
                .any(|x| matches!(x, Watch::File(p) if p == Path::new("/h/.config/zsh/.zshenv")))
        );
        assert!(
            w.iter().any(
                |x| matches!(x, Watch::Listing(p) if p == Path::new("/h/Library/LaunchAgents"))
            )
        );
        assert!(
            w.iter()
                .any(|x| matches!(x, Watch::Dir(p) if p == Path::new("/opt/homebrew/bin")))
        );
        let plain = watch_set(Path::new("/h"), &[("ZDOTDIR".into(), "/h".into())]);
        let zshenv = |ws: &[Watch]| {
            ws.iter()
                .filter(|x| matches!(x, Watch::File(p) if p.ends_with(".zshenv")))
                .count()
        };
        assert_eq!(zshenv(&plain), 1, "ZDOTDIR equal to HOME adds nothing");
    }

    #[test]
    fn observe_and_diff_report_each_change_kind() {
        let home = scratch("diff");
        let bin = home.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(home.join(".config/x")).unwrap();
        fs::write(home.join(".zshrc"), "a\n").unwrap();
        fs::write(bin.join("a"), "tool").unwrap();
        fs::set_permissions(bin.join("a"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("a", bin.join("l")).unwrap();
        let watch = home_watch(&home);
        let before = observe(&watch).unwrap();

        fs::write(home.join(".zshrc"), "a\nb\n").unwrap();
        fs::write(bin.join("b"), "new").unwrap();
        fs::remove_file(bin.join("l")).unwrap();
        fs::remove_dir_all(home.join(".config/x")).unwrap();
        fs::write(home.join(".config/x"), "now a file").unwrap();
        fs::create_dir(home.join(".config/newdir")).unwrap();
        let after = observe(&watch).unwrap();
        let d = diff(&before, &after);

        let got: Vec<(&str, &str, &str)> = d
            .iter()
            .map(|e| (e.path.as_str(), e.change.as_str(), e.kind.as_str()))
            .collect();
        let p = |rel: &str| s(&home.join(rel));
        assert_eq!(
            got,
            vec![
                (p(".config/newdir").as_str(), "created", "dir"),
                (p(".config/x").as_str(), "kind-changed", "file"),
                (p(".local/bin/b").as_str(), "created", "file"),
                (p(".local/bin/l").as_str(), "removed", "link"),
                (p(".zshrc").as_str(), "modified", "file"),
            ]
        );
        assert_eq!(d[0].before, None);
        assert_eq!(d[1].before.as_ref().unwrap().kind, "dir");
        assert_eq!(d[2].sha256.as_deref(), Some(sha256_hex(b"new").as_str()));
        assert_eq!(d[3].target.as_deref(), Some("a"));
        assert_eq!(d[3].sha256.as_deref(), Some(sha256_hex(b"tool").as_str()));
        assert_eq!(d[4].sha256.as_deref(), Some(sha256_hex(b"a\nb\n").as_str()));
        let old = d[4].before.as_ref().unwrap();
        assert_eq!(old.sha256.as_deref(), Some(sha256_hex(b"a\n").as_str()));
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_listing_change_inside_an_entry_is_not_seen() {
        let home = scratch("listing");
        fs::create_dir_all(home.join(".config/x")).unwrap();
        let watch = home_watch(&home);
        let before = observe(&watch).unwrap();
        fs::write(home.join(".config/x/inner"), "z").unwrap();
        let after = observe(&watch).unwrap();
        assert!(diff(&before, &after).is_empty());
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_name_with_a_control_character_is_recorded_as_other_and_visible() {
        let dir = scratch("control");
        fs::write(dir.join("a\nb"), "x").unwrap();
        let seen = observe(&[Watch::Dir(dir.clone())]).unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].kind, "other");
        assert!(!seen[0].path.chars().any(char::is_control), "{:?}", seen[0]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn too_many_entries_is_a_limit_error() {
        let dir = scratch("many");
        for i in 0..=MAX_DIR_ENTRIES {
            fs::File::create(dir.join(format!("f{i}"))).unwrap();
        }
        let err = observe(&[Watch::Dir(dir.clone())]).unwrap_err();
        assert_eq!(err.code, Code::LimitExceeded);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A script record whose `before` holds `n` files under `~/.local/bin` and whose window
    /// says each was modified, with its old state in `before`.
    fn record_with(n: usize) -> TrialRecord {
        let observed = |i: usize, sha: char| Observed {
            path: format!("/Users/someone/.local/bin/tool-{i:05}"),
            kind: "file".into(),
            sha256: Some(sha.to_string().repeat(64)),
            target: None,
        };
        let before: Vec<Observed> = (0..n).map(|i| observed(i, 'a')).collect();
        let window = (0..n)
            .map(|i| {
                let o = observed(i, 'b');
                WindowEntry {
                    path: o.path,
                    change: "modified".into(),
                    kind: o.kind,
                    sha256: o.sha256,
                    target: None,
                    before: Some(before[i].clone()),
                }
            })
            .collect();
        TrialRecord {
            schema: dfm_model::state::TRIAL_SCHEMA_SCRIPT,
            binding: dfm_model::state::Stamp {
                profile: "/p/x.toml".into(),
                machine_id: "m5".into(),
                binding_digest: "d".repeat(64),
            },
            trial_id: "trial-boat".into(),
            tool: "boat".into(),
            created_unix_ns: 1,
            baseline: dfm_model::state::Baseline {
                tool_preexisting: false,
                paths: vec![],
                owners: vec![],
                data: vec![],
            },
            owned_outputs: vec![],
            package: None,
            unknown: vec!["keychain".into()],
            adopted: None,
            script: Some(dfm_model::state::TrialScript {
                url: "https://boat.dev/install".into(),
                effective_url: "https://boat.dev/install".into(),
                sha256: "c".repeat(64),
                bytes: 1,
                shell: "sh".into(),
                args: vec![],
                env: vec![],
                tty: false,
                before,
                exit: Some(dfm_model::state::ScriptExit {
                    code: Some(0),
                    signal: None,
                }),
                window: Some(window),
                binary: None,
            }),
        }
    }

    fn size(r: &TrialRecord) -> usize {
        canonical(r).unwrap().len()
    }

    fn window_of(r: &TrialRecord) -> &[WindowEntry] {
        r.script.as_ref().unwrap().window.as_deref().unwrap()
    }

    #[test]
    fn a_record_under_the_budget_is_untouched() {
        let mut r = record_with(10);
        let was = r.clone();
        let shrunk = shrink_window(&mut r, MAX_RECORD_BYTES).unwrap();
        assert_eq!(shrunk, Shrunk::default());
        assert_eq!(r, was);
    }

    #[test]
    fn dropping_the_before_states_is_enough_for_9000_modified_files() {
        let mut r = record_with(9_000);
        assert!(size(&r) > MAX_RECORD_BYTES, "{}", size(&r));
        let shrunk = shrink_window(&mut r, MAX_RECORD_BYTES).unwrap();
        assert_eq!(
            shrunk,
            Shrunk {
                dropped_before: true,
                dropped_entries: 0
            }
        );
        assert!(size(&r) <= MAX_RECORD_BYTES);
        let w = window_of(&r);
        assert_eq!(w.len(), 9_000);
        assert!(w.iter().all(|e| e.before.is_none()));
        assert!(r.unknown.iter().any(|u| u == WINDOW_TRUNCATED));
        assert_eq!(
            r.script.as_ref().unwrap().exit.as_ref().unwrap().code,
            Some(0)
        );
        r.validate().unwrap();
    }

    #[test]
    fn the_window_tail_is_cut_when_the_before_states_are_not_enough() {
        let mut r = record_with(9_000);
        let budget = MAX_RECORD_BYTES / 2;
        let shrunk = shrink_window(&mut r, budget).unwrap();
        assert!(shrunk.dropped_before);
        let w = window_of(&r);
        assert!(shrunk.dropped_entries > 0);
        assert_eq!(w.len() + shrunk.dropped_entries, 9_000);
        assert!(size(&r) <= budget, "{}", size(&r));
        // The entries kept are the head of the path order.
        assert!(
            w.last()
                .unwrap()
                .path
                .ends_with(&format!("{:05}", w.len() - 1))
        );
        let once = r.unknown.iter().filter(|u| *u == WINDOW_TRUNCATED).count();
        assert_eq!(once, 1);
    }

    #[test]
    fn a_before_observation_over_half_the_budget_refuses_the_run() {
        assert!(check_before_run(&record_with(10)).is_ok());
        let err = check_before_run(&record_with(9_000)).unwrap_err();
        assert_eq!(err.code, Code::LimitExceeded);
        assert!(err.detail.contains("nothing was written"), "{}", err.detail);
    }

    #[test]
    fn binary_follows_links_and_hashes_the_real_file() {
        let dir = scratch("binary");
        let (bin, real) = (dir.join("bin"), dir.join("real"));
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("t"), "payload").unwrap();
        fs::set_permissions(real.join("t"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink(real.join("t"), bin.join("t")).unwrap();
        let path_var = std::env::join_paths([&bin]).unwrap();
        let b = binary(&dir, &path_var, "t").unwrap();
        assert_eq!(b.path, s(&bin.join("t")));
        assert_eq!(b.real_path, s(&real.join("t")));
        assert_eq!(b.sha256, sha256_hex(b"payload"));
        assert!(binary(&dir, &path_var, "missing-tool-xyz").is_none());
        fs::remove_dir_all(&dir).unwrap();
    }
}
