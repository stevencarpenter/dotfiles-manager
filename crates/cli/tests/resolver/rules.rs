//! Authority, containment and error-content rules (DFM-15 C02, C03, C04, C09, C10, C11),
//! each exercised on an isolated in-memory copy of the synthetic fixtures.

use crate::harness::{edit, examples, synthetic, val};
use dfm_cli::builtin_catalog;
use dfm_model::Code;
use dfm_model::resolve::{ResolveError, Resolved, SourceSet, resolve};
use sha2::{Digest, Sha256};

const A: &str = "profiles/machine-a.toml";
const B: &str = "profiles/machine-b.toml";
const PRIVATE: &str = "private/private-mod/module.toml";
const REUSABLE: &str = "shared/reusable/module.toml";

fn run(set: &SourceSet, profile: &str) -> Result<Resolved, ResolveError> {
    resolve(set, profile, &builtin_catalog())
}

fn ok(set: &SourceSet, profile: &str) -> Resolved {
    run(set, profile).unwrap_or_else(|e| panic!("{profile}: {e}"))
}

fn err(set: &SourceSet, profile: &str) -> ResolveError {
    match run(set, profile) {
        Err(e) => e,
        Ok(_) => panic!("{profile} resolved but should have been refused"),
    }
}

fn sha(bytes: &[u8]) -> String {
    format!("sha256:{}", dfm_model::digest::sha256_hex(bytes))
}

fn add_override(set: &mut SourceSet, profile: &str, resource: &str, path: &str, lit: &str) {
    let o = format!(
        "{{ resource = \"{resource}\", path = \"{path}\", value = {lit}, reason = \"test\" }}"
    );
    edit(set, profile, "add", "/overrides/-", Some(&o));
}

#[test]
fn baseline_fixtures_resolve() {
    let set = synthetic();
    assert_eq!(ok(&set, A).modules, ["core", "reusable"]);
    assert_eq!(ok(&set, B).modules, ["core", "reusable", "private-mod"]);
}

// C02: one mandatory core, single bundle ancestry, unique IDs.

#[test]
fn c02_the_core_is_mandatory_and_named_core() {
    let mut set = synthetic();
    edit(&mut set, "shared/bundle.toml", "remove", "/core", None);
    assert_eq!(err(&set, A).code, Code::ManifestInvalid);

    let mut set = synthetic();
    edit(
        &mut set,
        "shared/core/module.toml",
        "replace",
        "/id",
        Some("\"core-two\""),
    );
    assert_eq!(err(&set, A).code, Code::ReferenceInvalid);

    let mut set = synthetic();
    edit(
        &mut set,
        "private/bundle.toml",
        "add",
        "/modules/core",
        Some("\"private-mod/module.toml\""),
    );
    assert_eq!(err(&set, B).code, Code::CoreReplacement);

    let mut set = synthetic();
    edit(
        &mut set,
        "shared/core/module.toml",
        "replace",
        "/requires",
        Some("[\"reusable\"]"),
    );
    assert_eq!(err(&set, A).code, Code::ManifestInvalid);
}

#[test]
fn c02_bundle_ancestry_is_one_acyclic_chain_without_redefinition() {
    let mut set = synthetic();
    edit(
        &mut set,
        "private/bundle.toml",
        "add",
        "/modules/reusable",
        Some("\"private-mod/module.toml\""),
    );
    let e = err(&set, B);
    assert_eq!(e.code, Code::ModuleDuplicate);
    assert_eq!(e.conflicts_with.as_deref(), Some("shared/bundle.toml"));

    let mut set = synthetic();
    edit(&mut set, "shared/bundle.toml", "remove", "/core", None);
    edit(
        &mut set,
        "shared/bundle.toml",
        "add",
        "/extends",
        Some("\"../private/bundle.toml\""),
    );
    assert_eq!(err(&set, B).code, Code::BundleCycle);

    let mut set = synthetic();
    edit(
        &mut set,
        "private/bundle.toml",
        "replace",
        "/id",
        Some("\"synthetic-shared\""),
    );
    assert_eq!(err(&set, B).code, Code::ManifestInvalid);
}

#[test]
fn c02_ids_are_unique_and_match_their_catalogue_key() {
    let mut set = synthetic();
    let dup = "{ id = \"tool-config\", kind = \"link\", adapter = \"fs-link/v1\", target = \"x\", value = { source = \"files/tool.conf\" } }";
    edit(&mut set, REUSABLE, "add", "/resources/-", Some(dup));
    assert_eq!(err(&set, A).code, Code::ManifestInvalid);

    let mut set = synthetic();
    edit(
        &mut set,
        "shared/bundle.toml",
        "replace",
        "/modules/reusable",
        Some("\"core/module.toml\""),
    );
    assert_eq!(err(&set, A).code, Code::ReferenceInvalid);

    let mut set = synthetic();
    edit(&mut set, REUSABLE, "replace", "/id", Some("\"Reusable\""));
    assert_eq!(err(&set, A).code, Code::ManifestInvalid);
}

