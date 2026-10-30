//! `dfm adopt`: the secret scan and the checkout checks (docs/adopt.md).
//!
//! The scan runs `gitleaks dir` over a manager-owned copy of the candidates, never over the
//! candidate directory. gitleaks reads `.gitleaks.toml` from the scanned directory, and a planted
//! one could allowlist everything; the copy holds only files named `0`, `1`, ... so no such file
//! can exist in it. An empty `--config` file is no fix: it replaces the default rules with none,
//! so the scan passes no `--config` and the runner clears `GITLEAKS_CONFIG`. The ignore file is
//! read from a named empty directory instead of the working directory.

use crate::sync_cmd::Git;
use dfm_engine::{End, ProcRequest, Runner, now_ns};
use dfm_model::{Code, Refusal};
use std::collections::BTreeSet;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const SCAN_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_REPORT: usize = 16 << 20;
/// gitleaks' own "leaks found" status, set with `--exit-code`: no other end can mean a finding.
const LEAKS: i32 = 3;

fn failed(detail: impl Into<String>) -> Refusal {
    Refusal::new(Code::ProbeFailed, detail)
}

fn io_failed(what: &str, e: &std::io::Error) -> Refusal {
    Refusal::new(Code::IoFailed, format!("{what}: {}", e.kind()))
}

fn text(p: &Path) -> Result<&str, Refusal> {
    p.to_str().ok_or_else(|| {
        crate::usage(format!(
            "{} is not a UTF-8 path; adopt passes paths to its tools as text",
            p.display()
        ))
    })
}

/// Removes the scan directory on every exit, an error and a stop included. A failure to remove
/// is dropped: the scan result decides the run. A SIGKILL leaves the directory behind; it stays
/// 0700 with 0600 files under the manager's cache directory.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Scans `files` (each an original path and its bytes) with the default gitleaks rules and
/// returns the original paths of the files with a finding, sorted. Neither the secret nor the
/// report reaches the result: `--redact` is set and only the file names are read.
/// `scan_dir` is the parent directory (`<cache dir>/adopt-scan`); the scan creates a fresh
/// `<pid>-<ns>` directory inside it. A missing `gitleaks`, a failed scan and a report that
/// cannot be mapped to the candidates are `E_PROBE_FAILED`.
#[cfg_attr(not(test), expect(dead_code, reason = "the adopt command calls this"))]
pub(crate) fn scan(
    runner: &Runner,
    scan_dir: &Path,
    files: &[(String, Vec<u8>)],
) -> Result<Vec<String>, Refusal> {
    let private = |p: &Path, recursive: bool| {
        DirBuilder::new()
            .recursive(recursive)
            .mode(0o700)
            .create(p)
            .map_err(|e| io_failed(&format!("create {}", p.display()), &e))
    };
    private(scan_dir, true)?;
    let parent = fs::symlink_metadata(scan_dir)
        .map_err(|e| io_failed(&format!("read {}", scan_dir.display()), &e))?;
    if !parent.is_dir() || parent.uid() != runner.uid {
        return Err(failed(format!(
            "{} is not a directory of yours",
            scan_dir.display()
        )));
    }
    if parent.mode() & 0o7777 != 0o700 {
        fs::set_permissions(scan_dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| io_failed(&format!("protect {}", scan_dir.display()), &e))?;
    }
    let run = scan_dir.join(format!("{}-{}", std::process::id(), now_ns()?));
    private(&run, false)?;
    let _cleanup = Cleanup(run.clone());
    let (copy, ignore) = (run.join("files"), run.join("ignore"));
    private(&copy, false)?;
    private(&ignore, false)?;
    // Two copies of each candidate in `files/<i>/`. The numbered copy carries no file name, so
    // the default path allowlist (`*.svg`, lock files, a file named `.gitleaks.toml`) cannot
    // skip it. The copy with the original base name keeps the rules that match on a name or
    // extension (`cert.p12`). A finding in either maps back by the number.
    for (i, (original, bytes)) in files.iter().enumerate() {
        let dir = copy.join(i.to_string());
        private(&dir, false)?;
        let base = Path::new(original).file_name().unwrap_or("file".as_ref());
        let mut names = vec![PathBuf::from(i.to_string())];
        if base != i.to_string().as_str() {
            names.push(PathBuf::from(base));
        }
        for name in names {
            let path = dir.join(name);
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .and_then(|mut f| f.write_all(bytes))
                .map_err(|e| io_failed(&format!("copy a candidate to {}", path.display()), &e))?;
        }
    }
    let args: Vec<String> = [
        "dir",
        text(&copy)?,
        "--no-banner",
        "--redact",
        "--ignore-gitleaks-allow",
        "--exit-code",
        "3",
        "--gitleaks-ignore-path",
        text(&ignore)?,
        "--report-format",
        "json",
        "--report-path",
        "-",
    ]
    .map(String::from)
    .into();
    let out = runner.run(&ProcRequest {
        program: "gitleaks",
        args: &args,
        env: &[],
        stdin: None,
        timeout: SCAN_TIMEOUT,
        max_stdout: MAX_REPORT,
    })?;
    match out.end {
        End::Exited(0) => Ok(Vec::new()),
        End::Exited(LEAKS) => {
            let names: Vec<String> = files.iter().map(|(p, _)| p.clone()).collect();
            findings(&out.stdout, &copy, &names)
        }
        End::Exited(n) => Err(failed(format!("gitleaks exit {n}"))),
        End::Signaled => Err(failed("gitleaks was killed")),
        End::TimedOut => Err(failed("gitleaks timed out")),
        End::Stopped(r) => Err(r),
    }
}

