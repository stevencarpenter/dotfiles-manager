//! Cost bounds: linear dependency ordering, sorted policy checks, and hard caps on modules,
//! resources, values, bundle depth and explain output.

use dfm_cli::builtin_catalog;
use dfm_model::Code;
use dfm_model::resolve::{ResolveError, Resolved, SourceSet, explain, resolve};
use std::fmt::Write;
use std::time::{Duration, Instant};

/// A generous bound. The checks below fail by orders of magnitude, not by a few seconds, when
/// a quadratic scan comes back.
const GENEROUS: Duration = Duration::from_secs(30);

/// One bundle with a core and the given modules, and one profile selecting `selected`.
fn build(modules: &[(String, String)], selected: &[String]) -> SourceSet {
    let mut set = SourceSet::new();
    let mut bundle = String::from("schema = 1\nid = \"gen\"\ncore = \"m/core.toml\"\n[modules]\n");
    for (id, text) in modules {
        let _ = writeln!(bundle, "{id} = \"m/{id}.toml\"");
        set.insert_file(format!("b/m/{id}.toml"), text.as_bytes());
    }
    set.insert_file("b/bundle.toml", bundle);
    set.insert_file(
        "b/m/core.toml",
        "schema = 1\nid = \"core\"\nplatforms = [\"macos\"]\nrequires = []\n",
    );
    let list: Vec<String> = selected.iter().map(|s| format!("\"{s}\"")).collect();
    set.insert_file(
        "p/profile.toml",
        format!(
            "schema = 1\nid = \"gen-01\"\nbundle = \"../b/bundle.toml\"\nplatform = \"macos\"\narchitecture = \"arm64\"\nmodules = [{}]\n",
            list.join(", ")
        ),
    );
    set
}

fn module(id: &str, requires: &[String], body: &str) -> (String, String) {
    let req: Vec<String> = requires.iter().map(|r| format!("\"{r}\"")).collect();
    (
        id.to_string(),
        format!(
            "schema = 1\nid = \"{id}\"\nplatforms = [\"macos\"]\nrequires = [{}]\n{body}",
            req.join(", ")
        ),
    )
}

fn run(set: &SourceSet) -> Result<Resolved, ResolveError> {
    resolve(set, "p/profile.toml", &builtin_catalog())
}

fn links(module_id: &str, n: usize) -> String {
    let mut s = String::new();
    for j in 0..n {
        let _ = write!(
            s,
            "[[resources]]\nid = \"r{j:05}\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \"t/{module_id}/r{j:05}\"\n[resources.value]\nsource = \"files/f\"\n"
        );
    }
    s
}

fn big_render(module_id: &str, keys: usize, policy: bool) -> String {
    let mut s = format!(
        "[[resources]]\nid = \"big\"\nkind = \"render\"\nadapter = \"git-config/v1\"\ntarget = \"t/{module_id}\"\n[resources.value.k]\n"
    );
    for j in 0..keys {
        let _ = writeln!(s, "k{j:05} = 1");
    }
    if policy {
        let list: Vec<String> = (0..keys).map(|j| format!("\"/k/k{j:05}\"")).collect();
        let _ = write!(s, "[resources.policy]\ndefaults = [{}]\n", list.join(", "));
    }
    s
}

#[test]
fn l1_dependency_ordering_is_linear_in_a_long_chain() {
    let ids: Vec<String> = (0..3000).map(|i| format!("m{i:04}")).collect();
    let modules: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| module(id, &ids[i.saturating_sub(1)..i], ""))
        .collect();
    let set = build(&modules, &ids[2999..]);
    let t = Instant::now();
    let r = run(&set).unwrap();
    assert!(t.elapsed() < GENEROUS, "{:?}", t.elapsed());
    let mut want = vec!["core".to_string()];
    want.extend(ids);
    assert_eq!(r.modules, want, "each module comes after its prerequisite");
}

#[test]
fn l1_a_long_cycle_is_still_found() {
    let ids: Vec<String> = (0..500).map(|i| format!("m{i:03}")).collect();
    let modules: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| module(id, &[ids[(i + 1) % ids.len()].clone()], ""))
        .collect();
    let e = run(&build(&modules, &ids[..1])).unwrap_err();
    assert_eq!(e.code, Code::DependencyCycle);
}

#[test]
fn l2_policy_checks_scale_to_twenty_thousand_defaults() {
    let m = module("big-mod", &[], &big_render("big-mod", 20_000, true));
    let set = build(&[m], &["big-mod".into()]);
    let t = Instant::now();
    let r = run(&set).unwrap();
    assert!(t.elapsed() < GENEROUS, "{:?}", t.elapsed());
    assert_eq!(r.resources["big-mod.big"].defaults.len(), 20_000);
    // Overlaps are still caught: a parent of an existing default.
    let m = module(
        "big-mod",
        &[],
        &big_render("big-mod", 3, true).replace("\"/k/k00001\"", "\"/k\""),
    );
    let e = run(&build(&[m], &["big-mod".into()])).unwrap_err();
    assert_eq!(e.code, Code::ManifestInvalid);
    assert!(e.message.contains("overlaps"), "{e}");
}

