//! Cost bounds: linear dependency ordering, sorted policy checks, and hard caps on modules,
//! resources, values, bundle depth and explain output.

use crate::harness::synthetic;
use dfm_cli::builtin_catalog;
use dfm_model::Code;
use dfm_model::resolve::{
    AdapterCatalog, AdapterSpec, Claims, Field, Include, ResolveError, Resolved, Schema, SourceSet,
    TargetRule, explain, resolve,
};
use std::fmt::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
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

// Include directives: capped per document and in total, parsed once per document.

fn include_lines(n: usize) -> String {
    (0..n)
        .map(|i| format!("[include]\n    path = ~/.config/inc/f{i:04}\n"))
        .collect()
}

fn set_with_gitconfig(text: &str, extra_links: usize) -> SourceSet {
    let mut set = synthetic();
    set.insert_file("shared/core/files/gitconfig", text);
    for i in 0..extra_links {
        let r = format!(
            "{{ id = \"git{i}\", kind = \"link\", adapter = \"fs-link/v1\", target = \".config/synthetic/gitconfig{i}\", value = {{ source = \"files/gitconfig\" }} }}"
        );
        crate::harness::edit(
            &mut set,
            "shared/core/module.toml",
            "add",
            "/resources/-",
            Some(&r),
        );
    }
    set
}

#[test]
fn i1_one_document_may_hold_a_thousand_includes_and_not_one_more() {
    let a = "profiles/machine-a.toml";
    let r = resolve(
        &set_with_gitconfig(&include_lines(1000), 0),
        a,
        &builtin_catalog(),
    )
    .unwrap();
    assert_eq!(r.includes[0].entries.len(), 1000);
    let e = resolve(
        &set_with_gitconfig(&include_lines(1001), 0),
        a,
        &builtin_catalog(),
    )
    .unwrap_err();
    assert_eq!(e.code, Code::LimitExceeded);
    assert_eq!(e.source, "shared/core/module.toml");
    assert!(
        e.message.contains("more than 1000 include directives"),
        "{e}"
    );
}

#[test]
fn i1_the_total_over_all_links_is_capped_at_ten_thousand() {
    let a = "profiles/machine-a.toml";
    // core.git plus nine more links to the same 1000-line document: exactly 10000.
    let r = resolve(
        &set_with_gitconfig(&include_lines(1000), 9),
        a,
        &builtin_catalog(),
    )
    .unwrap();
    assert_eq!(
        r.includes.iter().map(|g| g.entries.len()).sum::<usize>(),
        10_000
    );
    let e = resolve(
        &set_with_gitconfig(&include_lines(1000), 10),
        a,
        &builtin_catalog(),
    )
    .unwrap_err();
    assert_eq!(e.code, Code::LimitExceeded);
    assert!(
        e.message.contains("more than 10000 include directives"),
        "{e}"
    );
}

static PARSES: AtomicUsize = AtomicUsize::new(0);

fn counting_parser(text: &str) -> Vec<Include> {
    PARSES.fetch_add(1, Ordering::SeqCst);
    text.lines()
        .map(|l| Include {
            path: l.to_string(),
            condition: None,
        })
        .collect()
}

#[test]
fn i1_each_document_is_parsed_once_however_many_resources_link_it() {
    let link = AdapterSpec {
        id: "fs-link/v1".into(),
        kind: "link",
        target: TargetRule::Required,
        secret_values: false,
        sensitive: false,
        native_document: false,
        claims: Claims::Target,
        value: Schema::Table(vec![Field {
            name: "source",
            schema: Schema::Source,
            required: true,
        }]),
        source_includes: Some(counting_parser),
        precedence: None,
    };
    let cat = AdapterCatalog::new([link]);
    let res = |i: usize, file: &str| {
        format!(
            "[[resources]]\nid = \"l{i}\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \"t/l{i}\"\n[resources.value]\nsource = \"files/{file}\"\n"
        )
    };
    let body: String = (0..6)
        .map(|i| res(i, if i < 4 { "one" } else { "two" }))
        .collect();
    let mut set = build(&[module("lnk", &[], &body)], &["lnk".into()]);
    set.insert_file("b/m/files/one", "x\ny\n");
    set.insert_file("b/m/files/two", "z\n");
    PARSES.store(0, Ordering::SeqCst);
    let r = resolve(&set, "p/profile.toml", &cat).unwrap();
    assert_eq!(PARSES.load(Ordering::SeqCst), 2, "six links, two documents");
    assert_eq!(r.includes.len(), 6, "every link still gets its own group");
}