#[test]
fn c02_dependencies_close_over_requires_and_selection_has_no_duplicates() {
    let set = synthetic();
    // machine-b selects only private-mod; reusable arrives through requires.
    assert_eq!(ok(&set, B).modules, ["core", "reusable", "private-mod"]);

    let mut set = synthetic();
    edit(&mut set, B, "add", "/modules/-", Some("\"reusable\""));
    assert_eq!(
        ok(&set, B).modules,
        ["core", "reusable", "private-mod"],
        "selecting a prerequisite is allowed"
    );

    let mut set = synthetic();
    edit(&mut set, B, "add", "/modules/-", Some("\"private-mod\""));
    assert_eq!(err(&set, B).code, Code::ModuleDuplicate);

    let mut set = synthetic();
    edit(&mut set, B, "add", "/modules/-", Some("\"core\""));
    assert_eq!(
        err(&set, B).code,
        Code::ModuleDuplicate,
        "core is always selected"
    );

    let mut set = synthetic();
    edit(
        &mut set,
        PRIVATE,
        "replace",
        "/requires",
        Some("[\"reusable\", \"reusable\"]"),
    );
    assert_eq!(err(&set, B).code, Code::ModuleDuplicate);

    let mut set = synthetic();
    edit(
        &mut set,
        PRIVATE,
        "replace",
        "/requires",
        Some("[\"ghost\"]"),
    );
    assert_eq!(err(&set, B).code, Code::ModuleUnknown);
}

#[test]
fn c02_platforms_and_cycles_are_rejected() {
    let mut set = synthetic();
    edit(&mut set, B, "replace", "/platform", Some("\"linux\""));
    assert_eq!(err(&set, B).code, Code::PlatformUnsupported);

    let mut set = synthetic();
    edit(&mut set, B, "replace", "/platform", Some("\"windows\""));
    assert_eq!(err(&set, B).code, Code::ManifestInvalid);

    let mut set = synthetic();
    edit(
        &mut set,
        REUSABLE,
        "replace",
        "/requires",
        Some("[\"private-mod\"]"),
    );
    let e = err(&set, B);
    assert_eq!(e.code, Code::DependencyCycle);
    assert!(e.message.contains("private-mod") && e.message.contains("reusable"));
}

#[test]
fn c02_overrides_follow_exact_bounded_path_and_type_rules() {
    let rows: &[(&str, &str, Code, Option<&str>)] = &[
        (
            "/tool/retries/x",
            "1",
            Code::OverrideForbidden,
            Some("integer"),
        ),
        (
            "/tool",
            "{ retries = 1 }",
            Code::OverrideForbidden,
            Some("table"),
        ),
        (
            "/tool/missing",
            "1",
            Code::OverrideForbidden,
            Some("integer"),
        ),
        (
            "/tool/paths/0",
            "\"x\"",
            Code::OverrideForbidden,
            Some("string"),
        ),
        ("/tool/retries", "\"5\"", Code::TypeMismatch, Some("string")),
        ("/tool/retries", "true", Code::TypeMismatch, Some("boolean")),
        ("/tool/retries", "1.5", Code::TypeMismatch, Some("float")),
        ("/tool/verbose", "1", Code::TypeMismatch, Some("integer")),
        ("/tool/paths", "\"a\"", Code::TypeMismatch, Some("string")),
        ("", "1", Code::ReferenceInvalid, None),
        ("tool/retries", "1", Code::ReferenceInvalid, None),
        ("/tool/*", "1", Code::ReferenceInvalid, None),
        ("/tool//retries", "1", Code::ReferenceInvalid, None),
    ];
    for (path, lit, code, category) in rows {
        let mut set = synthetic();
        add_override(&mut set, A, "reusable.tool-settings", path, lit);
        let e = err(&set, A);
        assert_eq!(e.code, *code, "{path} {lit}: {e}");
        if let Some(c) = category {
            assert_eq!(e.rejected, Some(*c), "{path} {lit}");
        }
    }
    // A valid replacement works and changes only that value.
    let mut set = synthetic();
    add_override(&mut set, A, "reusable.tool-settings", "/tool/retries", "9");
    let r = ok(&set, A);
    let v = &r.resources["reusable.tool-settings"].value;
    assert_eq!(v["tool"]["retries"], val("9"));
    assert_eq!(v["tool"]["timeout"], val("30"));
}

#[test]
fn c02_arrays_are_atomic() {
    let mut set = synthetic();
    add_override(
        &mut set,
        A,
        "reusable.tool-settings",
        "/tool/paths",
        "[\"z\"]",
    );
    let r = ok(&set, A);
    assert_eq!(
        r.resources["reusable.tool-settings"].value["tool"]["paths"],
        val("[\"z\"]")
    );
}

#[test]
fn c02_overrides_name_resources_in_the_selected_composition_and_give_a_reason() {
    for resource in ["private-mod.api-token", "ghost.thing", "nodot"] {
        let mut set = synthetic();
        add_override(&mut set, A, resource, "/x", "1");
        assert_eq!(err(&set, A).code, Code::ReferenceInvalid, "{resource}");
    }
    let mut set = synthetic();
    let o = "{ resource = \"core.editor-defaults\", path = \"/core/editor\", value = \"vi\", reason = \"  \" }";
    edit(&mut set, A, "add", "/overrides/-", Some(o));
    assert_eq!(err(&set, A).code, Code::ManifestInvalid);
}

