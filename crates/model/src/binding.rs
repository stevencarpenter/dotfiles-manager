//! The machine binding document (`machine.toml`). Schema 2 records the checkout root that
//! commands default `--root` to; schema 1 has no root and stays readable.

use crate::resolve::PROFILE_SCHEMA;
use crate::{Code, Refusal};
use serde::{Deserialize, Serialize};

pub const SCHEMA: i64 = 2;
/// A binding written before the root was recorded. Every command still reads it.
pub const SCHEMA_NO_ROOT: i64 = 1;
pub const MAX_BYTES: usize = 1 << 20;
pub const PLATFORM: &str = "macos";
pub const ARCH: &str = "aarch64";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    schema: i64,
    profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<String>,
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

/// The machine a profile declares, as written in its top-level `platform` and `architecture`
/// keys. `None` for a key that is absent or not a string; the resolver validates both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileTarget {
    pub platform: Option<String>,
    pub architecture: Option<String>,
}

/// A profile must be a bounded TOML document with a top-level `schema = 1`. The resolver
/// validates the rest; this keeps `bind` from recording an arbitrary file and returns the
/// declared machine so `bind` can refuse a profile for another one.
pub fn check_profile(bytes: &[u8]) -> Result<ProfileTarget, Refusal> {
    let bad = |d: &str| Refusal::new(Code::ProfileInvalid, d.to_string());
    if bytes.len() > MAX_BYTES {
        return Err(bad("profile exceeds 1 MiB"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| bad("profile is not UTF-8"))?;
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        bad(&format!("profile is not valid TOML: {}", e.message()))
    })?;
    match table.get("schema") {
        Some(toml::Value::Integer(PROFILE_SCHEMA)) => {}
        Some(toml::Value::Integer(n)) => {
            return Err(Refusal::new(
                Code::SchemaUnsupported,
                format!("profile schema {n} is not supported (supported: {PROFILE_SCHEMA})"),
            ));
        }
        Some(_) => return Err(bad("profile schema must be an integer")),
        None => return Err(bad("profile has no top-level schema key")),
    }
    let get = |k: &str| table.get(k).and_then(|v| v.as_str()).map(str::to_string);
    Ok(ProfileTarget {
        platform: get("platform"),
        architecture: get("architecture"),
    })
}

/// A canonical absolute path: the form `bind` records for the profile and the root.
fn absolute(path: &str) -> bool {
    path.starts_with('/') && !path.chars().any(char::is_control)
}

