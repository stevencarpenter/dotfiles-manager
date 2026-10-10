//! RFC 6901 JSON Pointers into a resource `value` tree (docs/architecture.md section 6,
//! DESIGN.md section 5). Only tables are addressable: arrays are atomic, so a token never
//! indexes an array. The root pointer, wildcards and empty tokens are refused.

use toml::Value;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PointerFault {
    NotAPointer,
    Root,
    EmptyToken,
    Wildcard,
    BadEscape,
}

impl PointerFault {
    pub(crate) fn describe(&self) -> &'static str {
        match self {
            PointerFault::NotAPointer => "a pointer starts with /",
            PointerFault::Root => "the root pointer is not allowed",
            PointerFault::EmptyToken => "empty path segments are not allowed",
            PointerFault::Wildcard => "wildcards are not allowed",
            PointerFault::BadEscape => "~ must be followed by 0 or 1",
        }
    }
}

pub(crate) fn parse(path: &str) -> Result<Vec<String>, PointerFault> {
    if path.is_empty() {
        return Err(PointerFault::Root);
    }
    let rest = path.strip_prefix('/').ok_or(PointerFault::NotAPointer)?;
    rest.split('/')
        .map(|raw| {
            if raw.is_empty() {
                return Err(PointerFault::EmptyToken);
            }
            if raw == "*" {
                return Err(PointerFault::Wildcard);
            }
            let mut out = String::new();
            let mut chars = raw.chars();
            while let Some(c) = chars.next() {
                if c == '~' {
                    match chars.next() {
                        Some('0') => out.push('~'),
                        Some('1') => out.push('/'),
                        _ => return Err(PointerFault::BadEscape),
                    }
                } else {
                    out.push(c);
                }
            }
            Ok(out)
        })
        .collect()
}

pub(crate) fn escape(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

pub(crate) fn join(base: &str, token: &str) -> String {
    format!("{base}/{}", escape(token))
}

pub(crate) fn get<'a>(value: &'a Value, tokens: &[String]) -> Option<&'a Value> {
    tokens.iter().try_fold(value, |v, t| match v {
        Value::Table(t_map) => t_map.get(t),
        _ => None,
    })
}

/// Replaces the value at `tokens`, which must exist. Returns false if it does not.
pub(crate) fn replace(value: &mut Value, tokens: &[String], new: Value) -> bool {
    let Some((last, parents)) = tokens.split_last() else {
        return false;
    };
    let mut cur = value;
    for t in parents {
        match cur {
            Value::Table(m) => match m.get_mut(t) {
                Some(next) => cur = next,
                None => return false,
            },
            _ => return false,
        }
    }
    match cur {
        Value::Table(m) if m.contains_key(last) => {
            m.insert(last.clone(), new);
            true
        }
        _ => false,
    }
}

pub(crate) fn get_mut<'a>(value: &'a mut Value, tokens: &[String]) -> Option<&'a mut Value> {
    let mut cur = value;
    for t in tokens {
        match cur {
            Value::Table(m) => cur = m.get_mut(t)?,
            _ => return None,
        }
    }
    Some(cur)
}

/// True if one pointer equals the other or is a path-prefix of it.
pub(crate) fn overlaps(a: &str, b: &str) -> bool {
    a == b || b.starts_with(&format!("{a}/")) || a.starts_with(&format!("{b}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_refuses() {
        assert_eq!(parse("/a/b~1c~0").unwrap(), ["a", "b/c~"]);
        assert_eq!(parse(""), Err(PointerFault::Root));
        assert_eq!(parse("a"), Err(PointerFault::NotAPointer));
        assert_eq!(parse("/a//b"), Err(PointerFault::EmptyToken));
        assert_eq!(parse("/a/*"), Err(PointerFault::Wildcard));
        assert_eq!(parse("/a~2"), Err(PointerFault::BadEscape));
    }

    #[test]
    fn arrays_are_not_addressable() {
        let v: Value = toml::from_str("a = [1, 2]\n[t]\nx = 1").unwrap();
        assert!(get(&v, &parse("/t/x").unwrap()).is_some());
        assert!(get(&v, &parse("/a/0").unwrap()).is_none());
    }

    #[test]
    fn overlap_respects_segment_boundaries() {
        assert!(overlaps("/a", "/a/b"));
        assert!(overlaps("/a/b", "/a"));
        assert!(!overlaps("/a", "/ab"));
    }
}