#[test]
fn c02_contributions_need_an_exposed_map_a_namespaced_key_and_a_free_slot() {
    let contrib = |key: &str, path: &str| {
        format!(
            "{{ resource = \"reusable.agent-settings\", path = \"{path}\", key = \"{key}\", reason = \"t\", value = {{ event = \"E\", type = \"command\", argv = [\"x\"] }} }}"
        )
    };
    for (key, path, code) in [
        ("other.audit", "/handlers", Code::ExtensionNamespace),
        ("private-mod.", "/handlers", Code::ExtensionNamespace),
        ("private-mod.UPPER", "/handlers", Code::ExtensionNamespace),
        ("audit", "/handlers", Code::ExtensionNamespace),
        ("private-mod.x", "/managed", Code::OverrideForbidden),
        ("private-mod.audit", "/handlers", Code::ExtensionConflict),
    ] {
        let mut set = synthetic();
        edit(
            &mut set,
            PRIVATE,
            "add",
            "/contributions/-",
            Some(&contrib(key, path)),
        );
        let e = err(&set, B);
        assert_eq!(e.code, code, "{key} {path}: {e}");
    }
    // A key that the owner already declares cannot be replaced by a contribution.
    let mut set = synthetic();
    edit(
        &mut set,
        REUSABLE,
        "add",
        "/resources/3/value/handlers/private-mod.audit",
        Some("{ event = \"E\", type = \"command\", argv = [\"x\"] }"),
    );
    let e = err(&set, B);
    assert_eq!(e.code, Code::ExtensionConflict);
    assert_eq!(e.conflicts_with.as_deref(), Some("reusable"));
}

#[test]
fn c02_ordering_never_grants_override_authority() {
    // The same resolution whatever order modules, overrides or selection are written in.
    let base = ok(&synthetic(), B);
    let mut set = synthetic();
    edit(
        &mut set,
        B,
        "replace",
        "/modules",
        Some("[\"reusable\", \"private-mod\"]"),
    );
    assert_eq!(ok(&set, B), base);
    let mut set = synthetic();
    edit(
        &mut set,
        PRIVATE,
        "replace",
        "/requires",
        Some("[\"reusable\"]"),
    );
    let o: toml::Value = toml::from_str(&crate::harness::text(&set, PRIVATE)).unwrap();
    let mut reversed = o.clone();
    reversed["overrides"].as_array_mut().unwrap().reverse();
    set.insert_file(PRIVATE, toml::to_string(&reversed).unwrap().into_bytes());
    assert_eq!(ok(&set, B), base);

    // Two overrides of one resource and path conflict, equal values included, and neither the
    // profile nor the module position decides.
    let mut by_value = Vec::new();
    for value in ["\"emacs\"", "\"vim\""] {
        let mut set = synthetic();
        add_override(&mut set, B, "core.editor-defaults", "/core/editor", value);
        let e = err(&set, B);
        assert_eq!(e.code, Code::OverrideConflict);
        by_value.push(e);
    }
    assert_eq!(by_value[0], by_value[1]);
    let mut set = synthetic();
    edit(
        &mut set,
        B,
        "replace",
        "/modules",
        Some("[\"private-mod\"]"),
    );
    add_override(
        &mut set,
        B,
        "core.editor-defaults",
        "/core/editor",
        "\"emacs\"",
    );
    assert_eq!(err(&set, B), by_value[0]);
}

#[test]
fn c02_a_dependency_confers_no_authority_over_its_parent() {
    // private-mod requires reusable. Neither it nor the core's dependents may touch a protected path.
    let mut set = synthetic();
    edit(
        &mut set,
        PRIVATE,
        "add",
        "/overrides/-",
        Some(
            "{ resource = \"reusable.tool-settings\", path = \"/tool\", value = { retries = 1 }, reason = \"t\" }",
        ),
    );
    assert_eq!(err(&set, B).code, Code::OverrideForbidden);
}

// C03: provenance survives replacement and disabling.

#[test]
fn c03_a_replaced_source_keeps_the_supplying_origin_and_reason() {
    let r = ok(&synthetic(), B);
    let banner = &r.resources["reusable.tool-banner"].provenance["/source"];
    assert_eq!(banner.owner, "reusable");
    assert_eq!(banner.changer.as_deref(), Some("private-mod"));
    assert_eq!(
        banner.change,
        Some(dfm_model::resolve::ChangeKind::Override)
    );
    assert_eq!(
        banner.reason.as_deref(),
        Some("Supply a private banner through the exposed source slot.")
    );
    assert_eq!(banner.original, Some(val("\"files/banner\"")));
    assert_eq!(banner.source_file, PRIVATE);
    // The same relative text names a different file because it is relative to the supplier.
    assert_eq!(
        banner.source_path.as_deref(),
        Some("private/private-mod/files/banner")
    );
    assert_eq!(
        banner.content_id.as_deref(),
        Some(sha(b"private banner\n").as_str())
    );
    let shared = &r.resources["reusable.tool-config"].provenance["/source"];
    assert_eq!(shared.changer, None);
    assert_eq!(
        shared.source_path.as_deref(),
        Some("shared/reusable/files/tool.conf")
    );
}

#[test]
fn c03_contributions_and_profile_overrides_have_their_own_origin() {
    let r = ok(&synthetic(), B);
    let h = &r.resources["reusable.agent-settings"].provenance["/handlers/private-mod.audit/event"];
    assert_eq!(h.change, Some(dfm_model::resolve::ChangeKind::Contribution));
    assert_eq!(h.changer.as_deref(), Some("private-mod"));
    assert_eq!(h.original, None);
    let p = &r.resources["reusable.tool-settings"].provenance["/tool/retries"];
    assert_eq!(p.changer.as_deref(), Some("machine-b"));
    assert_eq!(p.source_file, B);
    assert_eq!(p.original, Some(val("3")));
}