#[test]
fn i1_explain_output_stays_bounded_when_a_fragment_is_included_a_thousand_times() {
    // Every clipped field is at its 200 character limit and the IDs are 60 characters long, so a
    // 1000-position report is over 1 MiB. The writer must stop at the limit, not after it.
    let long = |c: char, n: usize| c.to_string().repeat(n);
    let (base_id, frag_id, frag_mod) = (long('g', 60), long('f', 60), long('m', 60));
    let frag_target = format!(".config/{}", long('t', 250));
    let mut gitconfig = String::new();
    for i in 0..1000 {
        gitconfig.push_str(&format!(
            "[includeIf \"gitdir:{}{i:04}\"]\n    path = ~/{frag_target}\n",
            long('c', 250)
        ));
    }
    let core = format!(
        "schema = 1\nid = \"core\"\nplatforms = [\"macos\"]\nrequires = []\n[[resources]]\nid = \"{base_id}\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \".config/{}\"\n[resources.value]\nsource = \"files/gitconfig\"\n",
        long('g', 250)
    );
    let frag = format!(
        "[[resources]]\nid = \"{frag_id}\"\nkind = \"render\"\nadapter = \"git-config/v1\"\ntarget = \"{frag_target}\"\n[resources.value.a]\nk = 1\n"
    );
    let mut set = build(
        &[module(&frag_mod, &[], &frag)],
        std::slice::from_ref(&frag_mod),
    );
    set.insert_file("b/m/core.toml", core);
    set.insert_file("b/m/files/gitconfig", gitconfig);
    let r = run(&set).unwrap();
    assert_eq!(r.includes[0].entries.len(), 1000);
    let id = format!("{frag_mod}.{frag_id}");
    let e = explain(&r, &id).unwrap_err();
    assert_eq!(
        e.code,
        Code::LimitExceeded,
        "a 1000-position report is over 1 MiB and is refused"
    );
    // The base lists its includes once and fits.
    let text = explain(&r, &format!("core.{base_id}")).unwrap();
    assert!(text.len() < (1 << 20) + 4096, "{}", text.len());
    // A short fragment list is fine.
    let mut few = set.clone();
    few.insert_file(
        "b/m/files/gitconfig",
        format!("[include]\n    path = ~/{frag_target}\n"),
    );
    assert!(explain(&run(&few).unwrap(), &id).is_ok());
}

#[test]
fn l3_the_module_cap_counts_the_core_and_pins_its_boundary() {
    // At most 10000 modules including the core: 9999 others resolve, 10000 others are refused.
    let ids: Vec<String> = (0..9_999).map(|i| format!("m{i:05}")).collect();
    let modules: Vec<_> = ids.iter().map(|id| module(id, &[], "")).collect();
    let r = run(&build(&modules, &ids)).unwrap();
    assert_eq!(r.modules.len(), 10_000);
    let ids: Vec<String> = (0..10_000).map(|i| format!("m{i:05}")).collect();
    let modules: Vec<_> = ids.iter().map(|id| module(id, &[], "")).collect();
    let e = run(&build(&modules, &ids)).unwrap_err();
    assert_eq!(e.code, Code::LimitExceeded);
    assert!(e.message.contains("more than 10000 modules"), "{e}");
}

// Claims, supplier lookup and source hashing scale with their inputs, not their products.

/// A `skills/v1` resource named `id` with `roots` target roots and `entries` entries, each
/// entry pointing at `source`.
fn skills(id: &str, roots: usize, entries: usize, source: &str) -> String {
    let mut s = format!(
        "[[resources]]\nid = \"{id}\"\nkind = \"distribution\"\nadapter = \"skills/v1\"\n[resources.value]\ntarget_roots = ["
    );
    for r in 0..roots {
        let _ = write!(s, "\"d{id}/r{r:05}\", ");
    }
    s.push_str("]\n");
    for e in 0..entries {
        let _ = write!(
            s,
            "[resources.value.entries.e{e:05}]\nsource = \"{source}\"\nenabled = true\n"
        );
    }
    s
}