/// The candidate paths named by a gitleaks JSON report of the copy in `dir`. A report that is
/// not a non-empty list of findings, or names a file that is not one of the copies, is refused:
/// exit 3 means a finding, so the scan must not read as clean.
fn findings(report: &[u8], dir: &Path, names: &[String]) -> Result<Vec<String>, Refusal> {
    let unreadable = || failed("gitleaks reported a finding that adopt cannot map to a candidate");
    let value: serde_json::Value = serde_json::from_slice(report).map_err(|_| unreadable())?;
    let list = value
        .as_array()
        .filter(|l| !l.is_empty())
        .ok_or_else(unreadable)?;
    let mut hit = BTreeSet::new();
    for item in list {
        let file = item
            .get("File")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(unreadable)?;
        let name = Path::new(file)
            .strip_prefix(dir)
            .ok()
            .and_then(|p| p.components().next())
            .and_then(|c| c.as_os_str().to_str())
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|i| names.get(i))
            .ok_or_else(unreadable)?;
        hit.insert(name.clone());
    }
    Ok(hit.into_iter().collect())
}

/// The paths of `rels` (relative to `root`) that have uncommitted changes: modified, staged,
/// deleted or untracked. `root` that is no repository is `E_PROBE_FAILED`.
#[cfg_attr(not(test), expect(dead_code, reason = "the adopt command calls this"))]
pub(crate) fn dirty(git: &Git, root: &Path, rels: &[&str]) -> Result<Vec<String>, Refusal> {
    if rels.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["-C", text(root)?, "status", "--porcelain=v1", "-z", "--"];
    args.extend(rels);
    let out = git.run(true, Code::ProbeFailed, "git status", &args)?;
    let mut entries = out.split('\0');
    let mut paths = Vec::new();
    while let Some(entry) = entries.next() {
        let (Some(status), Some(path)) = (entry.get(..2), entry.get(3..)) else {
            continue;
        };
        paths.push(path.to_string());
        // A rename or copy entry is followed by its source path as a separate field.
        if status.bytes().any(|b| matches!(b, b'R' | b'C')) {
            entries.next();
        }
    }
    // git lists tracked changes before untracked paths; a sorted list is stable for the report.
    paths.sort();
    Ok(paths)
}