#[test]
fn c03_disabling_an_optional_entry_keeps_reason_origin_and_drops_its_claim() {
    let set = examples();
    let work = ok(&set, "profiles/work-01.toml");
    let skills = &work.resources["agents.skills"];
    let p = &skills.provenance["/entries/sample-audit/enabled"];
    assert_eq!(p.owner, "agents");
    assert_eq!(p.changer.as_deref(), Some("work"));
    assert_eq!(p.original, Some(val("true")));
    assert_eq!(p.source_file, "external/modules/work/module.toml");
    assert_eq!(
        p.reason.as_deref(),
        Some("The optional example skill is not selected for this machine.")
    );
    assert!(skills.claims.is_empty(), "{:?}", skills.claims);
    let personal = ok(&set, "profiles/personal-dev-01.toml");
    assert_eq!(personal.resources["agents.skills"].claims.len(), 2);
}

#[test]
fn c03_native_documents_are_marked_so_precedence_is_reported() {
    let r = ok(&synthetic(), A);
    assert!(r.resources["reusable.tool-settings"].native_document);
    assert!(r.resources["reusable.agent-settings"].native_document);
    assert!(!r.resources["reusable.tool-config"].native_document);
}

// C04: unknown keys, wrong kinds, unsafe targets, escaping links, unsupported adapters.

#[test]
fn c04_unknown_keys_are_refused_everywhere() {
    let rows: &[(&str, &str, &str, &str)] = &[
        (A, "profile", "/bogus", A),
        (
            "shared/bundle.toml",
            "bundle",
            "/bogus",
            "shared/bundle.toml",
        ),
        (REUSABLE, "module", "/bogus", REUSABLE),
        (REUSABLE, "resource", "/resources/0/bogus", REUSABLE),
        (REUSABLE, "policy", "/resources/0/policy/bogus", REUSABLE),
        (
            REUSABLE,
            "adapter value",
            "/resources/0/value/bogus",
            REUSABLE,
        ),
        (B, "override", "/overrides/0/bogus", B),
        (PRIVATE, "contribution", "/contributions/0/bogus", PRIVATE),
    ];
    for (file, what, path, source) in rows {
        let mut set = synthetic();
        edit(&mut set, file, "add", path, Some("\"v\""));
        let profile = if *file == A { A } else { B };
        let e = err(&set, profile);
        assert_eq!(e.code, Code::FieldUnknown, "{what}: {e}");
        assert_eq!(e.source, *source, "{what}");
        assert!(
            e.field.as_deref().is_some_and(|f| f.ends_with("/bogus")),
            "{what}: {e}"
        );
        assert_eq!(e.rejected, Some("string"), "{what}");
    }
}

#[test]
fn c04_references_to_the_wrong_kind_of_document_are_refused() {
    let mut set = synthetic();
    edit(
        &mut set,
        "shared/bundle.toml",
        "replace",
        "/core",
        Some("\"bundle.toml\""),
    );
    assert_eq!(err(&set, A).code, Code::ReferenceInvalid);
    let mut set = synthetic();
    edit(
        &mut set,
        "private/bundle.toml",
        "replace",
        "/extends",
        Some("\"../shared/reusable/module.toml\""),
    );
    assert_eq!(err(&set, B).code, Code::ReferenceInvalid);
    let mut set = synthetic();
    edit(
        &mut set,
        A,
        "replace",
        "/bundle",
        Some("\"../shared/reusable/module.toml\""),
    );
    assert_eq!(err(&set, A).code, Code::ReferenceInvalid);
    let mut set = synthetic();
    edit(
        &mut set,
        "shared/bundle.toml",
        "replace",
        "/modules/reusable",
        Some("\"bundle.toml\""),
    );
    assert_eq!(err(&set, A).code, Code::ReferenceInvalid);
}

#[test]
fn c04_unsafe_targets_are_refused() {
    for target in [
        "/etc/x",
        "../x",
        "a/../b",
        "a//b",
        "a/./b",
        "~/x",
        "a\\u0000b",
        "a\\nb",
        "",
    ] {
        let mut set = synthetic();
        edit(
            &mut set,
            REUSABLE,
            "replace",
            "/resources/0/target",
            Some(&format!("\"{target}\"")),
        );
        let e = err(&set, A);
        assert_eq!(e.code, Code::TargetInvalid, "{target:?}: {e}");
        assert_eq!(e.resource.as_deref(), Some("reusable.tool-config"));
    }
    let mut set = examples();
    edit(
        &mut set,
        "base/modules/agents/module.toml",
        "replace",
        "/resources/2/value/target_roots/0",
        Some("\"../x\""),
    );
    assert_eq!(
        err(&set, "profiles/personal-dev-01.toml").code,
        Code::TargetInvalid
    );
}

