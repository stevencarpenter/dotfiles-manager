#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// A scratch directory removed on drop. Canonical, so ancestors match what the kernel reports.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(label: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "dfm-{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Recursive copy that keeps symlinks as symlinks and file modes.
pub fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let ty = e.file_type().unwrap();
        if ty.is_symlink() {
            symlink(fs::read_link(&src).unwrap(), &dst).unwrap();
        } else if ty.is_dir() {
            copy_tree(&src, &dst);
        } else {
            fs::copy(&src, &dst).unwrap();
        }
    }
}

pub const BIN: &str = env!("CARGO_BIN_EXE_dotfiles-manager");

pub fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

pub fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

pub fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|o| text(&o.stdout).trim() == "0")
}

/// Names, kinds, modes, modification times and bytes of everything below `root`, links unread.
/// The lock file is included.
pub fn fingerprint(root: &Path) -> String {
    fn go(dir: &Path, h: &mut Sha256) {
        let mut v: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        v.sort_by_key(|e| e.file_name());
        for e in v {
            let m = fs::symlink_metadata(e.path()).unwrap();
            h.update(e.path().to_str().unwrap().as_bytes());
            h.update(
                format!("{:?} {:o}", m.modified().unwrap(), m.permissions().mode()).as_bytes(),
            );
            if m.is_dir() {
                go(&e.path(), h);
            } else if m.is_file() {
                h.update(fs::read(e.path()).unwrap());
            } else if m.file_type().is_symlink() {
                h.update(
                    fs::read_link(e.path())
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .as_bytes(),
                );
            }
        }
    }
    let mut h = Sha256::new();
    go(root, &mut h);
    format!("{:x}", h.finalize())
}

pub const CORE: &str = r#"schema = 1
id = "core"
platforms = ["macos", "linux"]
requires = []

[[resources]]
id = "git"
kind = "link"
adapter = "fs-link/v1"
target = ".config/git/config"
[resources.value]
source = "files/gitconfig"

[[resources]]
id = "ssh"
kind = "link"
adapter = "fs-link/v1"
target = ".ssh/config"
[resources.value]
source = "files/ssh_config"
"#;

pub const EXTRA: &str = r#"schema = 1
id = "extra"
platforms = ["macos", "linux"]
requires = []

[[resources]]
id = "tmux"
kind = "link"
adapter = "fs-link/v1"
target = ".config/tmux/tmux.conf"
[resources.value]
source = "files/tmux.conf"

[[resources]]
id = "zsh"
kind = "link"
adapter = "fs-link/v1"
target = ".zshrc"
[resources.value]
source = "files/zshrc"
"#;

pub const BUNDLE: &str = r#"schema = 1
id = "t-base"
core = "modules/core/module.toml"

[modules]
extra = "modules/extra/module.toml"
"#;

pub const PROFILE: &str = "schema = 1\nid = \"t-01\"\nbundle = \"../base/bundle.toml\"\nplatform = \"macos\"\narchitecture = \"arm64\"\nmodules = [\"extra\"]\noverrides = []\n";

/// A bound machine over a synthetic link-only source tree, run through the isolated binary with
/// `--home <temp dir>` and an empty environment. Nothing is applied.
pub struct Fx {
    pub home: TempDir,
    pub _src: TempDir,
    pub root: PathBuf,
}

impl Fx {
    pub fn unbound() -> Fx {
        let src = TempDir::new("fx-src");
        let root = src.path().join("root");
        let fx = Fx {
            home: TempDir::new("fx-home"),
            _src: src,
            root,
        };
        fx.put("base/bundle.toml", BUNDLE);
        fx.put("base/modules/core/module.toml", CORE);
        fx.put("base/modules/extra/module.toml", EXTRA);
        for (m, f, c) in [
            ("core", "gitconfig", "[user]\n"),
            ("core", "ssh_config", "Host *\n"),
            ("extra", "tmux.conf", "set -g mouse on\n"),
            ("extra", "zshrc", "export A=1\n"),
        ] {
            fx.put(&format!("base/modules/{m}/files/{f}"), c);
        }
        fx.put("profiles/t-01.toml", PROFILE);
        fx
    }

    pub fn new() -> Fx {
        let fx = Fx::unbound();
        let p = fx.root.join("profiles/t-01.toml");
        let o = fx.run(&[
            "bind",
            "--profile",
            p.to_str().unwrap(),
            "--machine-id",
            "m5-test",
        ]);
        assert_eq!(code(&o), 0, "{}", text(&o.stderr));
        fx
    }

    pub fn put(&self, rel: &str, content: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    pub fn run_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        Command::new(BIN)
            .env_clear()
            .envs(env.iter().copied())
            .arg("--home")
            .arg(self.home.path())
            .args(args)
            .output()
            .unwrap()
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.run_env(&[], args)
    }

    pub fn root_arg(&self) -> &str {
        self.root.to_str().unwrap()
    }

    pub fn h(&self, rel: &str) -> PathBuf {
        self.home.path().join(rel)
    }
}
