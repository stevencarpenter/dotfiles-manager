//! The machine binding document (`machine.toml`, schema 1).

use crate::{Code, Refusal};
use serde::{Deserialize, Serialize};

pub const SCHEMA: i64 = 1;
pub const MAX_BYTES: usize = 1 << 20;
pub const PLATFORM: &str = "macos";
pub const ARCH: &str = "aarch64";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    schema: i64,
    profile: String,
    machine_id: String,
    platform: String,
    arch: String,
    origins: Vec<String>,
}

/// Valid machine identity: lowercase ASCII, 1 to 64 characters, no leading separator.
pub fn valid_machine_id(id: &str) -> bool {
    let b = id.as_bytes();
    (1..=64).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(c))
}

impl Binding {
    /// Builds a binding for this platform. The profile must be a canonical absolute path.
    pub fn new(profile: &str, machine_id: &str) -> Result<Binding, Refusal> {
        if !profile.starts_with('/') || profile.chars().any(char::is_control) {
            return Err(Refusal::new(
                Code::ProfileInvalid,
                "profile must be a canonical absolute path",
            ));
        }
        if !valid_machine_id(machine_id) {
            return Err(Refusal::new(
                Code::Usage,
                "machine id must match [a-z0-9][a-z0-9._-]{0,63}",
            ));
        }
        Ok(Binding {
            schema: SCHEMA,
            profile: profile.into(),
            machine_id: machine_id.into(),
            platform: PLATFORM.into(),
            arch: ARCH.into(),
            origins: Vec::new(),
        })
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    /// True when both bindings name the same profile and machine.
    pub fn same_identity(&self, other: &Binding) -> bool {
        self.profile == other.profile && self.machine_id == other.machine_id
    }

    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("a flat struct of strings always serializes")
    }

    /// Strict parse. A schema other than 1 is refused before any other field is read.
    pub fn parse(bytes: &[u8]) -> Result<Binding, Refusal> {
        let bad = |d: String| Refusal::new(Code::BindingInvalid, d);
        if bytes.len() > MAX_BYTES {
            return Err(Refusal::new(
                Code::LimitExceeded,
                "binding file exceeds 1 MiB",
            ));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| bad("not UTF-8".into()))?;
        let table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| bad(format!("not valid TOML: {}", e.message())))?;
        match table.get("schema").and_then(toml::Value::as_integer) {
            Some(SCHEMA) => {}
            Some(n) => {
                return Err(Refusal::new(
                    Code::SchemaUnsupported,
                    format!("binding schema {n} is not supported (supported: 1)"),
                ));
            }
            None => return Err(bad("missing integer schema".into())),
        }
        let b: Binding = table
            .try_into()
            .map_err(|e: toml::de::Error| bad(e.message().to_string()))?;
        if b.platform != PLATFORM || b.arch != ARCH {
            return Err(Refusal::new(
                Code::PlatformUnsupported,
                format!("binding targets {}/{}", b.platform, b.arch),
            ));
        }
        Binding::new(&b.profile, &b.machine_id).map_err(|r| bad(r.detail))?;
        if !b.origins.is_empty() {
            return Err(bad("origins are not supported yet".into()));
        }
        Ok(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let b = Binding::new("/p/profile.toml", "m5-personal").unwrap();
        assert_eq!(Binding::parse(b.to_toml().as_bytes()).unwrap(), b);
    }

    #[test]
    fn rejects_bad_input() {
        let ok = Binding::new("/p/x.toml", "m5").unwrap().to_toml();
        let code = |s: &str| Binding::parse(s.as_bytes()).unwrap_err().code;
        assert_eq!(
            code(&ok.replace("schema = 1", "schema = 2")),
            Code::SchemaUnsupported
        );
        assert_eq!(code(&format!("{ok}extra = 1\n")), Code::BindingInvalid);
        assert_eq!(code("schema = 1\n"), Code::BindingInvalid);
        assert_eq!(code("not toml ["), Code::BindingInvalid);
        assert_eq!(
            code(&ok.replace("macos", "linux")),
            Code::PlatformUnsupported
        );
        assert_eq!(code(&ok.replace("\"m5\"", "\"M5\"")), Code::BindingInvalid);
        assert_eq!(
            Binding::parse(&vec![b'#'; MAX_BYTES + 1]).unwrap_err().code,
            Code::LimitExceeded
        );
        assert_eq!(
            Binding::new("relative", "m5").unwrap_err().code,
            Code::ProfileInvalid
        );
        assert_eq!(Binding::new("/p", "-x").unwrap_err().code, Code::Usage);
    }
}