#[test]
fn c04_source_links_may_not_leave_their_root() {
    let tool = "shared/reusable/files/tool.conf";
    let cases = [
        (
            "../../../private/private-mod/files/banner",
            Some(Code::SourceEscape),
        ),
        ("/etc/hosts", Some(Code::SourceEscape)),
        ("../../../profiles/machine-a.toml", Some(Code::SourceEscape)),
        ("nowhere", Some(Code::SourceMissing)),
        ("tool.conf", Some(Code::SourceEscape)),
        ("banner", None),
    ];
    for (target, want) in cases {
        let mut set = synthetic();
        set.insert_link(tool, target);
        match (want, run(&set, A)) {
            (Some(code), Err(e)) => assert_eq!(e.code, code, "{target}: {e}"),
            (None, Ok(r)) => assert_eq!(
                r.resources["reusable.tool-config"].provenance["/source"]
                    .source_path
                    .as_deref(),
                Some("shared/reusable/files/banner")
            ),
            (w, g) => panic!("{target}: wanted {w:?}, got {g:?}"),
        }
    }
    // A link inside a directory source is checked too.
    let mut set = examples();
    set.insert_link(
        "base/modules/agents/files/sample-audit/escape",
        "../../../core/files/gitconfig",
    );
    let e = err(&set, "profiles/personal-dev-01.toml");
    assert_eq!(e.code, Code::SourceEscape);
    assert_eq!(e.resource.as_deref(), Some("agents.skills"));
}

#[test]
fn c04_unsupported_adapters_are_refused() {
    for (path, lit) in [
        ("/resources/0/adapter", "\"fs-link/v2\""),
        ("/resources/0/adapter", "\"nope/v1\""),
        ("/resources/0/kind", "\"render\""),
    ] {
        let mut set = synthetic();
        edit(&mut set, REUSABLE, "replace", path, Some(lit));
        assert_eq!(err(&set, A).code, Code::AdapterUnknown, "{path} {lit}");
    }
}

#[test]
fn c04_boolean_and_integer_are_distinct_in_adapter_values() {
    let mut set = synthetic();
    edit(
        &mut set,
        PRIVATE,
        "replace",
        "/resources/0/value/mode",
        Some("true"),
    );
    let e = err(&set, B);
    assert_eq!(e.code, Code::TypeMismatch);
    assert_eq!(e.rejected, Some("boolean"));
}

#[test]
fn c04_profile_overrides_are_optional() {
    let with = ok(&synthetic(), A);
    let mut set = synthetic();
    edit(&mut set, A, "remove", "/overrides", None);
    assert_eq!(ok(&set, A), with);
}

#[test]
fn c04_profile_sources_stay_inside_the_profile_directory() {
    let mut set = synthetic();
    add_override(
        &mut set,
        A,
        "reusable.tool-banner",
        "/source",
        "\"../private/private-mod/files/banner\"",
    );
    assert_eq!(err(&set, A).code, Code::SourceEscape);
    let mut set = synthetic();
    set.insert_file("profiles/files/banner", "profile banner\n");
    add_override(
        &mut set,
        A,
        "reusable.tool-banner",
        "/source",
        "\"files/banner\"",
    );
    let p = &ok(&set, A).resources["reusable.tool-banner"].provenance["/source"];
    assert_eq!(p.changer.as_deref(), Some("machine-a"));
    assert_eq!(p.source_path.as_deref(), Some("profiles/files/banner"));
}

// C09: invariants and competing owners.

#[test]
fn c09_invariants_cannot_be_overridden() {
    let mut set = synthetic();
    edit(
        &mut set,
        PRIVATE,
        "add",
        "/overrides/-",
        Some(
            "{ resource = \"core.launcher\", path = \"/source\", value = \"files/x\", reason = \"t\" }",
        ),
    );
    let e = err(&set, B);
    assert_eq!(e.code, Code::OverrideForbidden);
    assert_eq!(e.resource.as_deref(), Some("core.launcher"));
    assert_eq!(e.conflicts_with.as_deref(), Some("core"));
    // Identity fields are not part of `value`, so no pointer can reach them.
    for path in ["/target", "/adapter", "/kind", "/id", "/policy/defaults"] {
        let mut set = synthetic();
        add_override(&mut set, A, "core.editor-defaults", path, "\"x\"");
        assert_eq!(err(&set, A).code, Code::OverrideForbidden, "{path}");
    }
}

#[test]
fn c09_competing_owners_are_refused() {
    // Overrides: module against profile.
    let mut set = synthetic();
    add_override(
        &mut set,
        B,
        "core.editor-defaults",
        "/core/editor",
        "\"vim\"",
    );
    let e = err(&set, B);
    assert_eq!(e.code, Code::OverrideConflict);
    // The error names the declaring source of the later change and the other owner.
    assert_eq!(e.source, PRIVATE);
    assert_eq!(
        e.conflicts_with.as_deref(),
        Some("machine-b (profiles/machine-b.toml)")
    );
    // Outputs: case-insensitive equal, and file against directory.
    for target in [
        ".config/synthetic/TOOL.conf",
        ".config/synthetic/tool.conf/child",
        ".config",
    ] {
        let mut set = synthetic();
        let r = format!(
            "{{ id = \"extra\", kind = \"link\", adapter = \"fs-link/v1\", target = \"{target}\", value = {{ source = \"files/banner\" }} }}"
        );
        edit(&mut set, PRIVATE, "add", "/resources/-", Some(&r));
        let e = err(&set, B);
        assert_eq!(e.code, Code::TargetCollision, "{target}: {e}");
        assert!(e.conflicts_with.is_some());
    }
    // A distribution leaf against a link.
    let mut set = examples();
    let r = "{ id = \"clash\", kind = \"link\", adapter = \"fs-link/v1\", target = \".claude/skills/sample-audit\", value = { source = \"files/personal.env.tpl\" } }";
    edit(
        &mut set,
        "base/modules/personal-dev/module.toml",
        "add",
        "/resources/-",
        Some(r),
    );
    let e = err(&set, "profiles/personal-dev-01.toml");
    assert_eq!(e.code, Code::TargetCollision);
    assert!(
        e.conflicts_with
            .as_deref()
            .is_some_and(|o| o == "agents.skills" || o == "personal-dev.clash")
    );
    // Two skill entries that differ only by case.
    let mut set = examples();
    edit(
        &mut set,
        "base/modules/agents/module.toml",
        "add",
        "/resources/2/value/entries/Sample-Audit",
        Some("{ source = \"files/sample-audit\", enabled = true }"),
    );
    assert_eq!(
        err(&set, "profiles/personal-dev-01.toml").code,
        Code::TargetCollision
    );
}

