//! The `render` and `assemble` adapters (docs/adapters.md). Pure functions of the resource
//! value and the verified inputs apply or the planner supplies. They open no file.

use super::{Adapter, registry, secret};
use dfm_engine::{Dir, Generator, GeneratorInputs, Got, Runner, Sys};
use dfm_model::digest::sha256_hex;
use dfm_model::fold;
use dfm_model::merge::{Desired, Merged, merge_with, retire};
use dfm_model::plan::{
    Action, Blocker, MAX_HASHED_BYTES, Node, Observations, Operation, Ownership, Plan, Status,
    file_fingerprint, matches_pattern,
};
use dfm_model::resolve::{Resolved, Table, Value};
use dfm_model::state::Class;
use dfm_model::{Code, Refusal};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

/// What an adapter that produces file content provides, besides its catalog entry.
pub struct Gen {
    /// The content, from the resource value and the verified inputs.
    pub run: fn(&Value, &Operation, &GeneratorInputs) -> Result<Vec<u8>, Refusal>,
    /// The settings the value sets, as names only, for the reviewer. Never values.
    pub sets: fn(&Value) -> Vec<String>,
}

pub const GIT_CONFIG: Gen = Gen {
    run: |value, _, _| git_config(value),
    sets: git_config_names,
};

pub const TEXT_FRAGMENTS: Gen = Gen {
    run: text_fragments,
    sets: |_| Vec::new(),
};

/// The generators of this build, reading each resource value from the resolved model of the
/// plan being applied. The value is data read at run time: a config edit needs no rebuild.
/// Which adapters generate is decided by the registry in `adapters.rs`, nowhere else.
pub struct Generators<'a> {
    resolved: &'a Resolved,
    table: Vec<Adapter>,
    /// How a provider child is started. `None` in a build of the plan: nothing renders then.
    runner: Option<Runner>,
}

impl<'a> Generators<'a> {
    pub fn new(resolved: &'a Resolved) -> Self {
        Generators {
            resolved,
            table: registry(),
            runner: None,
        }
    }

    /// Generators that can also render a secret through `runner` (the `secrets` command).
    pub fn with_runner(resolved: &'a Resolved, runner: Runner) -> Self {
        Generators {
            runner: Some(runner),
            ..Generators::new(resolved)
        }
    }

    fn gen_of(&self, op: &Operation) -> Option<&Gen> {
        self.table
            .iter()
            .find(|a| a.spec.id == op.adapter)
            .and_then(|a| a.generator.as_ref())
    }

    fn value(&self, op: &Operation) -> Result<&'a Value, Refusal> {
        self.resolved
            .resources
            .get(&op.resource)
            .map(|r| &r.value)
            .ok_or_else(|| {
                Refusal::new(
                    Code::GeneratorUnavailable,
                    format!("{} is not in the resolved model", op.resource),
                )
            })
    }
}

