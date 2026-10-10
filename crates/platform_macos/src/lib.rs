//! macOS platform provider: the only crate that names macOS paths and tools.

use dfm_model::{Code, Refusal};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Directories searched for system tools, in order. `scripts/bootstrap.sh` hard-codes the same list.
pub const SYSTEM_PATH: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];
/// `<manager data dir>` relative to HOME. `scripts/bootstrap.sh` hard-codes the same literal.
pub const DATA_DIR: &str = ".local/share/dotfiles-manager";
/// Cache root relative to HOME. `scripts/bootstrap.sh` hard-codes the same literal.
pub const CACHE_DIR: &str = ".cache/dotfiles-manager";
pub const STATE_DIR: &str = ".local/state/dotfiles-manager";
pub const CONFIG_DIR: &str = ".config/dotfiles-manager";

/// Rejects every platform except native Apple Silicon macOS, before any mutation.
pub fn check_supported(os: &str, arch: &str) -> Result<(), Refusal> {
    if os == "macos" && arch == "aarch64" {
        Ok(())
    } else {
        Err(Refusal::new(
            Code::PlatformUnsupported,
            format!("{os}/{arch} is not supported; native Apple Silicon macOS is required"),
        ))
    }
}

pub fn current_platform() -> (&'static str, &'static str) {
    (std::env::consts::OS, std::env::consts::ARCH)
}

/// `O_NOFOLLOW` on Darwin. Opening a symlink with it fails instead of following the link.
pub const O_NOFOLLOW: i32 = 0x0100;

/// The effective user id, read with `id -u`.
pub fn current_uid() -> Option<u32> {
    run(&["/usr/bin/id", "-u"])?.parse().ok()
}

/// A HOME with whitespace or control characters is refused. Paths from it reach shell scripts
/// and tool arguments, and `scripts/bootstrap.sh` applies the same rule.
pub fn valid_home(home: &str) -> bool {
    home.starts_with('/') && !home.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Paths derived from one HOME.
#[derive(Clone, Debug)]
pub struct Platform {
    home: PathBuf,
}

impl Platform {
    /// HOME from the password database of the effective user. It must equal `$HOME`.
    pub fn detect() -> Result<Platform, Refusal> {
        let mismatch = |d: String| Refusal::new(Code::HomeMismatch, d);
        let user = run(&["/usr/bin/id", "-un"])
            .ok_or_else(|| mismatch("cannot read the user name".into()))?;
        let record = run(&[
            "/usr/bin/dscl",
            ".",
            "-read",
            &format!("/Users/{user}"),
            "NFSHomeDirectory",
        ])
        .ok_or_else(|| mismatch("cannot read the password database".into()))?;
        let pw_home = record
            .strip_prefix("NFSHomeDirectory:")
            .map(str::trim)
            .filter(|h| h.starts_with('/'))
            .ok_or_else(|| mismatch("unreadable password database home".into()))?;
        if !valid_home(pw_home) {
            return Err(mismatch(format!(
                "the home directory {pw_home:?} contains whitespace or control characters; this is not supported"
            )));
        }
        match std::env::var_os("HOME") {
            Some(h) if h == pw_home => Ok(Platform {
                home: PathBuf::from(pw_home),
            }),
            _ => Err(mismatch(format!(
                "password database home {pw_home} differs from HOME"
            ))),
        }
    }

    /// Used only by the `test-isolation` build, which replaces detection with an explicit HOME.
    pub fn with_home(home: PathBuf) -> Platform {
        Platform { home }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn data_dir(&self) -> PathBuf {
        self.home.join(DATA_DIR)
    }

    pub fn toolchain_dir(&self, version: &str) -> PathBuf {
        self.data_dir().join("toolchains").join(version)
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.home.join(CACHE_DIR)
    }

    pub fn build_dir(&self) -> PathBuf {
        self.cache_dir().join("build")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.home.join(STATE_DIR)
    }

    pub fn binding_path(&self) -> PathBuf {
        self.home.join(CONFIG_DIR).join("machine.toml")
    }
}

fn run(argv: &[&str]) -> Option<String> {
    let out = Command::new(argv[0])
        .args(&argv[1..])
        .env_clear()
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_apple_silicon_macos_is_supported() {
        assert!(check_supported("macos", "aarch64").is_ok());
        for (o, a) in [
            ("macos", "x86_64"),
            ("linux", "aarch64"),
            ("windows", "x86_64"),
        ] {
            assert_eq!(
                check_supported(o, a).unwrap_err().code,
                Code::PlatformUnsupported
            );
        }
    }

    #[test]
    fn homes_with_whitespace_or_control_characters_are_refused() {
        assert!(valid_home("/Users/a"));
        for bad in ["/Users/a b", "/Users/a\tb", "/Users/a\nb", "relative", ""] {
            assert!(!valid_home(bad), "{bad:?}");
        }
    }

    #[test]
    fn paths_derive_from_home() {
        let p = Platform::with_home("/h".into());
        assert_eq!(p.build_dir(), Path::new("/h/.cache/dotfiles-manager/build"));
        assert_eq!(
            p.toolchain_dir("1.2.3"),
            Path::new("/h/.local/share/dotfiles-manager/toolchains/1.2.3")
        );
        assert_eq!(
            p.binding_path(),
            Path::new("/h/.config/dotfiles-manager/machine.toml")
        );
    }
}