#[test]
fn s1_the_claim_product_is_refused_before_any_claim_is_built() {
    // 6000 roots x 3000 entries is 1.8e7 claims. It is refused from the two lengths alone.
    let m = module("many", &[], &skills("sk", 6000, 3000, "files/f"));
    let set = build(&[m], &["many".into()]);
    let t = Instant::now();
    let e = run(&set).unwrap_err();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert_eq!(e.code, Code::LimitExceeded);
    assert_eq!(e.resource.as_deref(), Some("many.sk"));
    assert!(e.message.contains("too many output paths"), "{e}");
}

#[test]
fn s1_the_claim_caps_are_pinned_per_resource_and_in_total() {
    let with_file = |modules: &[(String, String)], selected: &[String]| {
        let mut set = build(modules, selected);
        set.insert_file("b/m/files/f", "x");
        set
    };
    // Exactly 100000 claims in one resource resolves. One more root is refused.
    let m = module("one", &[], &skills("a", 100, 1000, "files/f"));
    let r = run(&with_file(&[m], &["one".into()])).unwrap();
    assert_eq!(r.resources["one.a"].claims.len(), 100_000);
    let m = module("one", &[], &skills("a", 101, 1000, "files/f"));
    let e = run(&with_file(&[m], &["one".into()])).unwrap_err();
    assert_eq!(
        (e.code, e.resource.as_deref()),
        (Code::LimitExceeded, Some("one.a"))
    );
    // Two resources at the cap total 200000 and resolve. A third pushes the total over.
    let two = format!(
        "{}{}",
        skills("a", 100, 1000, "files/f"),
        skills("b", 100, 1000, "files/f")
    );
    let r = run(&with_file(&[module("two", &[], &two)], &["two".into()])).unwrap();
    assert_eq!(
        r.resources.values().map(|r| r.claims.len()).sum::<usize>(),
        200_000
    );
    let three = format!("{two}{}", skills("c", 1, 1, "files/f"));
    let e = run(&with_file(&[module("two", &[], &three)], &["two".into()])).unwrap_err();
    assert_eq!(
        (e.code, e.resource.as_deref()),
        (Code::LimitExceeded, Some("two.c"))
    );
}

#[test]
fn s2_supplier_lookup_scales_with_thirty_thousand_values_and_twelve_thousand_overrides() {
    let mut owner = String::from(
        "[[resources]]\nid = \"big\"\nkind = \"render\"\nadapter = \"git-config/v1\"\ntarget = \"t/big\"\n[resources.value.k]\n",
    );
    for i in 0..30_000 {
        let _ = writeln!(owner, "k{i:05} = 1");
    }
    let defaults: Vec<String> = (0..12_000).map(|i| format!("\"/k/k{i:05}\"")).collect();
    let _ = write!(
        owner,
        "[resources.policy]\ndefaults = [{}]\n",
        defaults.join(", ")
    );
    let mut modules = vec![module("own", &[], &owner)];
    for part in 0..3 {
        let mut body = String::new();
        for i in (part * 4000)..((part + 1) * 4000) {
            let _ = write!(
                body,
                "[[overrides]]\nresource = \"own.big\"\npath = \"/k/k{i:05}\"\nvalue = 2\nreason = \"r\"\n"
            );
        }
        modules.push(module(&format!("ovr{part}"), &[], &body));
    }
    let ids: Vec<String> = modules.iter().map(|m| m.0.clone()).collect();
    let t = Instant::now();
    let r = run(&build(&modules, &ids)).unwrap();
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    let big = &r.resources["own.big"];
    assert_eq!(big.provenance.len(), 30_000);
    assert_eq!(big.value["k"]["k11999"].as_integer(), Some(2));
    assert_eq!(big.value["k"]["k12000"].as_integer(), Some(1));
    assert_eq!(big.provenance["/k/k11999"].changer.as_deref(), Some("ovr2"));
    assert_eq!(big.provenance["/k/k12000"].changer, None);
}

