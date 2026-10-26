//! Parsing and structural validation of the three authored documents (schema 1). Unknown keys
//! are refused. Nothing here reads outside the bytes it
//! is given.

use super::error::{ResolveError, category};
use super::pointer;
use crate::Code;
use std::collections::{BTreeMap, BTreeSet};
use toml::{Table, Value};

/// Limits from docs/resolver.md (S10).
pub const MAX_FILE_BYTES: usize = 1 << 20;
pub const MAX_DEPTH: usize = 32;
pub const MAX_STRING_BYTES: usize = 64 * 1024;

pub const PLATFORMS: [&str; 2] = ["macos", "linux"];

#[derive(Clone, Debug)]
pub(crate) struct OverrideDecl {
    pub resource: String,
    pub path: String,
    pub value: Value,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ContributionDecl {
    pub resource: String,
    pub path: String,
    pub key: String,
    pub value: Value,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ResourceDoc {
    pub id: String,
    pub kind: String,
    pub adapter: String,
    pub target: Option<String>,
    pub value: Value,
    pub defaults: Vec<String>,
    pub extensions: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ModuleDoc {
    pub file: String,
    pub id: String,
    pub platforms: Vec<String>,
    pub requires: Vec<String>,
    pub resources: Vec<ResourceDoc>,
    pub overrides: Vec<OverrideDecl>,
    pub contributions: Vec<ContributionDecl>,
}

#[derive(Clone, Debug)]
pub(crate) struct BundleDoc {
    pub file: String,
    pub id: String,
    pub core: Option<String>,
    pub extends: Option<String>,
    pub modules: BTreeMap<String, String>,
    pub sources: Option<super::scope::SourceScope>,
}

#[derive(Clone, Debug)]
pub(crate) struct ProfileDoc {
    pub file: String,
    pub id: String,
    pub bundle: String,
    pub platform: String,
    pub architecture: String,
    pub modules: Vec<String>,
    pub overrides: Vec<OverrideDecl>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Bundle,
    Module,
    Profile,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Bundle => "bundle",
            Kind::Module => "module",
            Kind::Profile => "profile",
        }
    }
}

fn invalid(file: &str, message: impl Into<String>, action: impl Into<String>) -> ResolveError {
    ResolveError::new(Code::ManifestInvalid, file, message, action)
}

/// Lowercase letters, digits and internal hyphens, starting with a letter (DESIGN.md section 3).
pub(crate) fn valid_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b.last().is_some_and(|c| *c != b'-')
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && !s.contains("--")
}