/// Whether `rel` under `root` belongs to the same git checkout as `root`, tracked or not: a path
/// that leaves the root, one in a directory that does not exist, one in a nested repository
/// (another top level), one that git ignores, and a `root` that is no repository are outside.
#[cfg_attr(not(test), expect(dead_code, reason = "the adopt command calls this"))]
pub(crate) fn inside_git_tree(git: &Git, root: &Path, rel: &str) -> Result<bool, Refusal> {
    let rel_path = Path::new(rel);
    if rel.is_empty()
        || rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Ok(false);
    }
    // `Some(code)` when git ended normally; a stop is an error, a signal or timeout is a failure.
    let exit = |args: &[&str]| -> Result<(i32, String), Refusal> {
        let out = git.output(true, args)?;
        match out.end {
            End::Exited(n) => Ok((
                n,
                String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
            )),
            End::Stopped(r) => Err(r),
            End::Signaled => Err(failed("git was killed")),
            End::TimedOut => Err(failed("git timed out")),
        }
    };
    let top = |dir: &Path| -> Result<Option<String>, Refusal> {
        let (code, top) = exit(&["-C", text(dir)?, "rev-parse", "--show-toplevel"])?;
        Ok((code == 0).then_some(top))
    };
    let Some(root_top) = top(root)? else {
        return Ok(false);
    };
    // A link at the path may point outside the tree, whatever git tracks.
    let full = root.join(rel_path);
    if fs::symlink_metadata(&full).is_ok_and(|m| m.file_type().is_symlink()) {
        return Ok(false);
    }
    let Some(parent) = full.parent() else {
        return Ok(false);
    };
    if top(parent)?.as_deref() != Some(root_top.as_str()) {
        return Ok(false);
    }
    // `check-ignore -q`: 0 ignored, 1 not ignored; a tracked path is never reported as ignored.
    let (code, _) = exit(&["-C", text(root)?, "check-ignore", "-q", "--", rel])?;
    Ok(code == 1)
}

// The fake `gitleaks` of the integration tests, shared with the unit tests below.
#[cfg(test)]
#[path = "../tests/common/providers.rs"]
#[expect(dead_code, reason = "the unit tests use only the gitleaks look-alike")]
mod providers;

#[cfg(test)]
mod tests {
    use super::providers::FAKE_GITLEAKS;
    use super::*;
    use crate::Ctx;
    use dfm_engine::Sys;
    use dfm_platform_macos::Platform;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    fn scratch(label: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "dfm-adopt-{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn uid() -> u32 {
        dfm_platform_macos::current_uid().unwrap()
    }

    fn runner(search: Vec<PathBuf>, base: &Path) -> Runner {
        Runner {
            search_path: search,
            home: base.join("home"),
            cwd: base.join("run"),
            uid: uid(),
            grace: Duration::from_millis(200),
            detach: false,
        }
    }

    /// A fixture with `bin/gitleaks` as the look-alike; `log` and markers sit beside `bin`.
    fn fake(base: &Path) -> Runner {
        fs::create_dir_all(base.join("bin")).unwrap();
        let exe = base.join("bin/gitleaks");
        fs::write(&exe, FAKE_GITLEAKS).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        runner(vec![base.join("bin")], base)
    }

