//! The 16 resolver definitions in examples/cases.toml (docs/architecture.md section 13).

use crate::harness;
use dfm_cli::builtin_catalog;
use dfm_model::resolve::{ResolveError, Resolved, resolve};
use toml::Value;

use crate::harness::examples;

pub fn cases() -> Vec<Value> {
    let set = examples();
    let doc: Value = toml::from_str(&harness::text(&set, "cases.toml")).unwrap();
    doc["cases"].as_array().unwrap().clone()
}

/// Resolves one case against its own copy of the fixtures.
pub fn run(case: &Value) -> Result<Resolved, ResolveError> {
    let mut set = examples();
    for c in case
        .get("changes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        harness::apply(
            &mut set,
            c["file"].as_str().unwrap(),
            c["op"].as_str().unwrap(),
            c["path"].as_str().unwrap(),
            c.get("value").cloned(),
        );
    }
    resolve(&set, case["profile"].as_str().unwrap(), &builtin_catalog())
}

#[test]
fn there_are_sixteen_definitions() {
    assert_eq!(cases().len(), 16);
}

/// Where each of the 12 refusals is reported: the declaring source, the resource if one is
/// involved, and the other owner if there is one.
const LOCATIONS: [(&str, &str, Option<&str>, Option<&str>); 12] = [
    (
        "protected-core-source",
        "profiles/work-01.toml",
        Some("core.git"),
        Some("core"),
    ),
    (
        "competing-default-overrides",
        "profiles/work-01.toml",
        Some("core.git-defaults"),
        Some("work (external/modules/work/module.toml)"),
    ),
    (
        "external-core-replacement",
        "external/bundle.toml",
        None,
        None,
    ),
    ("unknown-module", "profiles/work-01.toml", None, None),
    (
        "duplicate-module-selection",
        "profiles/work-01.toml",
        None,
        None,
    ),
    (
        "module-dependency-cycle",
        "external/modules/work/module.toml",
        None,
        None,
    ),
    (
        "contribution-namespace",
        "external/modules/work/module.toml",
        Some("agents.claude-settings"),
        None,
    ),
    (
        "missing-external-source",
        "profiles/work-01.toml",
        None,
        None,
    ),
    (
        "output-owner-collision",
        "external/modules/work/module.toml",
        Some("work.git-identity"),
        Some("core.git"),
    ),
    (
        "source-containment",
        "base/modules/core/module.toml",
        Some("core.ssh"),
        None,
    ),
    (
        "home-target-containment",
        "base/modules/core/module.toml",
        Some("core.ssh"),
        None,
    ),
    (
        "unsupported-module-platform",
        "base/modules/desktop/module.toml",
        None,
        Some("profiles/work-01.toml"),
    ),
];

#[test]
fn every_definition_gives_its_expected_outcome() {
    let mut refusals = 0;
    for case in cases() {
        let id = case["id"].as_str().unwrap();
        let got = run(&case);
        match (case.get("expected_error"), got) {
            (Some(code), Err(e)) => {
                refusals += 1;
                assert_eq!(e.code.as_str(), code.as_str().unwrap(), "{id}: {e}");
                assert_eq!(e.code.exit().code(), 3, "{id}");
                let (_, source, resource, owner) = LOCATIONS
                    .iter()
                    .find(|l| l.0 == id)
                    .unwrap_or_else(|| panic!("{id}: no expected location"));
                assert_eq!(e.source, *source, "{id}: declaring source: {e}");
                assert_eq!(e.resource.as_deref(), *resource, "{id}: resource: {e}");
                assert_eq!(
                    e.conflicts_with.as_deref(),
                    *owner,
                    "{id}: other owner: {e}"
                );
                assert!(!e.action.is_empty() && !e.message.is_empty(), "{id}");
            }
            (Some(code), Ok(_)) => panic!("{id}: expected {code}, resolved"),
            (None, Err(e)) => panic!("{id}: expected success, got {e}"),
            (None, Ok(r)) => {
                let expect = &case["expect"];
                let modules: Vec<&str> = expect["modules"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| m.as_str().unwrap())
                    .collect();
                assert_eq!(r.modules, modules, "{id}");
                for v in expect
                    .get("values")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let res = &r.resources[v["resource"].as_str().unwrap()];
                    let tokens: Vec<&str> = v["path"].as_str().unwrap()[1..].split('/').collect();
                    let got = tokens.iter().fold(&res.value, |cur, t| &cur[*t]);
                    assert_eq!(got, &v["value"], "{id}: {} {}", res.id, v["path"]);
                }
                for o in expect
                    .get("origins")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let res = &r.resources[o["resource"].as_str().unwrap()];
                    let p = &res.provenance[o["path"].as_str().unwrap()];
                    let want_module = o["module"].as_str().expect("origin names a module");
                    let want_source = o["source"].as_str().expect("origin names a source file");
                    assert_eq!(p.changer.as_deref(), Some(want_module), "{id}");
                    assert_eq!(p.source_path.as_deref(), Some(want_source), "{id}");
                    assert!(
                        p.content_id
                            .as_deref()
                            .is_some_and(|c| c.starts_with("sha256:")),
                        "{id}"
                    );
                    assert!(
                        p.reason.as_deref().is_some_and(|r| !r.trim().is_empty()),
                        "{id}"
                    );
                }
            }
        }
    }
    assert_eq!(
        refusals,
        LOCATIONS.len(),
        "every refusal has an expected location"
    );
}
