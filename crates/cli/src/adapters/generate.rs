//! The `render` and `assemble` adapters (docs/adapters.md). Pure functions of the resource
//! value and the verified inputs apply or the planner supplies. They open no file.

use dfm_engine::{Dir, Generator, GeneratorInputs};
use dfm_model::digest::sha256_hex;
use dfm_model::fold;
use dfm_model::plan::{
    Action, Blocker, MAX_HASHED_BYTES, Node, Operation, Ownership, Plan, Status, matches_pattern,
};
use dfm_model::resolve::{Entry, Resolved, SourceSet, Table, Value};
use dfm_model::state::Class;
use dfm_model::{Code, Refusal};
use std::collections::BTreeMap;
use std::fmt::Write;

pub const GIT_CONFIG: &str = "git-config/v1";
pub const TEXT_FRAGMENTS: &str = "text-fragments/v1";

/// The generators of this build, reading each resource value from the resolved model of the
/// plan being applied. The value is data read at run time: a config edit needs no rebuild.
pub struct Generators<'a> {
    pub resolved: &'a Resolved,
}

impl Generator for Generators<'_> {
    fn supports(&self, op: &Operation) -> bool {
        op.class == Class::Normal
            && matches!(op.adapter.as_str(), GIT_CONFIG | TEXT_FRAGMENTS)
            && self.resolved.resources.contains_key(&op.resource)
    }

    fn generate(&self, op: &Operation, inputs: &GeneratorInputs) -> Result<Vec<u8>, Refusal> {
        let value = &self
            .resolved
            .resources
            .get(&op.resource)
            .ok_or_else(|| {
                Refusal::new(
                    Code::GeneratorUnavailable,
                    format!("{} is not in the resolved model", op.resource),
                )
            })?
            .value;
        match op.adapter.as_str() {
            GIT_CONFIG => git_config(value),
            TEXT_FRAGMENTS => text_fragments(value, op, inputs),
            other => Err(Refusal::new(
                Code::GeneratorUnavailable,
                format!("no generator for {other}"),
            )),
        }
    }
}

fn invalid(detail: impl Into<String>) -> Refusal {
    Refusal::new(Code::ManifestInvalid, detail)
}

/// A Git name (section or key): a letter, then letters, digits or `-`.
fn git_name(kind: &str, n: &str) -> Result<(), Refusal> {
    let mut chars = n.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "git-config {kind} {n:?} is not a Git name (a letter, then letters, digits or '-')"
        )))
    }
}

/// A quoted Git value or subsection. Backslash and quote are escaped. Newline and tab use
/// their escapes. Any other control character is refused, not rewritten.
fn quote(s: &str) -> Result<String, Refusal> {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                return Err(invalid(format!(
                    "git-config text holds the control character U+{:04X}",
                    u32::from(c)
                )));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    Ok(out)
}

fn scalar(v: &Value) -> Result<String, Refusal> {
    match v {
        Value::String(s) => quote(s),
        Value::Integer(i) => Ok(i.to_string()),
        Value::Boolean(b) => Ok(b.to_string()),
        other => Err(invalid(format!(
            "git-config accepts a string, integer or boolean, not a {}",
            other.type_str()
        ))),
    }
}

/// `key = value` lines. An array writes the key once per element, in array order. Git reads a
/// repeated key in file order, so the array order is the order Git sees.
fn entries(out: &mut String, table: &Table) -> Result<(), Refusal> {
    for (key, v) in table.iter().filter(|(_, v)| !v.is_table()) {
        git_name("key", key)?;
        let values: Vec<&Value> = match v {
            Value::Array(a) => a.iter().collect(),
            single => vec![single],
        };
        for item in values {
            let _ = writeln!(out, "\t{key} = {}", scalar(item)?);
        }
    }
    Ok(())
}

/// Renders the Git config text for `value` (docs/adapters.md, `git-config/v1`).
///
/// `value` is a table of sections. A section is a table whose scalar or array entries are keys
/// and whose table entries are subsections (`[remote "origin"]`). TOML tables carry no order,
/// so sections, keys and subsections are written in byte order of their names. Only the
/// elements of an array keep the order the author wrote. The text ends with a newline unless
/// it is empty.
fn git_config(value: &Value) -> Result<Vec<u8>, Refusal> {
    let sections = value
        .as_table()
        .ok_or_else(|| invalid("git-config value must be a table of sections"))?;
    let mut out = String::new();
    for (name, section) in sections {
        git_name("section", name)?;
        let table = section
            .as_table()
            .ok_or_else(|| invalid(format!("git-config section {name:?} must be a table")))?;
        if table.values().any(|v| !v.is_table()) {
            let _ = writeln!(out, "[{name}]");
            entries(&mut out, table)?;
        }
        for (sub, body) in table.iter().filter(|(_, v)| v.is_table()) {
            let Some(inner) = body.as_table() else {
                continue;
            };
            if inner.values().any(Value::is_table) {
                return Err(invalid(format!(
                    "git-config {name}.{sub} nests deeper than one subsection"
                )));
            }
            let _ = writeln!(out, "[{name} {}]", quote(sub)?);
            entries(&mut out, inner)?;
        }
    }
    Ok(out.into_bytes())
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, Refusal> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("text-fragments value needs a string {key}")))
}