    fn files(items: &[(&str, &str)]) -> Vec<(String, Vec<u8>)> {
        items
            .iter()
            .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec()))
            .collect()
    }

    fn leftovers(parent: &Path) -> usize {
        fs::read_dir(parent).map_or(0, |d| d.count())
    }

    #[test]
    fn scan_maps_findings_back_to_candidate_paths() {
        let base = scratch("scan");
        let r = fake(&base);
        let parent = base.join("adopt-scan");
        let input = files(&[
            ("/h/.config/a", "nothing here"),
            ("/h/.config/b", "x FAKE_SECRET y"),
            ("/h/c", "FAKE_SECRET"),
        ]);
        let hits = scan(&r, &parent, &input).unwrap();
        assert_eq!(hits, ["/h/.config/b", "/h/c"]);
        assert_eq!(
            scan(&r, &parent, &input[..1]).unwrap(),
            Vec::<String>::new()
        );
        let log = fs::read_to_string(base.join("log")).unwrap();
        assert!(log.contains("--redact"), "{log}");
        assert!(log.contains("--report-path -"), "{log}");
        assert!(!log.contains("--config"), "{log}");
        assert!(log.contains("--gitleaks-ignore-path"), "{log}");
        // The scanned directory was private while the scan ran, and nothing is left after.
        assert!(log.lines().any(|l| l == "drwx------"), "{log}");
        assert_eq!(leftovers(&parent), 0);
    }

    #[test]
    fn scan_refuses_when_gitleaks_is_missing_or_fails() {
        let base = scratch("fail");
        let parent = base.join("adopt-scan");
        let input = files(&[("/h/a", "x")]);
        // Missing: the search path holds no gitleaks.
        fs::create_dir_all(base.join("empty")).unwrap();
        let missing = runner(vec![base.join("empty")], &base);
        let e = scan(&missing, &parent, &input).unwrap_err();
        assert_eq!(e.code, Code::ProbeFailed);
        assert_eq!(leftovers(&parent), 0);
        // Failed: exit 1. The tool's stderr does not reach the refusal.
        let r = fake(&base);
        fs::write(base.join("gitleaks-fail"), "").unwrap();
        let e = scan(&r, &parent, &input).unwrap_err();
        assert_eq!(e.code, Code::ProbeFailed);
        assert!(!e.detail.contains("PLANTED-STDERR"), "{}", e.detail);
        assert_eq!(leftovers(&parent), 0);
    }

    #[test]
    fn a_report_that_cannot_be_mapped_is_a_failed_scan() {
        let names = ["/h/a".to_string()];
        let ok = |j: &str| findings(j.as_bytes(), Path::new("/s/files"), &names);
        assert_eq!(ok(r#"[{"File":"/s/files/0/a"}]"#).unwrap(), ["/h/a"]);
        for bad in [
            "",
            "not json",
            "[]",
            r#"{"File":"/s/files/0"}"#,
            r#"[{"Other":1}]"#,
            r#"[{"File":"/elsewhere/0"}]"#,
            r#"[{"File":"/s/files/9"}]"#,
        ] {
            assert_eq!(ok(bad).unwrap_err().code, Code::ProbeFailed, "{bad:?}");
        }
    }

    #[test]
    fn a_gitleaks_config_in_the_candidate_directory_is_not_used() {
        // Local proof only: hosted CI does not install gitleaks, so this test skips there.
        let found = std::env::var_os("PATH")
            .and_then(|p| std::env::split_paths(&p).find(|d| d.join("gitleaks").is_file()));
        let Some(real) = found else {
            eprintln!("skipped: no gitleaks on PATH");
            return;
        };
        let base = scratch("real");
        let r = runner(vec![real], &base);
        let parent = base.join("adopt-scan");
        // The key is the AWS documentation example with its last character changed, so the
        // default `aws-access-token` rule matches it and a repository scanner does not flag it.
        let key = "aws_access_key_id = AKIAIOSFODNN7EXAMPLQ\n";
        let input = files(&[
            (
                "/h/.gitleaks.toml",
                "[allowlist]\nregexes = [\".*\"]\npaths = [\".*\"]\n",
            ),
            (
                "/h/.gitleaksignore",
                "0:aws-access-token:1\n1:aws-access-token:1\n",
            ),
            ("/h/.config/tool/credentials", key),
            // An inline allow comment and a path-only rule (pkcs12-file) must still report.
            (
                "/h/.config/tool/commented",
                "aws_access_key_id = AKIAIOSFODNN7EXAMPLQ # gitleaks:allow\n",
            ),
            ("/h/.config/tool/cert.p12", "not really a key store"),
            // Names the default path allowlist skips: the content must still be scanned.
            ("/h/.config/tool/x.svg", key),
            ("/h/package-lock.json", key),
            ("/h/other/.gitleaks.toml", key),
        ]);
        let hits = scan(&r, &parent, &input).unwrap();
        assert_eq!(
            hits,
            [
                "/h/.config/tool/cert.p12",
                "/h/.config/tool/commented",
                "/h/.config/tool/credentials",
                "/h/.config/tool/x.svg",
                "/h/other/.gitleaks.toml",
                "/h/package-lock.json"
            ]
        );
        assert_eq!(leftovers(&parent), 0);
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let out = Command::new("/usr/bin/git")
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn git_for(home: &Path) -> Git {
        let ctx = Ctx {
            platform: Platform::with_home(home.to_path_buf()),
            binding_path: home.join("binding.toml"),
            os: "macos".into(),
            arch: "aarch64".into(),
            isolated: None,
            color: false,
        };
        Git::new(&ctx, &Sys { uid: uid() })
    }

    /// A repository with committed `modules/a/module.toml` and `modules/b/module.toml`.
    fn repo(base: &Path) -> PathBuf {
        let root = base.join("repo");
        fs::create_dir_all(root.join("modules/a")).unwrap();
        fs::create_dir_all(root.join("modules/b")).unwrap();
        git_in(&root, &["init", "-q", "."]);
        fs::write(root.join("modules/a/module.toml"), "a\n").unwrap();
        fs::write(root.join("modules/b/module.toml"), "b\n").unwrap();
        git_in(&root, &["add", "."]);
        git_in(&root, &["commit", "-q", "-m", "init"]);
        root
    }

    #[test]
    fn dirty_names_modified_and_untracked_paths_only() {
        let base = scratch("dirty");
        let root = repo(&base);
        let git = git_for(&base.join("home"));
        let all = ["modules/a/module.toml", "modules/b/module.toml", "Brewfile"];
        assert!(dirty(&git, &root, &all).unwrap().is_empty());
        assert!(dirty(&git, &root, &[]).unwrap().is_empty());
        fs::write(root.join("modules/a/module.toml"), "changed\n").unwrap();
        fs::write(root.join("Brewfile"), "brew \"x\"\n").unwrap();
        assert_eq!(
            dirty(&git, &root, &all).unwrap(),
            ["Brewfile", "modules/a/module.toml"]
        );
        // A path outside the list is not reported.
        assert_eq!(
            dirty(&git, &root, &["modules/a/module.toml"]).unwrap(),
            ["modules/a/module.toml"]
        );
        // Not a repository: the check cannot answer.
        let plain = base.join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(
            dirty(&git, &plain, &["x"]).unwrap_err().code,
            Code::ProbeFailed
        );
    }

    #[test]
    fn a_tracked_or_untracked_path_of_the_same_checkout_is_inside_it() {
        let base = scratch("inside");
        let root = repo(&base);
        let git = git_for(&base.join("home"));
        assert!(inside_git_tree(&git, &root, "modules/a/module.toml").unwrap());
        // Untracked, in a directory of the same checkout.
        fs::create_dir_all(root.join("modules/new")).unwrap();
        assert!(inside_git_tree(&git, &root, "modules/new/module.toml").unwrap());
        // A directory that does not exist yet, a path that leaves the root, a path of a nested
        // repository, and a root that is no repository are all outside.
        assert!(!inside_git_tree(&git, &root, "nowhere/module.toml").unwrap());
        assert!(!inside_git_tree(&git, &root, "../repo/modules/a/module.toml").unwrap());
        assert!(!inside_git_tree(&git, &root, "/etc/hosts").unwrap());
        // A tracked link to a file outside the tree is not inside it.
        std::os::unix::fs::symlink("/etc/hosts", root.join("modules/a/link.toml")).unwrap();
        git_in(&root, &["add", "."]);
        git_in(&root, &["commit", "-q", "-m", "link"]);
        assert!(!inside_git_tree(&git, &root, "modules/a/link.toml").unwrap());
        let nested = root.join("vendor/inner");
        fs::create_dir_all(&nested).unwrap();
        git_in(&nested, &["init", "-q", "."]);
        fs::write(nested.join("module.toml"), "n\n").unwrap();
        assert!(!inside_git_tree(&git, &root, "vendor/inner/module.toml").unwrap());
        let plain = base.join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert!(!inside_git_tree(&git, &plain, "module.toml").unwrap());
    }
}
