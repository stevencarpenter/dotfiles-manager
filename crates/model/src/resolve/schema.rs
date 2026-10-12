//! What the resolver needs to know about an adapter: its kind, whether it owns a target, how
//! its `value` tree is typed, and which outputs it claims. The registry that maps identifiers to
//! implementations is built by the CLI (docs/architecture.md section 3); `model` only defines the
//! shape, so it names no OS-bound provider.

use super::error::category;
use super::pointer;
use std::collections::BTreeMap;
use toml::Value;

#[derive(Clone, Debug)]
pub enum Schema {
    Str,
    Int,
    Bool,
    /// A path relative to the manifest that supplies the value. Contained in that manifest's root.
    Source,
    /// A HOME-relative output path.
    HomePath,
    /// Atomic: replaced as a whole, never edited by position.
    Array(Box<Schema>),
    /// A closed set of keys. Any other key is refused.
    Table(Vec<Field>),
    /// A POSIX file mode: an integer in `0..=0o777`. With `private`, group and other bits must
    /// be clear.
    FileMode {
        private: bool,
    },
    /// Open keys with one value schema (a named collection).
    Map(Box<Schema>),
    /// A native application document. Types are preserved, keys are the application's.
    Native,
}

#[derive(Clone, Debug)]
pub struct Field {
    pub name: &'static str,
    pub schema: Schema,
    pub required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetRule {
    Required,
    /// The adapter writes one destination only. A resource naming another is refused.
    Fixed(&'static str),
    Forbidden,
}

/// The output paths a resource claims, before any observation of the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claims {
    None,
    /// The resource `target`.
    Target,
    /// The resource `target`, plus a directory of fragment files read into it. The directory is
    /// claimed as a prefix. Another resource may place a file there only if its name matches
    /// the pattern, because that is how a selected module supplies a fragment.
    TargetAndFragments {
        dir: &'static str,
        pattern: &'static str,
    },
    /// One leaf per enabled entry under each root: `<root>/<entry name>`.
    Leaves {
        roots: &'static str,
        entries: &'static str,
        enabled: &'static str,
    },
}

#[derive(Clone, Debug)]
pub struct AdapterSpec {
    pub id: String,
    pub kind: &'static str,
    pub target: TargetRule,
    /// Values hold references to secrets. They are shown as `[redacted]` and never hashed.
    pub secret_values: bool,
    /// The output can hold credentials (architecture section 8, "Secrets and sensitive paths"):
    /// the planner stores no content hash for it. Implied by `secret_values`.
    pub sensitive: bool,
    /// The adapter writes into a native document with its own precedence rules.
    pub native_document: bool,
    pub claims: Claims,
    pub value: Schema,
    /// For adapters whose source file is a native document that includes other files (for
    /// example a Git config): lists those includes in the order the application reads them.
    pub source_includes: Option<fn(&str) -> Vec<Include>>,
    /// How the application orders competing values in a native document, for `explain`.
    pub precedence: Option<&'static str>,
}

/// One include directive found in an authored native document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Include {
    /// The path as written, for example `~/.config/x/base.inc`.
    pub path: String,
    /// The condition guarding it, for example `gitdir:~/work/`.
    pub condition: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct AdapterCatalog {
    specs: BTreeMap<String, AdapterSpec>,
}

impl AdapterCatalog {
    pub fn new(specs: impl IntoIterator<Item = AdapterSpec>) -> Self {
        AdapterCatalog {
            specs: specs.into_iter().map(|s| (s.id.clone(), s)).collect(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&AdapterSpec> {
        self.specs.get(id)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    UnknownKey {
        found: &'static str,
    },
    Type {
        expected: &'static str,
        found: &'static str,
    },
    MissingKey,
    EmptyKey,
    OutOfRange(&'static str),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SchemaFault {
    pub pointer: String,
    pub fault: Fault,
}

/// A `Source` leaf found while validating: its pointer and the relative path it declares.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SourceRef {
    pub pointer: String,
    pub rel: String,
}

/// A `HomePath` leaf found while validating.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HomeRef {
    pub pointer: String,
    pub path: String,
}

#[derive(Default)]
pub(crate) struct Found {
    pub sources: Vec<SourceRef>,
    pub homes: Vec<HomeRef>,
}

fn expected_name(s: &Schema) -> &'static str {
    match s {
        Schema::Str | Schema::Source | Schema::HomePath => "string",
        Schema::Int | Schema::FileMode { .. } => "integer",
        Schema::Bool => "boolean",
        Schema::Array(_) => "array",
        Schema::Table(_) | Schema::Map(_) | Schema::Native => "table",
    }
}

pub(crate) fn validate(
    value: &Value,
    schema: &Schema,
    ptr: &str,
    found: &mut Found,
) -> Result<(), SchemaFault> {
    let wrong = || SchemaFault {
        pointer: ptr.into(),
        fault: Fault::Type {
            expected: expected_name(schema),
            found: category(value),
        },
    };
    match (schema, value) {
        // An empty key has no RFC 6901 pointer (the empty token is refused), so nothing could
        // report or address its value. A native document is checked at every depth.
        (Schema::Native, Value::Table(_)) if has_empty_key(value) => Err(SchemaFault {
            pointer: ptr.into(),
            fault: Fault::EmptyKey,
        }),
        (Schema::Map(_), Value::Table(t)) if t.contains_key("") => Err(SchemaFault {
            pointer: ptr.into(),
            fault: Fault::EmptyKey,
        }),
        (Schema::Str, Value::String(_))
        | (Schema::Int, Value::Integer(_))
        | (Schema::Bool, Value::Boolean(_))
        | (Schema::Native, Value::Table(_)) => Ok(()),
        (Schema::FileMode { private }, Value::Integer(n)) => {
            let fault = |why| SchemaFault {
                pointer: ptr.into(),
                fault: Fault::OutOfRange(why),
            };
            if !(0..=0o777).contains(n) {
                Err(fault("a file mode is an integer from 0 to 0o777"))
            } else if *private && n & 0o077 != 0 {
                Err(fault("a private file mode must clear group and other bits"))
            } else {
                Ok(())
            }
        }
        (Schema::Source, Value::String(s)) => {
            found.sources.push(SourceRef {
                pointer: ptr.into(),
                rel: s.clone(),
            });
            Ok(())
        }
        (Schema::HomePath, Value::String(s)) => {
            found.homes.push(HomeRef {
                pointer: ptr.into(),
                path: s.clone(),
            });
            Ok(())
        }
        (Schema::Array(item), Value::Array(items)) => {
            for (i, v) in items.iter().enumerate() {
                validate(v, item, &format!("{ptr}/{i}"), found)?;
            }
            Ok(())
        }
        (Schema::Table(fields), Value::Table(t)) => {
            if let Some(k) = t
                .keys()
                .find(|k| !fields.iter().any(|f| f.name == k.as_str()))
            {
                return Err(SchemaFault {
                    pointer: pointer::join(ptr, k),
                    fault: Fault::UnknownKey {
                        found: category(&t[k]),
                    },
                });
            }
            for f in fields {
                match t.get(f.name) {
                    Some(v) => validate(v, &f.schema, &pointer::join(ptr, f.name), found)?,
                    None if f.required => {
                        return Err(SchemaFault {
                            pointer: pointer::join(ptr, f.name),
                            fault: Fault::MissingKey,
                        });
                    }
                    None => {}
                }
            }
            Ok(())
        }
        (Schema::Map(item), Value::Table(t)) => t
            .iter()
            .try_for_each(|(k, v)| validate(v, item, &pointer::join(ptr, k), found)),
        _ => Err(wrong()),
    }
}

/// Whether a table, or a table inside an array or table below it, has the key `""`.
fn has_empty_key(v: &Value) -> bool {
    match v {
        Value::Table(t) => t.contains_key("") || t.values().any(has_empty_key),
        Value::Array(a) => a.iter().any(has_empty_key),
        _ => false,
    }
}

/// A HOME-relative output path (DESIGN.md section 4): no absolute path, empty, `.` or `..`
/// component, NUL or other control character. A leading `~` component is refused too, because
/// nothing expands it and a literal directory named `~` is never intended.
pub(crate) fn home_path_fault(path: &str) -> Option<&'static str> {
    if path.is_empty() {
        return Some("empty path");
    }
    if path.starts_with('/') {
        return Some("absolute path");
    }
    if path.chars().any(char::is_control) {
        return Some("control character");
    }
    if path.chars().any(super::error::is_invisible) {
        return Some("invisible or bidirectional-control character");
    }
    // Printed text escapes a backslash, so an id that holds one could not be pasted back as a flag.
    if path.contains('\\') {
        return Some("backslash");
    }
    for c in path.split('/') {
        match c {
            "" => return Some("empty component"),
            "." | ".." => return Some("dot component"),
            _ if c.starts_with('~') => return Some("tilde component"),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Value {
        toml::from_str(s).unwrap()
    }

    fn link() -> Schema {
        Schema::Table(vec![Field {
            name: "source",
            schema: Schema::Source,
            required: true,
        }])
    }

    #[test]
    fn closed_tables_refuse_unknown_and_missing_keys() {
        let mut f = Found::default();
        let e = validate(&t("source = \"a\"\nextra = 1"), &link(), "", &mut f).unwrap_err();
        assert_eq!(e.pointer, "/extra");
        assert_eq!(e.fault, Fault::UnknownKey { found: "integer" });
        let e = validate(&t(""), &link(), "", &mut f).unwrap_err();
        assert_eq!(e.fault, Fault::MissingKey);
    }

    #[test]
    fn booleans_and_integers_are_distinct() {
        let s = Schema::Table(vec![Field {
            name: "n",
            schema: Schema::Int,
            required: true,
        }]);
        let e = validate(&t("n = true"), &s, "", &mut Found::default()).unwrap_err();
        assert_eq!(
            e.fault,
            Fault::Type {
                expected: "integer",
                found: "boolean"
            }
        );
    }

    #[test]
    fn arrays_are_checked_as_a_whole() {
        let s = Schema::Array(Box::new(Schema::Str));
        assert!(
            validate(
                &Value::Array(vec![Value::Integer(1)]),
                &s,
                "/a",
                &mut Found::default()
            )
            .is_err()
        );
    }

    #[test]
    fn file_modes_are_bounded_and_private_ones_clear_group_and_other() {
        let m = |private| Schema::FileMode { private };
        let ok = |n: i64, private| {
            validate(&Value::Integer(n), &m(private), "/m", &mut Found::default())
        };
        assert!(ok(0o600, true).is_ok() && ok(0o400, true).is_ok() && ok(0o644, false).is_ok());
        for (n, private) in [
            (-1, false),
            (0o1000, false),
            (0o666, true),
            (0o640, true),
            (0o601, true),
        ] {
            assert!(
                matches!(ok(n, private).unwrap_err().fault, Fault::OutOfRange(_)),
                "{n:o}"
            );
        }
        assert!(validate(&Value::Boolean(true), &m(true), "/m", &mut Found::default()).is_err());
    }

    #[test]
    fn home_paths() {
        assert_eq!(home_path_fault(".config/x"), None);
        for bad in [
            "",
            "/x",
            "a/../b",
            "a//b",
            "./a",
            "a\0b",
            "~/x",
            "a/~b",
            "a\u{202e}b",
            "a\u{200b}b",
            "\u{feff}a",
            "a\\b",
        ] {
            assert!(home_path_fault(bad).is_some(), "{bad:?}");
        }
    }
}