fn utf8(what: &str, bytes: &[u8]) -> Result<String, Refusal> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| Refusal::new(Code::Conflict, format!("{what} is not UTF-8 text")))
}

/// Joins the body and the listed fragments with `separator` (docs/adapters.md,
/// `text-fragments/v1`). Fragments follow the body in byte order of their names. Nothing is
/// added at the end. A fragment name in the directory that matches `pattern` and that the plan
/// did not list means the directory changed since the plan: `E_PLAN_STALE`.
fn text_fragments(
    value: &Value,
    op: &Operation,
    inputs: &GeneratorInputs,
) -> Result<Vec<u8>, Refusal> {
    let (pattern, separator) = (text(value, "pattern")?, text(value, "separator")?);
    let [input] = op.inputs.as_slice() else {
        return Err(Refusal::new(
            Code::PlanInvalid,
            format!("{} needs exactly one body input", op.id),
        ));
    };
    let body = inputs.sources.get(&input.path).ok_or_else(|| {
        Refusal::new(
            Code::PlanInvalid,
            format!("body {} was not supplied", input.path),
        )
    })?;
    if let Some(extra) = inputs.listing.iter().flatten().find(|n| {
        matches_pattern(pattern, n) && !inputs.fragments.keys().any(|k| fold(k) == fold(n))
    }) {
        return Err(Refusal::new(
            Code::PlanStale,
            format!("fragment {extra} is in the directory and the plan does not list it"),
        ));
    }
    let mut parts = vec![utf8(&format!("body {}", input.path), body)?];
    for (name, bytes) in &inputs.fragments {
        parts.push(utf8(&format!("fragment {name}"), bytes)?);
    }
    Ok(parts.join(separator).into_bytes())
}

/// Fills `output_sha256` of every planned generated file and marks one `current` when the
/// owned live file already equals it (so a rerun plans nothing). The generators run on the
/// same inputs apply reads again: source files from the loaded snapshot, foreign fragments from
/// HOME, and selected fragments from the output their producer will write. Operations are in
/// dependency order, so a producer comes before its consumer. A generation failure blocks the
/// operation with a blocker that names the field and carries no content.
pub fn plan_outputs(
    plan: &mut Plan,
    resolved: &Resolved,
    sources: &SourceSet,
    home: &Dir,
) -> Result<(), Refusal> {
    let generators = Generators { resolved };
    let mut made: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    // A link producer puts its source file in the fragment directory.
    for op in &plan.operations {
        if let (Some(p), Some(_), Some(Entry::File(b))) = (
            &op.path,
            &op.link,
            op.inputs.first().and_then(|i| sources.get(&i.path)),
        ) {
            made.insert(fold(p), b.clone());
        }
    }
    let mut blockers = Vec::new();
    for i in 0..plan.operations.len() {
        let op = plan.operations[i].clone();
        if op.status != Status::Planned
            || !matches!(op.action, Action::CreateFile | Action::UpdateFile)
            || !generators.supports(&op)
        {
            continue;
        }
        let built = inputs_for(&op, &made, sources, home)
            .and_then(|inputs| generators.generate(&op, &inputs));
        let out = &mut plan.operations[i];
        match built {
            Ok(bytes) if u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= MAX_HASHED_BYTES => {
                let hash = sha256_hex(&bytes);
                if out.action == Action::UpdateFile
                    && out.ownership == Ownership::Owned
                    && live_equals(&plan.observations, out, &hash)
                {
                    out.action = Action::None;
                    out.status = Status::Current;
                    out.effect = format!("{} already holds the generated content", op.id);
                }
                out.output_sha256 = Some(hash);
                if let Some(p) = &op.path {
                    made.insert(fold(p), bytes);
                }
            }
            Ok(_) => blockers.push(blocker(
                resolved,
                out,
                Refusal::new(Code::LimitExceeded, "the generated content exceeds 16 MiB"),
            )),
            Err(r) => blockers.push(blocker(resolved, out, r)),
        }
    }
    plan.blockers.extend(blockers);
    Ok(())
}

