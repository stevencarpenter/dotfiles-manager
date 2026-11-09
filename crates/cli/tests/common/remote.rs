//! Local git remotes for the test targets that sync from an origin: bare repositories in a
//! temporary directory, reached as `file://` origins, and the lock that names them.

use super::text;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Git for building fixtures: a bare environment, no user configuration, a fixed identity.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("/usr/bin/git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-C",
        ])
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?} in {}:\n{}",
        dir.display(),
        text(&o.stderr)
    );
    text(&o.stdout).trim_end().to_string()
}

pub fn put(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// A bare remote, a seed working copy that pushes to it, and the id of its head.
pub struct Remote {
    pub bare: PathBuf,
    pub seed: PathBuf,
}

impl Remote {
    pub fn new(work: &Path, name: &str) -> Remote {
        let bare = work.join(name);
        let seed = work.join(format!("{name}-seed"));
        fs::create_dir_all(&bare).unwrap();
        fs::create_dir_all(&seed).unwrap();
        git(&bare, &["init", "-q", "--bare"]);
        git(&seed, &["init", "-q"]);
        git(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
        Remote { bare, seed }
    }

    pub fn origin(&self) -> String {
        format!("file://{}", self.bare.display())
    }

    /// Commits everything in the seed copy and pushes `main`. Returns the new head.
    pub fn push(&self, message: &str) -> String {
        git(&self.seed, &["add", "-A"]);
        git(&self.seed, &["commit", "-q", "-m", message]);
        git(
            &self.seed,
            &["push", "-q", "origin", "HEAD:refs/heads/main"],
        );
        git(&self.seed, &["rev-parse", "HEAD"])
    }

    pub fn head(&self) -> String {
        git(&self.bare, &["rev-parse", "refs/heads/main"])
    }

    /// Rewrites the head commit without changing its tree and force-pushes it: a new commit
    /// identity over the same content (a new message, since the same second would reproduce
    /// the same commit).
    pub fn amend(&self) -> String {
        git(
            &self.seed,
            &[
                "commit",
                "-q",
                "--amend",
                "-m",
                "amended: same tree, new commit",
            ],
        );
        git(
            &self.seed,
            &["push", "-q", "-f", "origin", "HEAD:refs/heads/main"],
        );
        git(&self.seed, &["rev-parse", "HEAD"])
    }
}

pub fn lock_for(sources: &[(&str, Option<&str>, &str)]) -> String {
    let mut s = String::from("[manager]\ntoolchain = \"1.98.1\"\n");
    for (i, (origin, commit, path)) in sources.iter().enumerate() {
        s.push_str(&format!(
            "\n[[sources]]\nid = \"src{i}\"\nkind = \"git\"\norigin = \"{origin}\"\nref = \"refs/heads/main\"\npath = \"{path}\"\n"
        ));
        if let Some(c) = commit {
            s.push_str(&format!("commit = \"{c}\"\n"));
        }
    }
    s
}