fn depth(v: &Value) -> usize {
    match v {
        Value::Table(t) => 1 + t.values().map(depth).max().unwrap_or(0),
        Value::Array(a) => 1 + a.iter().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

fn longest_string(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len(),
        Value::Table(t) => t
            .iter()
            .map(|(k, v)| k.len().max(longest_string(v)))
            .max()
            .unwrap_or(0),
        Value::Array(a) => a.iter().map(longest_string).max().unwrap_or(0),
        _ => 0,
    }
}

fn parse_table(file: &str, bytes: &[u8]) -> Result<Table, ResolveError> {
    let limit = |what: &str| {
        ResolveError::new(
            Code::LimitExceeded,
            file,
            format!("document exceeds the {what} limit"),
            "split the document or reduce its size",
        )
    };
    if bytes.len() > MAX_FILE_BYTES {
        return Err(limit("1 MiB size"));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid(file, "document is not UTF-8", "save the file as UTF-8"))?;
    let table: Table = text.parse().map_err(|e: toml::de::Error| {
        invalid(
            file,
            format!("not valid TOML: {}", e.message()),
            "fix the syntax error",
        )
    })?;
    let value = Value::Table(table);
    if depth(&value) > MAX_DEPTH {
        return Err(limit("nesting depth 32"));
    }
    if longest_string(&value) > MAX_STRING_BYTES {
        return Err(limit("64 KiB string"));
    }
    match value {
        Value::Table(t) => Ok(t),
        _ => unreachable!("constructed as a table above"),
    }
}

fn sniff(t: &Table) -> Option<Kind> {
    let has = |keys: &[&str]| keys.iter().any(|k| t.contains_key(*k));
    if has(&["architecture", "bundle", "platform"]) {
        Some(Kind::Profile)
    } else if has(&["platforms", "requires", "resources", "contributions"]) {
        Some(Kind::Module)
    } else if has(&["extends", "core"]) || matches!(t.get("modules"), Some(Value::Table(_))) {
        Some(Kind::Bundle)
    } else {
        None
    }
}

/// The schema version every authored document (profile, bundle, module) must declare.
pub(crate) const PROFILE_SCHEMA: i64 = 1;

fn check_schema_then_kind(file: &str, t: &Table, want: Kind) -> Result<(), ResolveError> {
    match t.get("schema") {
        Some(Value::Integer(PROFILE_SCHEMA)) => {}
        Some(Value::Integer(n)) => {
            return Err(ResolveError::new(
                Code::SchemaUnsupported,
                file,
                format!("schema {n} is not supported; this build reads schema 1"),
                "use schema 1, or update the manager if the document is newer",
            )
            .field("/schema")
            .rejected("integer"));
        }
        Some(v) => {
            return Err(
                invalid(file, "schema must be an integer", "set `schema = 1`")
                    .field("/schema")
                    .rejected(category(v)),
            );
        }
        None => return Err(invalid(file, "missing schema", "add `schema = 1`").field("/schema")),
    }
    match sniff(t) {
        Some(k) if k != want => Err(ResolveError::new(
            Code::ReferenceInvalid,
            file,
            format!(
                "expected a {} document, found a {} document",
                want.name(),
                k.name()
            ),
            format!("point the reference at a {} document", want.name()),
        )),
        _ => Ok(()),
    }
}

fn only_keys(file: &str, t: &Table, allowed: &[&str], at: &str) -> Result<(), ResolveError> {
    match t.keys().find(|k| !allowed.contains(&k.as_str())) {
        None => Ok(()),
        Some(k) => Err(ResolveError::new(
            Code::FieldUnknown,
            file,
            format!("unknown key `{k}` in {at}; schema 1 accepts known keys only"),
            "remove the key; a new key needs a new schema version",
        )
        .field(format!("{at}/{k}"))
        .rejected(category(&t[k]))),
    }
}

fn get<'a>(file: &str, t: &'a Table, key: &str, at: &str) -> Result<&'a Value, ResolveError> {
    t.get(key).ok_or_else(|| {
        invalid(
            file,
            format!("{at} is missing `{key}`"),
            format!("add `{key}`"),
        )
        .field(format!("{at}/{key}"))
    })
}

fn string(file: &str, t: &Table, key: &str, at: &str) -> Result<String, ResolveError> {
    match get(file, t, key, at)? {
        Value::String(s) => Ok(s.clone()),
        v => Err(invalid(
            file,
            format!("`{key}` in {at} must be a string"),
            "use a string value",
        )
        .field(format!("{at}/{key}"))
        .rejected(category(v))),
    }
}

fn opt_string(file: &str, t: &Table, key: &str, at: &str) -> Result<Option<String>, ResolveError> {
    if t.contains_key(key) {
        string(file, t, key, at).map(Some)
    } else {
        Ok(None)
    }
}

fn string_list(file: &str, v: &Value, field: &str) -> Result<Vec<String>, ResolveError> {
    let bad = |found: &'static str| {
        invalid(
            file,
            format!("`{field}` must be an array of strings"),
            "use an array of strings",
        )
        .field(field)
        .rejected(found)
    };
    match v {
        Value::Array(a) => a
            .iter()
            .map(|i| match i {
                Value::String(s) => Ok(s.clone()),
                other => Err(bad(category(other))),
            })
            .collect(),
        other => Err(bad(category(other))),
    }
}