// C10 and C11: what an error says.

#[test]
fn c10_a_forbidden_override_names_source_resource_field_category_owner_and_fix() {
    let mut set = synthetic();
    add_override(&mut set, B, "core.launcher", "/source", "\"files/x\"");
    let e = err(&set, B);
    assert_eq!(e.code, Code::OverrideForbidden);
    assert_eq!(e.source, B);
    assert_eq!(e.resource.as_deref(), Some("core.launcher"));
    assert_eq!(e.field.as_deref(), Some("/source"));
    assert_eq!(e.rejected, Some("string"));
    assert_eq!(e.conflicts_with.as_deref(), Some("core"));
    assert!(e.action.contains("core.launcher"));
    let shown = e.to_string();
    for part in [
        B,
        "core.launcher",
        "/source",
        "rejected value: string",
        "conflicts with: core",
        "Fix:",
    ] {
        assert!(shown.contains(part), "{part} missing from {shown}");
    }
    assert!(
        !shown.contains("files/x"),
        "the rejected value must not be copied: {shown}"
    );
}

#[test]
fn c10_secret_bearing_values_never_appear_in_errors() {
    const PLANTED: &str = "PLANTED-SECRET-VALUE-9f3a";
    let token = "private-mod.api-token";
    // Protected path, wrong type, unknown key and a bad target all carry the planted value.
    let attempts: Vec<(&str, &str, String)> = vec![
        ("override", "/account", format!("\"{PLANTED}\"")),
        ("override", "/mode", format!("\"{PLANTED}\"")),
    ];
    for (_, path, lit) in attempts {
        let mut set = synthetic();
        edit(
            &mut set,
            PRIVATE,
            "add",
            "/overrides/-",
            Some(&format!(
                "{{ resource = \"{token}\", path = \"{path}\", value = {lit}, reason = \"t\" }}"
            )),
        );
        let e = err(&set, B);
        let shown = format!("{e} {e:?} {}", dfm_model::Refusal::from(e.clone()));
        assert!(!shown.contains(PLANTED), "{shown}");
    }
    let mut set = synthetic();
    edit(
        &mut set,
        PRIVATE,
        "replace",
        "/resources/0/value/account",
        Some(&format!("\"{PLANTED}\"")),
    );
    edit(
        &mut set,
        PRIVATE,
        "add",
        "/resources/0/value/extra",
        Some(&format!("\"{PLANTED}\"")),
    );
    let e = err(&set, B);
    assert_eq!(e.code, Code::FieldUnknown);
    assert!(!format!("{e} {e:?}").contains(PLANTED));
    let mut set = synthetic();
    edit(
        &mut set,
        PRIVATE,
        "replace",
        "/resources/0/target",
        Some(&format!("\"../{PLANTED}\"")),
    );
    let e = err(&set, B);
    assert_eq!(e.code, Code::TargetInvalid);
    assert!(!format!("{e} {e:?}").contains(PLANTED));
    // Secret resources carry no content identity: not even a hash of the template.
    let r = ok(&synthetic(), B);
    let t = &r.resources[token];
    assert!(t.secret_values);
    assert!(t.provenance.values().all(|p| p.content_id.is_none()));
}

#[test]
fn c11_one_unknown_field() {
    let mut set = synthetic();
    edit(
        &mut set,
        REUSABLE,
        "add",
        "/resources/1/value/colour",
        Some("\"blue\""),
    );
    let e = err(&set, A);
    assert_eq!(e.code, Code::FieldUnknown);
    assert_eq!(e.code.as_str(), "E_FIELD_UNKNOWN");
    assert_eq!(e.source, REUSABLE);
    assert_eq!(e.resource.as_deref(), Some("reusable.tool-banner"));
    assert_eq!(e.field.as_deref(), Some("/colour"));
    assert_eq!(e.rejected, Some("string"));
    assert!(!e.action.is_empty());
}

#[test]
fn c11_one_conflicting_override() {
    let mut set = synthetic();
    add_override(
        &mut set,
        B,
        "core.editor-defaults",
        "/core/editor",
        "\"emacs\"",
    );
    let e = err(&set, B);
    assert_eq!(e.code.as_str(), "E_OVERRIDE_CONFLICT");
    assert_eq!(e.resource.as_deref(), Some("core.editor-defaults"));
    assert_eq!(e.field.as_deref(), Some("/core/editor"));
    assert_eq!(e.rejected, Some("string"));
    assert!(e.conflicts_with.is_some());
    assert!(!e.action.is_empty());
}