fn live_equals(obs: &dfm_model::plan::Observations, op: &Operation, hash: &str) -> bool {
    let Some(Node::File { mode, sha256, .. }) = op
        .path
        .as_ref()
        .and_then(|p| obs.seen.get(p))
        .map(|s| &s.node)
    else {
        return false;
    };
    sha256.as_deref() == Some(hash) && op.mode.is_none_or(|m| m == *mode)
}

fn blocker(resolved: &Resolved, op: &mut Operation, r: Refusal) -> Blocker {
    op.status = Status::Blocked;
    op.effect = format!("blocked: {}", r.detail);
    Blocker {
        code: r.code,
        resource: op.resource.clone(),
        source: resolved
            .resources
            .get(&op.resource)
            .map_or_else(String::new, |x| x.declared_in.clone()),
        field: Some("value".into()),
        path: op.path.clone(),
        detail: r.detail,
        correction: "fix the resource value or the input file named above, then plan again".into(),
    }
}

/// The inputs as they stand at plan time. The listing is exactly the planned fragments: the
/// fragments a planned removal deletes and the producers not yet written are accounted for by
/// the plan itself.
fn inputs_for(
    op: &Operation,
    made: &BTreeMap<String, Vec<u8>>,
    sources: &SourceSet,
    home: &Dir,
) -> Result<GeneratorInputs, Refusal> {
    let mut inputs = GeneratorInputs::default();
    for i in &op.inputs {
        match sources.get(&i.path) {
            Some(Entry::File(b)) => {
                inputs.sources.insert(i.path.clone(), b.clone());
            }
            _ => {
                return Err(Refusal::new(
                    Code::SourceMissing,
                    format!("source {} is not a file in the loaded tree", i.path),
                ));
            }
        }
    }
    let Some(dir) = op.fragment_dir.as_deref() else {
        return Ok(inputs);
    };
    for f in &op.fragments {
        let path = format!("{dir}/{}", f.name);
        let bytes = if f.origin.starts_with("selected:") {
            made.get(&fold(&path)).cloned().ok_or_else(|| {
                Refusal::new(
                    Code::GeneratorUnavailable,
                    format!("fragment {path} is not produced by a generated operation"),
                )
            })?
        } else {
            home.read_below(&path, MAX_HASHED_BYTES)?.ok_or_else(|| {
                Refusal::new(
                    Code::Conflict,
                    format!("fragment {path} is not a plain file within 16 MiB"),
                )
            })?
        };
        inputs.fragments.insert(f.name.clone(), bytes);
    }
    inputs.listing = Some(inputs.fragments.keys().cloned().collect());
    Ok(inputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(src: &str) -> Result<String, Refusal> {
        let v = Value::Table(src.parse::<Table>().expect("fixture toml"));
        git_config(&v).map(|b| String::from_utf8(b).expect("utf8"))
    }

    #[test]
    fn git_config_writes_sorted_sections_and_ends_with_a_newline() {
        let t = cfg(
            "[init]\ndefaultBranch = \"main\"\n[core]\neditor = \"nvim\"\n[fetch]\nprune = true\n",
        )
        .expect("renders");
        assert_eq!(
            t,
            "[core]\n\teditor = \"nvim\"\n[fetch]\n\tprune = true\n[init]\n\tdefaultBranch = \"main\"\n"
        );
    }

    #[test]
    fn git_config_array_keeps_its_order_as_repeated_keys() {
        let t = cfg("[credential]\nhelper = [\"cache\", \"store\"]\n").expect("renders");
        assert_eq!(
            t,
            "[credential]\n\thelper = \"cache\"\n\thelper = \"store\"\n"
        );
    }

    #[test]
    fn git_config_subsections_and_escapes() {
        let t = cfg("[includeIf.\"gitdir:~/work/\"]\npath = \"a\\\"b\\\\c\"\n").expect("renders");
        assert_eq!(
            t,
            "[includeIf \"gitdir:~/work/\"]\n\tpath = \"a\\\"b\\\\c\"\n"
        );
    }

    #[test]
    fn git_config_refuses_bad_names_floats_and_control_characters() {
        for bad in [
            "[\"bad name\"]\nk = 1\n",
            "[core]\n\"1key\" = 1\n",
            "[core]\nk = 1.5\n",
            "[core]\nk = \"a\\u0001b\"\n",
            "[core]\nk = [[1]]\n",
            "[a.b.c]\nk = 1\n",
        ] {
            let e = cfg(bad).expect_err(bad);
            assert_eq!(e.code, Code::ManifestInvalid, "{bad}");
        }
    }
}