#[test]
fn l3_the_resource_cap_is_enforced() {
    let ids: Vec<String> = (0..4).map(|i| format!("res{i}")).collect();
    let modules: Vec<_> = ids
        .iter()
        .map(|id| module(id, &[], &links(id, 2600)))
        .collect();
    let e = run(&build(&modules, &ids)).unwrap_err();
    assert_eq!(e.code, Code::LimitExceeded);
    assert!(e.message.contains("10000"), "{e}");
    // Just under the cap resolves as far as source checks (files/f is missing on purpose).
    let ids: Vec<String> = (0..3).map(|i| format!("res{i}")).collect();
    let modules: Vec<_> = ids
        .iter()
        .map(|id| module(id, &[], &links(id, 3300)))
        .collect();
    let e = run(&build(&modules, &ids)).unwrap_err();
    assert_eq!(
        e.code,
        Code::SourceMissing,
        "9900 resources pass the cap: {e}"
    );
}

#[test]
fn l3_the_module_cap_is_enforced() {
    let ids: Vec<String> = (0..10_001).map(|i| format!("m{i:05}")).collect();
    let modules: Vec<_> = ids.iter().map(|id| module(id, &[], "")).collect();
    let e = run(&build(&modules, &ids)).unwrap_err();
    assert_eq!(e.code, Code::LimitExceeded);
    assert!(e.message.contains("10000 modules"), "{e}");
}

#[test]
fn l3_the_value_budget_stops_the_composition_before_provenance_is_built() {
    let ids: Vec<String> = (0..6).map(|i| format!("wide{i}")).collect();
    let modules: Vec<_> = ids
        .iter()
        .map(|id| module(id, &[], &big_render(id, 40_000, false)))
        .collect();
    let e = run(&build(&modules, &ids)).unwrap_err();
    assert_eq!(e.code, Code::LimitExceeded);
    assert!(e.message.contains("200000"), "{e}");
    // Five modules stay under the budget.
    let r = run(&build(&modules[..5], &ids[..5])).unwrap();
    assert_eq!(r.resources.len(), 5);
}

#[test]
fn l3_bundle_ancestry_is_capped_at_sixteen() {
    let chain = |n: usize| {
        let mut set = SourceSet::new();
        for i in 0..n {
            let text = if i == 0 {
                "schema = 1\nid = \"b0\"\ncore = \"core.toml\"\n[modules]\n".to_string()
            } else {
                format!(
                    "schema = 1\nid = \"b{i}\"\nextends = \"../b{}/bundle.toml\"\n[modules]\n",
                    i - 1
                )
            };
            set.insert_file(format!("b{i}/bundle.toml"), text);
        }
        set.insert_file(
            "b0/core.toml",
            "schema = 1\nid = \"core\"\nplatforms = [\"macos\"]\nrequires = []\n",
        );
        set.insert_file(
            "p/profile.toml",
            format!(
                "schema = 1\nid = \"gen-01\"\nbundle = \"../b{}/bundle.toml\"\nplatform = \"macos\"\narchitecture = \"arm64\"\nmodules = []\n",
                n - 1
            ),
        );
        set
    };
    assert_eq!(run(&chain(16)).unwrap().modules, ["core"]);
    let e = run(&chain(17)).unwrap_err();
    assert_eq!(e.code, Code::LimitExceeded);
    assert!(e.message.contains("16"), "{e}");
}

#[test]
fn l3_explain_clips_long_text_and_refuses_oversized_reports() {
    let long = "r".repeat(5000);
    let body = format!(
        "[[overrides]]\nresource = \"core.thing\"\npath = \"/v\"\nvalue = 2\nreason = \"{long}\"\n"
    );
    let core = "schema = 1\nid = \"core\"\nplatforms = [\"macos\"]\nrequires = []\n[[resources]]\nid = \"thing\"\nkind = \"render\"\nadapter = \"git-config/v1\"\ntarget = \"t/thing\"\n[resources.value]\nv = 1\n[resources.policy]\ndefaults = [\"/v\"]\n";
    let mut set = build(&[module("ovr", &[], &body)], &["ovr".into()]);
    set.insert_file("b/m/core.toml", core);
    let r = run(&set).unwrap();
    let text = explain(&r, "core.thing").unwrap();
    assert!(
        text.contains("reason: rrrr") && text.contains("..."),
        "{text}"
    );
    assert!(!text.contains(&long) && text.len() < 4000, "{}", text.len());

    let m = module("wide", &[], &big_render("wide", 40_000, false));
    let r = run(&build(&[m], &["wide".into()])).unwrap();
    let e = explain(&r, "wide.big").unwrap_err();
    assert_eq!(e.code, Code::LimitExceeded);
}