#[test]
fn hashing_helper_matches_the_digest_module() {
    assert_eq!(sha(b"x"), format!("sha256:{:x}", Sha256::digest(b"x")));
}

// Fragment directories are claimed as prefixes.

const PERSONAL: &str = "profiles/personal-dev-01.toml";
const PERSONAL_MOD: &str = "base/modules/personal-dev/module.toml";

fn link_at(set: &mut SourceSet, id: &str, target: &str) {
    let r = format!(
        "{{ id = \"{id}\", kind = \"link\", adapter = \"fs-link/v1\", target = \"{target}\", value = {{ source = \"files/personal.env.tpl\" }} }}"
    );
    edit(set, PERSONAL_MOD, "add", "/resources/-", Some(&r));
}

#[test]
fn m1_a_fragment_directory_accepts_only_matching_files_from_other_resources() {
    // work-01 places `40-work.md` in the assembler's directory, which is how a module supplies a fragment.
    ok(&examples(), "profiles/work-01.toml");
    let fragment_dir = ".codex/AGENTS.d";
    for (target, why) in [
        (".codex/AGENTS.d/notes.txt", "name does not match *.md"),
        (".codex/AGENTS.d/sub/x.md", "not a direct child"),
        (".codex/AGENTS.d", "the directory itself"),
        (".CODEX/agents.D/X.TXT", "case alias of a non-matching name"),
        (".codex/AGENTS.d/sub/deeper/x.md", "nested"),
    ] {
        let mut set = examples();
        link_at(&mut set, "extra", target);
        let e = err(&set, PERSONAL);
        assert_eq!(e.code, Code::TargetCollision, "{target}: {why}: {e}");
        assert_eq!(
            e.conflicts_with
                .as_deref()
                .map(|o| o == fragment_dir || o == "agents.codex-instructions"),
            Some(true),
            "{target}"
        );
    }
    // A matching direct child is a fragment, not a collision.
    let mut set = examples();
    link_at(&mut set, "extra", ".codex/AGENTS.d/50-extra.MD");
    ok(&set, PERSONAL);
}

#[test]
fn m1_fragment_directories_may_not_alias_or_nest() {
    let assemble = |id: &str, target: &str, dir: &str| {
        format!(
            "{{ id = \"{id}\", kind = \"assemble\", adapter = \"text-fragments/v1\", target = \"{target}\", value = {{ body = \"files/personal.env.tpl\", fragments_dir = \"{dir}\", pattern = \"*.md\", separator = \"\\n\" }} }}"
        )
    };
    for dir in [
        ".codex/AGENTS.d",
        ".Codex/agents.D",
        ".codex/AGENTS.d/inner",
        ".codex",
    ] {
        let mut set = examples();
        edit(
            &mut set,
            PERSONAL_MOD,
            "add",
            "/resources/-",
            Some(&assemble("twin", ".codex/OTHER.md", dir)),
        );
        let e = err(&set, PERSONAL);
        assert_eq!(e.code, Code::TargetCollision, "{dir}: {e}");
    }
    // The assembled file may not sit inside its own fragment directory.
    let mut set = examples();
    edit(
        &mut set,
        PERSONAL_MOD,
        "add",
        "/resources/-",
        Some(&assemble("self", ".config/fr/self.md", ".config/fr")),
    );
    assert_eq!(err(&set, PERSONAL).code, Code::TargetCollision);
    // A distinct directory is fine, and the pattern may not escape one component.
    let mut set = examples();
    edit(
        &mut set,
        PERSONAL_MOD,
        "add",
        "/resources/-",
        Some(&assemble("ok", ".config/ok.md", ".config/ok.d")),
    );
    ok(&set, PERSONAL);
    for bad in ["../*.md", "a/*.md", "[ab].md", "{a,b}.md", ""] {
        let mut set = examples();
        let r = assemble("badpat", ".config/p.md", ".config/p.d").replace("*.md", bad);
        edit(&mut set, PERSONAL_MOD, "add", "/resources/-", Some(&r));
        assert_eq!(err(&set, PERSONAL).code, Code::ManifestInvalid, "{bad:?}");
    }
}

// Native precedence is per value, from the authored include order.