#[test]
fn s3_a_directory_reached_by_three_thousand_references_is_hashed_once() {
    let m = module("dirs", &[], &skills("sk", 1, 3000, "files/dir"));
    let mut set = build(&[m], &["dirs".into()]);
    for i in 0..3000 {
        set.insert_file(format!("b/m/files/dir/f{i:04}"), format!("file {i}"));
    }
    let t = Instant::now();
    let r = run(&set).unwrap();
    assert!(t.elapsed() < Duration::from_secs(20), "{:?}", t.elapsed());
    let ids: std::collections::BTreeSet<_> = r.resources["dirs.sk"]
        .provenance
        .iter()
        .filter(|(p, _)| p.ends_with("/source"))
        .map(|(_, p)| p.content_id.clone().unwrap())
        .collect();
    assert_eq!(ids.len(), 1, "every reference sees the same digest");
}

#[test]
fn s3_a_large_file_reached_by_many_references_is_hashed_once() {
    // 9000 links in three modules (a document is limited to 1 MiB), all naming one 1 MiB file.
    let body = |m: usize| {
        let mut s = String::new();
        for i in 0..3000 {
            let _ = write!(
                s,
                "[[resources]]\nid = \"l{i:05}\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \"t{m}/l{i:05}\"\n[resources.value]\nsource = \"files/big\"\n"
            );
        }
        s
    };
    let ids: Vec<String> = (0..3).map(|m| format!("lnk{m}")).collect();
    let modules: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(m, id)| module(id, &[], &body(m)))
        .collect();
    let mut set = build(&modules, &ids);
    set.insert_file("b/m/files/big", vec![b'x'; 1 << 20]);
    let t = Instant::now();
    let r = run(&set).unwrap();
    assert!(t.elapsed() < Duration::from_secs(6), "{:?}", t.elapsed());
    assert_eq!(r.resources.len(), 9000);
}

#[test]
fn s3_the_digest_cache_does_not_let_a_narrower_root_skip_the_containment_check() {
    // owner.thing reaches the directory from the owner's root, where the link inside it is
    // fine. owner.thing2 is overridden by the `sub` module, whose root is narrower, and the
    // same link leaves that root. The first reference must not vouch for the second.
    let file = |id: &str, requires: &str, body: &str| {
        format!(
            "schema = 1\nid = \"{id}\"\nplatforms = [\"macos\"]\nrequires = [{requires}]\n{body}"
        )
    };
    let thing = |id: &str| {
        format!(
            "[[resources]]\nid = \"{id}\"\nkind = \"link\"\nadapter = \"fs-link/v1\"\ntarget = \"t/{id}\"\n[resources.value]\nsource = \"sub/files/d\"\n[resources.policy]\ndefaults = [\"/source\"]\n"
        )
    };
    let mut set = SourceSet::new();
    set.insert_file("b/bundle.toml", "schema = 1\nid = \"gen\"\ncore = \"m/core.toml\"\n[modules]\nowner = \"m/owner/module.toml\"\nsub = \"m/owner/sub/module.toml\"\n");
    set.insert_file("b/m/core.toml", file("core", "", ""));
    set.insert_file(
        "b/m/owner/module.toml",
        file(
            "owner",
            "",
            &format!("{}{}", thing("thing"), thing("thing2")),
        ),
    );
    set.insert_file(
        "b/m/owner/sub/module.toml",
        file("sub", "\"owner\"", "[[overrides]]\nresource = \"owner.thing2\"\npath = \"/source\"\nvalue = \"files/d\"\nreason = \"r\"\n"),
    );
    set.insert_file("b/m/owner/sub/files/d/ok", "ok");
    set.insert_link("b/m/owner/sub/files/d/link", "../../../sibling");
    set.insert_file("b/m/owner/sibling", "inside the owner root, outside sub");
    set.insert_file(
        "p/profile.toml",
        "schema = 1\nid = \"gen-01\"\nbundle = \"../b/bundle.toml\"\nplatform = \"macos\"\narchitecture = \"arm64\"\nmodules = [\"sub\"]\n",
    );
    let e = run(&set).unwrap_err();
    assert_eq!(e.code, Code::SourceEscape, "{e}");
    assert_eq!(e.source, "b/m/owner/sub/module.toml");
    assert_eq!(e.resource.as_deref(), Some("owner.thing2"));
    // Without the narrower reference the same tree resolves, so the cache is what was tested.
    let mut ok = set.clone();
    ok.insert_file("b/m/owner/sub/module.toml", file("sub", "\"owner\"", ""));
    assert!(run(&ok).is_ok());
}
