//! Loads fixture trees into memory and applies RFC 6902 patches to parsed documents there.
//! Nothing here writes to disk: the checked-in examples are only read.

use dfm_model::resolve::SourceSet;
use std::fs;
use std::path::Path;
use toml::Value;

pub fn load(root: &Path) -> SourceSet {
    fn go(dir: &Path, rel: &str, out: &mut SourceSet) {
        let mut names: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let name = e.file_name().into_string().unwrap();
            let path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let ty = e.file_type().unwrap();
            if ty.is_symlink() {
                let t = fs::read_link(e.path()).unwrap();
                out.insert_link(path, t.to_str().unwrap());
            } else if ty.is_dir() {
                go(&e.path(), &path, out);
            } else {
                out.insert_file(path, fs::read(e.path()).unwrap());
            }
        }
    }
    let mut set = SourceSet::new();
    go(root, "", &mut set);
    set
}

fn tokens(path: &str) -> Vec<String> {
    assert!(path.starts_with('/'), "patch path {path}");
    path[1..]
        .split('/')
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect()
}

pub fn patch(doc: &mut Value, op: &str, path: &str, value: Option<Value>) {
    let toks = tokens(path);
    let (last, parents) = toks.split_last().unwrap();
    let mut cur = doc;
    for t in parents {
        cur = match cur {
            Value::Table(m) => m.get_mut(t).unwrap_or_else(|| panic!("no {t} in {path}")),
            Value::Array(a) => a.get_mut(t.parse::<usize>().unwrap()).unwrap(),
            _ => panic!("cannot descend into {t}"),
        };
    }
    match (cur, op) {
        (Value::Table(m), "add") => {
            m.insert(last.clone(), value.unwrap());
        }
        (Value::Table(m), "replace") => {
            assert!(m.contains_key(last), "replace of missing {path}");
            m.insert(last.clone(), value.unwrap());
        }
        (Value::Table(m), "remove") => {
            m.remove(last)
                .unwrap_or_else(|| panic!("remove of missing {path}"));
        }
        (Value::Array(a), "add") if last == "-" => a.push(value.unwrap()),
        (Value::Array(a), "add") => a.insert(last.parse().unwrap(), value.unwrap()),
        (Value::Array(a), "replace") => a[last.parse::<usize>().unwrap()] = value.unwrap(),
        (Value::Array(a), "remove") => {
            a.remove(last.parse::<usize>().unwrap());
        }
        (_, op) => panic!("unsupported patch {op} {path}"),
    }
}

/// Parses `file` in the set, patches it, and stores the re-serialized document.
pub fn apply(set: &mut SourceSet, file: &str, op: &str, path: &str, value: Option<Value>) {
    let bytes = match set.get(file) {
        Some(dfm_model::resolve::Entry::File(b)) => b.clone(),
        other => panic!("patch target {file} is not a file in the set: {other:?}"),
    };
    let mut doc: Value = toml::from_str(std::str::from_utf8(&bytes).unwrap()).unwrap();
    patch(&mut doc, op, path, value);
    set.insert_file(file, toml::to_string(&doc).unwrap().into_bytes());
}

pub fn text(set: &SourceSet, file: &str) -> String {
    match set.get(file) {
        Some(dfm_model::resolve::Entry::File(b)) => String::from_utf8(b.clone()).unwrap(),
        other => panic!("{file}: {other:?}"),
    }
}

/// A TOML value from its literal text, e.g. `val("\"x\"")` or `val("{ a = 1 }")`.
pub fn val(literal: &str) -> Value {
    let mut t: toml::Table = toml::from_str(&format!("v = {literal}")).unwrap();
    t.remove("v").unwrap()
}

/// Patches a document in the set with a TOML literal value.
pub fn edit(set: &mut SourceSet, file: &str, op: &str, path: &str, literal: Option<&str>) {
    apply(set, file, op, path, literal.map(val));
}

pub fn synthetic() -> SourceSet {
    load(&crate::common::repo_root().join("tests/fixtures/resolver"))
}

pub fn examples() -> SourceSet {
    load(&crate::common::repo_root().join("examples"))
}