fn id_list(file: &str, t: &Table, key: &str) -> Result<Vec<String>, ResolveError> {
    let ids = string_list(file, get(file, t, key, "")?, &format!("/{key}"))?;
    match ids.iter().find(|i| !valid_id(i)) {
        Some(_) => Err(invalid(
            file,
            format!("`{key}` holds an invalid module ID"),
            "IDs use lowercase letters, digits and internal hyphens, and start with a letter",
        )
        .field(format!("/{key}"))
        .rejected("string")),
        None => Ok(ids),
    }
}

fn nonblank_reason(file: &str, t: &Table, at: &str) -> Result<String, ResolveError> {
    let reason = string(file, t, "reason", at)?;
    if reason.trim().is_empty() {
        return Err(invalid(
            file,
            format!("{at} needs a nonempty reason"),
            "state why the change is needed",
        )
        .field(format!("{at}/reason"))
        .rejected("string"));
    }
    Ok(reason)
}

fn overrides(file: &str, t: &Table) -> Result<Vec<OverrideDecl>, ResolveError> {
    let Some(v) = t.get("overrides") else {
        return Ok(Vec::new());
    };
    let Value::Array(items) = v else {
        return Err(invalid(
            file,
            "`overrides` must be an array of tables",
            "use [[overrides]]",
        )
        .field("/overrides")
        .rejected(category(v)));
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let at = format!("/overrides/{i}");
            let Value::Table(o) = item else {
                return Err(
                    invalid(file, format!("{at} must be a table"), "use [[overrides]]")
                        .field(at)
                        .rejected(category(item)),
                );
            };
            only_keys(file, o, &["resource", "path", "value", "reason"], &at)?;
            Ok(OverrideDecl {
                resource: string(file, o, "resource", &at)?,
                path: string(file, o, "path", &at)?,
                value: get(file, o, "value", &at)?.clone(),
                reason: nonblank_reason(file, o, &at)?,
            })
        })
        .collect()
}

fn contributions(file: &str, t: &Table) -> Result<Vec<ContributionDecl>, ResolveError> {
    let Some(v) = t.get("contributions") else {
        return Ok(Vec::new());
    };
    let Value::Array(items) = v else {
        return Err(invalid(
            file,
            "`contributions` must be an array of tables",
            "use [[contributions]]",
        )
        .field("/contributions")
        .rejected(category(v)));
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let at = format!("/contributions/{i}");
            let Value::Table(c) = item else {
                return Err(invalid(
                    file,
                    format!("{at} must be a table"),
                    "use [[contributions]]",
                )
                .field(at)
                .rejected(category(item)));
            };
            only_keys(
                file,
                c,
                &["resource", "path", "key", "value", "reason"],
                &at,
            )?;
            Ok(ContributionDecl {
                resource: string(file, c, "resource", &at)?,
                path: string(file, c, "path", &at)?,
                key: string(file, c, "key", &at)?,
                value: get(file, c, "value", &at)?.clone(),
                reason: nonblank_reason(file, c, &at)?,
            })
        })
        .collect()
}

pub(crate) fn profile(file: &str, bytes: &[u8]) -> Result<ProfileDoc, ResolveError> {
    let t = parse_table(file, bytes)?;
    check_schema_then_kind(file, &t, Kind::Profile)?;
    only_keys(
        file,
        &t,
        &[
            "schema",
            "id",
            "bundle",
            "platform",
            "architecture",
            "modules",
            "overrides",
        ],
        "",
    )?;
    let id = string(file, &t, "id", "")?;
    if !valid_id(&id) {
        return Err(invalid(
            file,
            "profile id is invalid",
            "use lowercase letters, digits and hyphens",
        )
        .field("/id")
        .rejected("string"));
    }
    let platform = string(file, &t, "platform", "")?;
    if !PLATFORMS.contains(&platform.as_str()) {
        return Err(invalid(
            file,
            "platform must be `macos` or `linux`",
            "use a supported platform",
        )
        .field("/platform")
        .rejected("string"));
    }
    let architecture = string(file, &t, "architecture", "")?;
    if architecture.is_empty()
        || !architecture
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
    {
        return Err(invalid(
            file,
            "architecture is invalid",
            "use a name such as arm64 or x86_64",
        )
        .field("/architecture")
        .rejected("string"));
    }
    Ok(ProfileDoc {
        file: file.into(),
        id,
        bundle: string(file, &t, "bundle", "")?,
        platform,
        architecture,
        modules: id_list(file, &t, "modules")?,
        overrides: overrides(file, &t)?,
    })
}