impl Binding {
    /// Builds a schema 1 binding (no root) for this platform. The profile must be a canonical
    /// absolute path. [`Binding::with_root`] makes it the schema 2 binding that `bind` writes.
    pub fn new(profile: &str, machine_id: &str) -> Result<Binding, Refusal> {
        if !absolute(profile) {
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
            schema: SCHEMA_NO_ROOT,
            profile: profile.into(),
            root: None,
            machine_id: machine_id.into(),
            platform: PLATFORM.into(),
            arch: ARCH.into(),
            origins: Vec::new(),
        })
    }

    /// The schema 2 binding with `root`, a canonical absolute directory that holds the profile.
    pub fn with_root(self, root: &str) -> Result<Binding, Refusal> {
        let inside = std::path::Path::new(&self.profile).strip_prefix(root);
        if !absolute(root) || !inside.is_ok_and(|rest| !rest.as_os_str().is_empty()) {
            return Err(Refusal::new(
                Code::ProfileInvalid,
                "the root must be a canonical absolute path that contains the profile",
            ));
        }
        Ok(Binding {
            schema: SCHEMA,
            root: Some(root.into()),
            ..self
        })
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// The checkout root `bind` recorded; `None` for a schema 1 binding.
    pub fn root(&self) -> Option<&str> {
        self.root.as_deref()
    }

    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    /// True when both bindings name the same profile and machine.
    pub fn same_identity(&self, other: &Binding) -> bool {
        self.profile == other.profile && self.machine_id == other.machine_id
    }

    /// SHA-256 of the canonical TOML form. Audit evidence only; identity is profile plus machine.
    pub fn digest(&self) -> String {
        crate::digest::sha256_hex(self.to_toml().as_bytes())
    }

    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("a flat struct of strings always serializes")
    }

    /// Strict parse. A schema other than 1 or 2 is refused before any other field is read.
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
        let schema = match table.get("schema").and_then(toml::Value::as_integer) {
            Some(n @ (SCHEMA_NO_ROOT | SCHEMA)) => n,
            Some(n) => {
                return Err(Refusal::new(
                    Code::SchemaUnsupported,
                    format!("binding schema {n} is not supported (supported: 1, 2)"),
                ));
            }
            None => return Err(bad("missing integer schema".into())),
        };
        let b: Binding = table
            .try_into()
            .map_err(|e: toml::de::Error| bad(e.message().to_string()))?;
        if b.platform != PLATFORM || b.arch != ARCH {
            return Err(Refusal::new(
                Code::PlatformUnsupported,
                format!("binding targets {}/{}", b.platform, b.arch),
            ));
        }
        let base = Binding::new(&b.profile, &b.machine_id).map_err(|r| bad(r.detail))?;
        match (schema, &b.root) {
            (SCHEMA_NO_ROOT, None) => {}
            (SCHEMA, Some(root)) => {
                base.with_root(root).map_err(|r| bad(r.detail))?;
            }
            (SCHEMA_NO_ROOT, Some(_)) => return Err(bad("schema 1 has no root".into())),
            _ => return Err(bad("schema 2 needs a root".into())),
        }
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
        let b = b.with_root("/p").unwrap();
        assert_eq!(b.root(), Some("/p"));
        assert!(b.to_toml().starts_with("schema = 2\n"));
        assert_eq!(Binding::parse(b.to_toml().as_bytes()).unwrap(), b);
    }

    #[test]
    fn the_root_holds_the_profile_and_matches_the_schema() {
        let b = || Binding::new("/r/dfm/p.toml", "m5").unwrap();
        for bad in ["/other", "/r/dfm/p", "/r/dfm/p.toml", "r", "/r\n"] {
            assert_eq!(b().with_root(bad).unwrap_err().code, Code::ProfileInvalid);
        }
        let one = b().to_toml();
        let two = b().with_root("/r").unwrap().to_toml();
        let code = |s: &str| Binding::parse(s.as_bytes()).unwrap_err().code;
        assert_eq!(
            code(&two.replace("schema = 2", "schema = 1")),
            Code::BindingInvalid
        );
        assert_eq!(
            code(&one.replace("schema = 1", "schema = 2")),
            Code::BindingInvalid
        );
        assert_eq!(code(&two.replace("\"/r\"", "\"/x\"")), Code::BindingInvalid);
    }

    #[test]
    fn profile_needs_a_top_level_schema_key() {
        let t =
            check_profile(b"schema = 1\nplatform = \"macos\"\narchitecture = \"arm64\"\n").unwrap();
        assert_eq!(t.platform.as_deref(), Some("macos"));
        assert_eq!(t.architecture.as_deref(), Some("arm64"));
        assert!(check_profile(b"schema = 1\nid = \"x\"\n").is_ok());
        for bad in [&b""[..], b"[t]\nschema = 1\n", b"[oops", b"\xff"] {
            assert_eq!(check_profile(bad).unwrap_err().code, Code::ProfileInvalid);
        }
    }

    #[test]
    fn the_profile_schema_check_follows_the_resolver_constant_not_the_binding_schema() {
        // The resolver accepts exactly PROFILE_SCHEMA. A bump of the binding schema alone must
        // not change which profiles bind accepts.
        let ok = format!("schema = {PROFILE_SCHEMA}\n");
        assert!(check_profile(ok.as_bytes()).is_ok());
        let next = format!("schema = {}\n", PROFILE_SCHEMA + 1);
        let e = check_profile(next.as_bytes()).unwrap_err();
        assert_eq!(e.code, Code::SchemaUnsupported);
        assert!(e.detail.contains(&format!("supported: {PROFILE_SCHEMA}")));
    }

    #[test]
    fn profile_schema_must_be_integer_one() {
        for n in [&b"schema = 2\n"[..], b"schema = 0\n", b"schema = -1\n"] {
            assert_eq!(check_profile(n).unwrap_err().code, Code::SchemaUnsupported);
        }
        for bad in [
            &b"schema = \"1\"\n"[..],
            b"schema = 1.0\n",
            b"schema = true\n",
        ] {
            assert_eq!(check_profile(bad).unwrap_err().code, Code::ProfileInvalid);
        }
    }

    #[test]
    fn rejects_bad_input() {
        let ok = Binding::new("/p/x.toml", "m5").unwrap().to_toml();
        let code = |s: &str| Binding::parse(s.as_bytes()).unwrap_err().code;
        assert_eq!(
            code(&ok.replace("schema = 1", "schema = 3")),
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