impl Generator for Generators<'_> {
    fn merges(&self, op: &Operation) -> bool {
        op.class == Class::Sensitive
            && matches!(
                op.action,
                Action::CreateFile | Action::UpdateFile | Action::Retire
            )
            && self
                .table
                .iter()
                .any(|a| a.spec.id == op.adapter && a.merges())
    }

    fn merge(&self, op: &Operation, inputs: &GeneratorInputs) -> Result<Merged, Refusal> {
        if !self.merges(op) {
            return Err(Refusal::new(
                Code::GeneratorUnavailable,
                format!("no merge adapter for {}", op.adapter),
            ));
        }
        if op.action == Action::Retire {
            // The resource is no longer selected, so there is no declared value. Only the
            // entries the ledger records are removed.
            let live = inputs.live.as_deref().ok_or_else(|| {
                Refusal::new(Code::Conflict, "the file to retire entries from is gone")
            })?;
            let (bytes, changed) = retire(live, &inputs.owned_entries, &inputs.owned_appended)?;
            return Ok(Merged {
                bytes,
                rendered_view: Vec::new(),
                live_leaves: Default::default(),
                rendered_leaves: Default::default(),
                entries: Vec::new(),
                appended: Vec::new(),
                changed,
            });
        }
        let mut desired = Desired::from_value(self.value(op)?)?;
        if let Some(h) = &inputs.home {
            desired = desired.with_home(h);
        }
        let desired = match desired.fragment_spec() {
            Some((_, pattern)) => {
                // As an assembly: a matching name in the directory that the plan did not list
                // means the directory changed since the plan.
                if let Some(extra) = inputs.listing.iter().flatten().find(|n| {
                    matches_pattern(pattern, n)
                        && !inputs.fragments.keys().any(|k| dfm_model::fold::eq(k, n))
                }) {
                    return Err(Refusal::new(
                        Code::PlanStale,
                        format!(
                            "fragment {extra} is in the directory and the plan does not list it"
                        ),
                    ));
                }
                desired.with_fragments(
                    inputs
                        .fragments
                        .iter()
                        .map(|(n, b)| (n.as_str(), b.as_slice())),
                )?
            }
            None => desired,
        };
        merge_with(
            inputs.live.as_deref(),
            &desired,
            &inputs.owned_entries,
            &inputs.owned_appended,
            inputs.takeover,
        )
    }

    fn renders(&self, op: &Operation) -> bool {
        op.class == Class::Sensitive
            && matches!(op.action, Action::CreateFile | Action::UpdateFile)
            && self.runner.is_some()
            && self.resolved.resources.contains_key(&op.resource)
            && self
                .table
                .iter()
                .any(|a| a.spec.id == op.adapter && a.renders())
    }

    fn render(&self, op: &Operation, template: &[u8]) -> Result<Vec<u8>, Refusal> {
        let runner = self
            .runner
            .as_ref()
            .filter(|_| self.renders(op))
            .ok_or_else(|| {
                Refusal::new(
                    Code::GeneratorUnavailable,
                    format!("no render adapter for {}", op.adapter),
                )
            })?;
        secret::render(runner, self.value(op)?, template)
    }

    fn supports(&self, op: &Operation) -> bool {
        op.class == Class::Normal
            && self.gen_of(op).is_some()
            && self.resolved.resources.contains_key(&op.resource)
    }

    fn generate(&self, op: &Operation, inputs: &GeneratorInputs) -> Result<Vec<u8>, Refusal> {
        let gen_fn = self.gen_of(op).ok_or_else(|| {
            Refusal::new(
                Code::GeneratorUnavailable,
                format!("no generator for {}", op.adapter),
            )
        })?;
        (gen_fn.run)(self.value(op)?, op, inputs)
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

/// A quoted Git value. Backslash and quote are escaped. Newline and tab use
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

/// A quoted subsection name. Git decodes only `\\` and `\"` in it, so those two are escaped and
/// nothing else is: a newline, a tab or another control character cannot be written and is
/// refused.
fn subsection(s: &str) -> Result<String, Refusal> {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => {
                return Err(invalid(format!(
                    "git-config subsection holds the control character U+{:04X}",
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
    let mut seen = BTreeSet::new();
    for (key, v) in table.iter().filter(|(_, v)| !v.is_table()) {
        git_name("key", key)?;
        distinct(&mut seen, "key", key)?;
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

/// Git reads section and key names without regard to ASCII case, so two that differ only in
/// case would be one setting written twice. Subsection names are case-sensitive and skip this.
fn distinct(seen: &mut BTreeSet<String>, what: &str, name: &str) -> Result<(), Refusal> {
    if seen.insert(name.to_ascii_lowercase()) {
        Ok(())
    } else {
        Err(invalid(format!(
            "git-config {what} {name:?} repeats another {what} that differs only in ASCII case"
        )))
    }
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
    let mut seen = BTreeSet::new();
    for (name, section) in sections {
        git_name("section", name)?;
        distinct(&mut seen, "section", name)?;
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
            let _ = writeln!(out, "[{name} {}]", subsection(sub)?);
            entries(&mut out, inner)?;
        }
    }
    Ok(out.into_bytes())
}

/// `section.key` and `section.subsection.key` for every key the value sets, sorted, once per
/// key. Names only: a reviewer sees what is set without any value.
fn git_config_names(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for (name, section) in value.as_table().into_iter().flatten() {
        for (key, v) in section.as_table().into_iter().flatten() {
            match v.as_table() {
                Some(sub) => out.extend(sub.keys().map(|k| format!("{name}.{key}.{k}"))),
                None => out.push(format!("{name}.{key}")),
            }
        }
    }
    out.sort();
    out
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
        matches_pattern(pattern, n) && !inputs.fragments.keys().any(|k| dfm_model::fold::eq(k, n))
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

/// The files link producers will put in a fragment directory, by folded path: their source
/// files. Only a planned or current operation of normal class produces anything.
fn link_producers(
    ops: &[Operation],
    source: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> BTreeMap<String, Vec<u8>> {
    let mut made = BTreeMap::new();
    for op in ops {
        if op.class != Class::Normal || !matches!(op.status, Status::Planned | Status::Current) {
            continue;
        }
        if let (Some(p), Some(_), Some(b)) = (
            &op.path,
            &op.link,
            op.inputs.first().and_then(|i| source(&i.path)),
        ) {
            made.insert(fold(p), b);
        }
    }
    made
}

/// Fills `output_sha256`, `output_bytes` and `sets` of every planned generated file and marks
/// one `current` when the owned live file already equals its output (so a rerun plans nothing).
/// The generators run on the same inputs apply reads again: source files from the loaded
/// snapshot, foreign fragments from HOME, and selected fragments from the output their producer
/// will write. Operations are in dependency order, so a producer comes before its consumer. A
/// generation failure blocks the operation with a blocker that names the field and carries no
/// content. A consumer of a blocked producer reports the producer's blocker as the cause.
pub fn plan_outputs(plan: &mut Plan, resolved: &Resolved, sys: &Sys, home: &Dir) {
    let generators = Generators::new(resolved);
    // Source files are read as apply reads them: below the opened source root, without
    // following a link, regular, owned by the user, one hard link (`Dir::read_input`). A source
    // that apply would refuse is refused here, as a blocker, not as a surprise at apply.
    let root = match plan.sources.as_slice() {
        [one] => Dir::open_plain(std::path::Path::new(&one.root)).ok(),
        _ => None,
    };
    let source = |path: &str| -> Option<Vec<u8>> {
        match root.as_ref()?.read_input_below(sys, path, MAX_HASHED_BYTES) {
            Ok(Got::Bytes(b)) => Some(b),
            _ => None,
        }
    };
    let mut made = link_producers(&plan.operations, &source);
    // Why a path has no content: the blocker that stopped its producer, by folded path.
    let mut causes: BTreeMap<String, (Code, String)> = plan
        .blockers
        .iter()
        .filter_map(|b| Some((fold(b.path.as_deref()?), (b.code, b.detail.clone()))))
        .collect();
    let mut blockers = Vec::new();
    for i in 0..plan.operations.len() {
        let op = plan.operations[i].clone();
        if op.class != Class::Normal
            || op.status != Status::Planned
            || !matches!(op.action, Action::CreateFile | Action::UpdateFile)
            || !generators.supports(&op)
        {
            continue;
        }
        let built = inputs_for(&op, &made, &causes, &source, sys, home)
            .and_then(|inputs| generators.generate(&op, &inputs));
        let out = &mut plan.operations[i];
        match built {
            Ok(bytes) if u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= MAX_HASHED_BYTES => {
                let hash = sha256_hex(&bytes);
                if out.action == Action::UpdateFile
                    && out.ownership == Ownership::Owned
                    && live_equals(&plan.observations, out, &file_fingerprint(&bytes))
                {
                    out.action = Action::None;
                    out.status = Status::Current;
                    out.effect = format!("{} already holds the generated content", op.id);
                }
                out.output_sha256 = Some(hash);
                out.output_bytes = u64::try_from(bytes.len()).ok();
                if let (Some(g), Ok(v)) = (generators.gen_of(&op), generators.value(&op)) {
                    out.sets = (g.sets)(v);
                }
                if let Some(p) = &op.path {
                    made.insert(fold(p), bytes);
                }
            }
            Ok(_) => {
                let r = Refusal::new(Code::LimitExceeded, "the generated content exceeds 16 MiB");
                blockers.push(blocker(resolved, out, r, &mut causes));
            }
            Err(r) => blockers.push(blocker(resolved, out, r, &mut causes)),
        }
    }
    plan.blockers.extend(blockers);
}

fn live_equals(obs: &Observations, op: &Operation, hash: &str) -> bool {
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

fn blocker(
    resolved: &Resolved,
    op: &mut Operation,
    r: Refusal,
    causes: &mut BTreeMap<String, (Code, String)>,
) -> Blocker {
    op.status = Status::Blocked;
    op.effect = format!("blocked: {}", r.detail);
    if let Some(p) = &op.path {
        causes.insert(fold(p), (r.code, r.detail.clone()));
    }
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
/// the plan itself. Fragments together are bounded like at apply time.
fn inputs_for(
    op: &Operation,
    made: &BTreeMap<String, Vec<u8>>,
    causes: &BTreeMap<String, (Code, String)>,
    source: &dyn Fn(&str) -> Option<Vec<u8>>,
    sys: &Sys,
    home: &Dir,
) -> Result<GeneratorInputs, Refusal> {
    let mut inputs = GeneratorInputs::default();
    for i in &op.inputs {
        let bytes = source(&i.path).ok_or_else(|| {
            Refusal::new(
                Code::SourceUntrusted,
                format!(
                    "source {} is not a regular file owned by you with one hard link (a hardlinked or foreign-owned source is refused)",
                    i.path
                ),
            )
        })?;
        if i.content_id.as_deref() != Some(format!("sha256:{}", sha256_hex(&bytes)).as_str()) {
            return Err(Refusal::new(
                Code::SourceUntrusted,
                format!("source {} changed while the plan was made", i.path),
            ));
        }
        inputs.sources.insert(i.path.clone(), bytes);
    }
    let Some(dir) = op.fragment_dir.as_deref() else {
        return Ok(inputs);
    };
    let too_much = || Refusal::new(Code::LimitExceeded, "the fragments together exceed 16 MiB");
    let mut left = MAX_HASHED_BYTES;
    for f in &op.fragments {
        let path = format!("{dir}/{}", f.name);
        let bytes = if f.origin.starts_with("selected:") {
            match made.get(&fold(&path)) {
                Some(b) => b.clone(),
                // The producer has no content because something stopped it: say what.
                None => {
                    return Err(match causes.get(&fold(&path)) {
                        Some((code, why)) => Refusal::new(
                            *code,
                            format!("fragment {path} depends on a blocked producer: {why}"),
                        ),
                        None => Refusal::new(
                            Code::GeneratorUnavailable,
                            format!(
                                "fragment {path} has no content: its producer is not a generated file, or its source file is missing, hardlinked or not yours"
                            ),
                        ),
                    });
                }
            }
        } else {
            match home.read_input_below(sys, &path, left)? {
                Got::Bytes(b) => b,
                Got::TooBig => return Err(too_much()),
                Got::Absent => {
                    return Err(Refusal::new(
                        Code::Conflict,
                        format!("fragment {path} is not a plain file of yours with one link"),
                    ));
                }
            }
        };
        left = left
            .checked_sub(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .ok_or_else(too_much)?;
        inputs.fragments.insert(f.name.clone(), bytes);
    }
    inputs.listing = Some(inputs.fragments.keys().cloned().collect());
    Ok(inputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dfm_model::resolve::SourceSet;

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
    fn a_subsection_escapes_only_backslash_and_quote_and_refuses_control_characters() {
        let t = cfg("[url.\"a\\\\b\\\"c\"]\nk = 1\n").expect("renders");
        assert_eq!(t, "[url \"a\\\\b\\\"c\"]\n\tk = 1\n");
        for bad in [
            "[url.\"a\\nb\"]\nk = 1\n",
            "[url.\"a\\tb\"]\nk = 1\n",
            "[url.\"x\\\"]\\n[core]\"]\nk = 1\n",
            "[url.\"a\\u0001b\"]\nk = 1\n",
        ] {
            let e = cfg(bad).expect_err(bad);
            assert_eq!(e.code, Code::ManifestInvalid, "{bad}");
            assert!(e.detail.contains("subsection"), "{}", e.detail);
        }
    }

    #[test]
    fn names_that_differ_only_in_ascii_case_are_refused_but_subsections_are_case_sensitive() {
        for bad in [
            "[core]\neditor = \"a\"\nEditor = \"b\"\n",
            "[core]\neditor = \"a\"\n[Core]\neditor = \"b\"\n",
            "[url.\"x\"]\ninsteadOf = \"a\"\ninsteadof = \"b\"\n",
        ] {
            let e = cfg(bad).expect_err(bad);
            assert_eq!(e.code, Code::ManifestInvalid, "{bad}");
            assert!(e.detail.contains("ASCII case"), "{}", e.detail);
        }
        let t = cfg("[url.\"X\"]\nk = 1\n[url.\"x\"]\nk = 2\n[core]\nk = 1\n[core.sub]\nK = 2\n")
            .expect("renders");
        assert!(t.contains("[url \"X\"]") && t.contains("[url \"x\"]"));
    }

    #[test]
    fn the_setting_names_list_keys_and_never_values() {
        let v = Value::Table(
            "[core]\nsshCommand = \"ssh -i secret\"\n[credential]\nhelper = [\"a\", \"b\"]\n[url.\"x\"]\ninsteadOf = \"y\"\n"
                .parse::<Table>()
                .expect("toml"),
        );
        let names = git_config_names(&v);
        assert_eq!(
            names,
            ["core.sshCommand", "credential.helper", "url.x.insteadOf"]
        );
        assert!(!names.join(" ").contains("secret"));
    }

    fn link_op(path: &str, class: Class, status: Status) -> Operation {
        serde_json::from_value(serde_json::json!({
            "id": format!("r@{path}"), "journal_id": "j", "resource": "r", "module": "m",
            "adapter": "fs-link/v1", "class": class, "action": "create_link",
            "status": status, "path": path, "mode": null, "link": "/src/f",
            "privilege": "user", "effect": "", "ownership": "absent",
            "inputs": [{"path": "f", "content_id": null}], "fragments": [],
            "after": [], "preconditions": [], "postcondition": ""
        }))
        .expect("operation")
    }

    #[test]
    fn only_a_planned_or_current_normal_link_produces_a_fragment() {
        let mut src = SourceSet::new();
        src.insert_file("f", "body");
        let ops = [
            link_op("ok-planned", Class::Normal, Status::Planned),
            link_op("ok-current", Class::Normal, Status::Current),
            link_op("blocked", Class::Normal, Status::Blocked),
            link_op("scope", Class::Normal, Status::NotInScope),
            link_op("sensitive", Class::Sensitive, Status::Planned),
        ];
        let made = link_producers(&ops, &|p| match src.get(p) {
            Some(dfm_model::resolve::Entry::File(b)) => Some(b.clone()),
            _ => None,
        });
        let keys: Vec<&str> = made.keys().map(String::as_str).collect();
        assert_eq!(keys, ["ok-current", "ok-planned"]);
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
