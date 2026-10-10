//! The 16 resolver definitions in examples/cases.toml (docs/architecture.md section 13).

use crate::harness;
use dfm_cli::builtin_catalog;
use dfm_model::resolve::{ResolveError, Resolved, resolve};
use toml::Value;

use crate::harness::examples;

fn cases() -> Vec<Value> {
    let set = examples();
    let doc: Value = toml::from_str(&harness::text(&set, "cases.toml")).unwrap();
    doc["cases"].as_array().unwrap().clone()
}

/// Resolves one case against its own copy of the fixtures.
fn run(case: &Value) -> Result<Resolved, ResolveError> {
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

#[test]
fn every_definition_gives_its_expected_outcome() {
    for case in cases() {
        let id = case["id"].as_str().unwrap();
        let got = run(&case);
        match (case.get("expected_error"), got) {
            (Some(code), Err(e)) => {
                assert_eq!(e.code.as_str(), code.as_str().unwrap(), "{id}: {e}");
                assert_eq!(e.code.exit().code(), 3, "{id}");
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
                    assert_eq!(p.changer.as_deref(), o["module"].as_str(), "{id}");
                    assert_eq!(p.source_path.as_deref(), o["source"].as_str(), "{id}");
                    assert!(p.content_id.is_some(), "{id}");
                    assert!(p.reason.is_some(), "{id}");
                }
            }
        }
    }
}