pub(crate) fn bundle(file: &str, bytes: &[u8]) -> Result<BundleDoc, ResolveError> {
    let t = parse_table(file, bytes)?;
    check_schema_then_kind(file, &t, Kind::Bundle)?;
    only_keys(
        file,
        &t,
        &["schema", "id", "core", "extends", "modules", "sources"],
        "",
    )?;
    let id = string(file, &t, "id", "")?;
    if !valid_id(&id) {
        return Err(invalid(
            file,
            "bundle id is invalid",
            "use lowercase letters, digits and hyphens",
        )
        .field("/id")
        .rejected("string"));
    }
    let core = opt_string(file, &t, "core", "")?;
    let extends = opt_string(file, &t, "extends", "")?;
    match (&core, &extends) {
        (Some(_), Some(_)) => {
            return Err(ResolveError::new(
                Code::CoreReplacement,
                file,
                "a bundle that extends another bundle cannot declare a core",
                "remove `core`; the core comes from the root bundle",
            )
            .field("/core"));
        }
        (None, None) => {
            return Err(invalid(
                file,
                "a bundle needs `core` or `extends`",
                "add one of them",
            )
            .field("/core"));
        }
        _ => {}
    }
    let modules = match get(file, &t, "modules", "")? {
        Value::Table(m) => m
            .iter()
            .map(|(k, v)| match v {
                Value::String(s) => Ok((k.clone(), s.clone())),
                other => Err(
                    invalid(file, "catalogue entries must be paths", "use a string path")
                        .field(pointer::join("/modules", k))
                        .rejected(category(other)),
                ),
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?,
        other => {
            return Err(invalid(file, "`modules` must be a table", "use [modules]")
                .field("/modules")
                .rejected(category(other)));
        }
    };
    if let Some(k) = modules.keys().find(|k| !valid_id(k)) {
        return Err(invalid(
            file,
            "a catalogue key is not a valid module ID",
            "rename the key",
        )
        .field(pointer::join("/modules", k)));
    }
    let sources = t
        .get("sources")
        .map(|v| super::scope::SourceScope::parse(file, v))
        .transpose()?;
    Ok(BundleDoc {
        file: file.into(),
        id,
        core,
        extends,
        modules,
        sources,
    })
}

fn policy_list(
    file: &str,
    policy: &Table,
    key: &str,
    res: &str,
) -> Result<Vec<String>, ResolveError> {
    match policy.get(key) {
        None => Ok(Vec::new()),
        Some(v) => string_list(file, v, &format!("/resources/{res}/policy/{key}")),
    }
}

fn resource(file: &str, item: &Value, i: usize) -> Result<ResourceDoc, ResolveError> {
    let at = format!("/resources/{i}");
    let Value::Table(r) = item else {
        return Err(
            invalid(file, format!("{at} must be a table"), "use [[resources]]")
                .field(at)
                .rejected(category(item)),
        );
    };
    only_keys(
        file,
        r,
        &["id", "kind", "adapter", "target", "value", "policy"],
        &at,
    )?;
    let id = string(file, r, "id", &at)?;
    if !valid_id(&id) {
        return Err(invalid(
            file,
            "resource id is invalid",
            "use lowercase letters, digits and hyphens",
        )
        .field(format!("{at}/id"))
        .rejected("string"));
    }
    let value = get(file, r, "value", &at)?.clone();
    if !matches!(value, Value::Table(_)) {
        return Err(invalid(
            file,
            "resource value must be a table",
            "use a [resources.value] table",
        )
        .resource(id)
        .field(format!("{at}/value"))
        .rejected(category(&value)));
    }
    let (mut defaults, mut extensions) = (Vec::new(), Vec::new());
    if let Some(p) = r.get("policy") {
        let Value::Table(policy) = p else {
            return Err(
                invalid(file, "policy must be a table", "use [resources.policy]")
                    .resource(id)
                    .field(format!("{at}/policy"))
                    .rejected(category(p)),
            );
        };
        only_keys(
            file,
            policy,
            &["defaults", "extensions"],
            &format!("{at}/policy"),
        )?;
        defaults = policy_list(file, policy, "defaults", &i.to_string())?;
        extensions = policy_list(file, policy, "extensions", &i.to_string())?;
    }
    let doc = ResourceDoc {
        kind: string(file, r, "kind", &at)?,
        adapter: string(file, r, "adapter", &at)?,
        target: opt_string(file, r, "target", &at)?,
        id,
        value,
        defaults,
        extensions,
    };
    check_policy(file, &doc)?;
    Ok(doc)
}

/// Policy paths are RFC 6901 pointers that exist in `value`, never overlap, and never address
/// an array element. Extension paths must be maps. Pointers are compared as token lists in
/// sorted order, where every descendant follows its ancestor directly, so one pass over
/// neighbours finds every duplicate or overlap.
fn check_policy(file: &str, r: &ResourceDoc) -> Result<(), ResolveError> {
    let bad = |path: &str, why: &str| {
        invalid(
            file,
            format!("policy path is invalid: {why}"),
            "list an existing, non-overlapping pointer into the resource value",
        )
        .resource(r.id.clone())
        .field(path)
    };
    let extensions: BTreeSet<&String> = r.extensions.iter().collect();
    let mut parsed: Vec<(Vec<String>, &String)> =
        Vec::with_capacity(r.defaults.len() + r.extensions.len());
    for path in r.defaults.iter().chain(&r.extensions) {
        let tokens = pointer::parse(path).map_err(|f| bad(path, f.describe()))?;
        let Some(found) = pointer::get(&r.value, &tokens) else {
            return Err(bad(path, "it does not exist in the value"));
        };
        if extensions.contains(path) && !matches!(found, Value::Table(_)) {
            return Err(bad(path, "an extension path must be a map"));
        }
        parsed.push((tokens, path));
    }
    parsed.sort();
    for pair in parsed.windows(2) {
        if pair[1].0.starts_with(&pair[0].0) {
            return Err(bad(
                pair[1].1,
                "it duplicates or overlaps another policy path",
            ));
        }
    }
    Ok(())
}

pub(crate) fn module(file: &str, bytes: &[u8]) -> Result<ModuleDoc, ResolveError> {
    let t = parse_table(file, bytes)?;
    check_schema_then_kind(file, &t, Kind::Module)?;
    only_keys(
        file,
        &t,
        &[
            "schema",
            "id",
            "platforms",
            "requires",
            "resources",
            "overrides",
            "contributions",
        ],
        "",
    )?;
    let id = string(file, &t, "id", "")?;
    if !valid_id(&id) {
        return Err(invalid(
            file,
            "module id is invalid",
            "use lowercase letters, digits and hyphens",
        )
        .field("/id")
        .rejected("string"));
    }
    let platforms = string_list(file, get(file, &t, "platforms", "")?, "/platforms")?;
    let mut sorted = platforms.clone();
    sorted.sort();
    sorted.dedup();
    if platforms.is_empty()
        || sorted.len() != platforms.len()
        || platforms.iter().any(|p| !PLATFORMS.contains(&p.as_str()))
    {
        return Err(invalid(
            file,
            "platforms must be a nonempty set of `macos` or `linux`",
            "list each supported platform once",
        )
        .field("/platforms")
        .rejected("array"));
    }
    let mut resources = Vec::new();
    match t.get("resources") {
        None => {}
        Some(Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                let r = resource(file, item, i)?;
                if resources.iter().any(|o: &ResourceDoc| o.id == r.id) {
                    return Err(invalid(
                        file,
                        "duplicate resource ID in one module",
                        "rename one resource",
                    )
                    .resource(format!("{id}.{}", r.id))
                    .field(format!("/resources/{i}/id")));
                }
                resources.push(r);
            }
        }
        Some(v) => {
            return Err(invalid(
                file,
                "`resources` must be an array of tables",
                "use [[resources]]",
            )
            .field("/resources")
            .rejected(category(v)));
        }
    }
    Ok(ModuleDoc {
        file: file.into(),
        requires: id_list(file, &t, "requires")?,
        overrides: overrides(file, &t)?,
        contributions: contributions(file, &t)?,
        id,
        platforms,
        resources,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module_err(src: &str) -> ResolveError {
        module("m/module.toml", src.as_bytes()).unwrap_err()
    }

    const HEAD: &str = "schema = 1\nid = \"m\"\nplatforms = [\"macos\"]\nrequires = []\n";

    #[test]
    fn ids() {
        for ok in ["a", "core", "personal-dev", "a1-b2"] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in ["", "A", "1a", "a-", "a--b", "a.b", "a_b"] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn schema_is_checked_before_any_other_key() {
        let e = module_err("schema = 2\nbogus = 1\n");
        assert_eq!(e.code, Code::SchemaUnsupported);
        let e = module_err("schema = true\n");
        assert_eq!(e.code, Code::ManifestInvalid);
    }

    #[test]
    fn unknown_keys_are_refused_with_location_and_category() {
        let e = module_err(&format!("{HEAD}bogus = \"x\"\n"));
        assert_eq!(e.code, Code::FieldUnknown);
        assert_eq!(e.field.as_deref(), Some("/bogus"));
        assert_eq!(e.rejected, Some("string"));
        assert_eq!(e.source, "m/module.toml");
        assert!(!e.to_string().contains("\"x\""));
    }

    #[test]
    fn a_document_of_another_kind_is_a_wrong_kind_reference() {
        let e = module_err("schema = 1\nid = \"b\"\ncore = \"x\"\n[modules]\n");
        assert_eq!(e.code, Code::ReferenceInvalid);
    }

    #[test]
    fn policy_must_name_existing_disjoint_pointers_and_map_extensions() {
        let res = |policy: &str| {
            format!(
                "{HEAD}[[resources]]\nid = \"r\"\nkind = \"render\"\nadapter = \"a/v1\"\n\
                 [resources.value.s]\nx = 1\n[resources.value.m]\n[resources.policy]\n{policy}\n"
            )
        };
        assert!(
            module(
                "f",
                res("defaults = [\"/s/x\"]\nextensions = [\"/m\"]").as_bytes()
            )
            .is_ok()
        );
        for bad in [
            "defaults = [\"/s/y\"]",
            "defaults = [\"/s\", \"/s/x\"]",
            "defaults = [\"/s/x\", \"/s/x\"]",
            "defaults = [\"\"]",
            "defaults = [\"/s/*\"]",
            "extensions = [\"/s/x\"]",
        ] {
            let e = module("f", res(bad).as_bytes()).unwrap_err();
            assert_eq!(e.code, Code::ManifestInvalid, "{bad}");
        }
    }

    #[test]
    fn oversized_deep_and_long_inputs_hit_the_limits() {
        let big = vec![b'#'; MAX_FILE_BYTES + 1];
        assert_eq!(module("f", &big).unwrap_err().code, Code::LimitExceeded);
        let long = format!("{HEAD}x = \"{}\"\n", "a".repeat(MAX_STRING_BYTES + 1));
        assert_eq!(
            module("f", long.as_bytes()).unwrap_err().code,
            Code::LimitExceeded
        );
        let nested = format!("{HEAD}x = {}1{}\n", "[".repeat(40), "]".repeat(40));
        assert_eq!(
            module("f", nested.as_bytes()).unwrap_err().code,
            Code::LimitExceeded
        );
    }
}