#[test]
fn m2_includes_follow_the_order_written_in_the_authored_document() {
    let r = ok(&examples(), "profiles/work-01.toml");
    assert_eq!(r.includes.len(), 1);
    let g = &r.includes[0];
    assert_eq!(g.base, "core.git");
    assert_eq!(g.base_target.as_deref(), Some(".config/git/config"));
    assert_eq!(
        g.rule.as_deref(),
        Some("later include entries override earlier ones for single-valued keys")
    );
    let rows: Vec<_> = g
        .entries
        .iter()
        .map(|e| {
            (
                e.position,
                e.path.as_str(),
                e.condition.as_deref(),
                e.resource.as_deref(),
                e.module.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (
                1,
                "~/.config/external-overlays/git/base.inc",
                None,
                Some("core.git-defaults"),
                Some("core")
            ),
            (
                2,
                "~/.config/external-overlays/git/extra.inc",
                None,
                None,
                None
            ),
            (
                3,
                "~/.config/external-overlays/git/work.inc",
                Some("gitdir:~/work/"),
                Some("work.git-identity"),
                Some("work")
            ),
        ]
    );
    // A profile without the work module has no resource at position 3.
    let p = ok(&examples(), PERSONAL);
    assert_eq!(p.includes[0].entries[2].resource, None);
}

#[test]
fn m2_overlapping_fragments_are_listed_in_include_order() {
    let r = ok(&synthetic(), B);
    let g = &r.includes[0];
    let order: Vec<_> = g
        .entries
        .iter()
        .map(|e| (e.position, e.resource.as_deref()))
        .collect();
    assert_eq!(
        order,
        [
            (1, Some("core.editor-defaults")),
            (2, Some("private-mod.local-overrides")),
            (3, None)
        ]
    );
    assert_eq!(
        g.entries[1].condition.as_deref(),
        Some("gitdir:~/projects/")
    );
    // Both fragments set /core/editor, so the order decides. Only that pair overlaps.
    let a = &r.resources["core.editor-defaults"].provenance;
    let b = &r.resources["private-mod.local-overrides"].provenance;
    let shared: Vec<_> = a.keys().filter(|k| b.contains_key(*k)).collect();
    assert_eq!(shared, ["/core/editor"]);
    // The public machine has only the first fragment declared.
    let pa = ok(&synthetic(), A);
    assert_eq!(
        pa.includes[0]
            .entries
            .iter()
            .filter(|e| e.resource.is_some())
            .count(),
        1
    );
}

#[test]
fn m2_include_order_comes_from_the_document_not_from_declaration_order() {
    let mut set = synthetic();
    let swapped = "[include]\n    path = ~/.config/synthetic/local.inc\n[include]\n    path = ~/.config/synthetic/editor.inc\n";
    set.insert_file("shared/core/files/gitconfig", swapped);
    let r = ok(&set, B);
    let order: Vec<_> = r.includes[0]
        .entries
        .iter()
        .filter_map(|e| e.resource.as_deref())
        .collect();
    assert_eq!(
        order,
        ["private-mod.local-overrides", "core.editor-defaults"]
    );
}

// Looking up sources: `..` and case.

#[test]
fn l4_dotdot_needs_a_real_directory_and_case_aliases_do_not_hide_links() {
    for rel in [
        "files/nonexistent/../tool.conf",
        "files/tool.conf/../banner",
    ] {
        let mut set = synthetic();
        edit(
            &mut set,
            REUSABLE,
            "replace",
            "/resources/0/value/source",
            Some(&format!("\"{rel}\"")),
        );
        assert_eq!(err(&set, A).code, Code::SourceMissing, "{rel}");
    }
    // A real directory works.
    let mut set = synthetic();
    edit(
        &mut set,
        REUSABLE,
        "replace",
        "/resources/0/value/source",
        Some("\"files/../files/tool.conf\""),
    );
    ok(&set, A);
    // A link spelled `Link` is the same entry as `link` on a case-insensitive filesystem.
    let mut set = synthetic();
    set.insert_link(
        "shared/reusable/files/Link",
        "../../../private/private-mod/files/banner",
    );
    edit(
        &mut set,
        REUSABLE,
        "replace",
        "/resources/0/value/source",
        Some("\"files/link\""),
    );
    assert_eq!(err(&set, A).code, Code::SourceEscape);
    // Two spellings of one name cannot be told apart there.
    let mut set = synthetic();
    set.insert_file("shared/reusable/files/Tool.conf", "other");
    assert_eq!(err(&set, A).code, Code::SourceEscape);
}

#[test]
fn l5_bidi_and_zero_width_characters_are_refused_in_targets_and_escaped_in_messages() {
    for target in ["a\\u202eb", "a\\u200bb", "\\ufeffa", "a\\u2066b"] {
        let mut set = synthetic();
        edit(
            &mut set,
            REUSABLE,
            "replace",
            "/resources/0/target",
            Some(&format!("\"{target}\"")),
        );
        let e = err(&set, A);
        assert_eq!(e.code, Code::TargetInvalid, "{target}");
    }
    let mut set = synthetic();
    edit(&mut set, REUSABLE, "add", "/bogus\u{202e}x", Some("1"));
    let e = err(&set, A);
    let shown = e.to_string();
    assert!(
        shown.contains("\\u{202e}") && !shown.contains('\u{202e}'),
        "{shown}"
    );
}

#[test]
fn l6_secret_file_modes_are_bounded_and_private() {
    for (lit, ok_) in [
        ("384", true),
        ("256", true),
        ("0", true),
        ("-1", false),
        ("438", false),
        ("416", false),
        ("512", true),
        ("4096", false),
    ] {
        let mut set = synthetic();
        edit(
            &mut set,
            PRIVATE,
            "replace",
            "/resources/0/value/mode",
            Some(lit),
        );
        let got = run(&set, B);
        // 512 is 0o1000: out of range. 416 is 0o640 and 438 is 0o666: group or other bits.
        let want_ok = ok_ && lit != "512";
        assert_eq!(got.is_ok(), want_ok, "{lit}: {got:?}");
        if let Err(e) = got {
            assert_eq!(e.code, Code::ManifestInvalid, "{lit}");
            assert_eq!(e.rejected, Some("integer"));
            assert_eq!(e.resource.as_deref(), Some("private-mod.api-token"));
        }
    }
}
