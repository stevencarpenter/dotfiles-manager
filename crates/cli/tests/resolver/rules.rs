//! Authority, containment and error-content rules (DFM-15 C02, C03, C04, C09, C10, C11),
//! each exercised on an isolated in-memory copy of the synthetic fixtures. Every refusal is
//! checked for its code, declaring source, resource and a fragment of its message.

use crate::harness::{edit, examples, synthetic, val};
use dfm_cli::builtin_catalog;
use dfm_model::Code;
use dfm_model::resolve::{ResolveError, Resolved, SourceSet, resolve};
use sha2::{Digest, Sha256};

const A: &str = "profiles/machine-a.toml";
const B: &str = "profiles/machine-b.toml";
const PRIVATE: &str = "private/private-mod/module.toml";
const REUSABLE: &str = "shared/reusable/module.toml";
const SHARED_BUNDLE: &str = "shared/bundle.toml";
const PRIVATE_BUNDLE: &str = "private/bundle.toml";
const SETTINGS: &str = "reusable.tool-settings";
const TOKEN: &str = "private-mod.api-token";
const PERSONAL: &str = "profiles/personal-dev-01.toml";
const PERSONAL_MOD: &str = "base/modules/personal-dev/module.toml";
const AGENTS_MOD: &str = "base/modules/agents/module.toml";

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

/// Resolves, expects a refusal, and checks its code, declaring source, resource and a message
/// fragment. Returns the error for any further checks.
fn refused(
    set: &SourceSet,
    profile: &str,
    code: Code,
    source: &str,
    resource: Option<&str>,
    message: &str,
) -> ResolveError {
    let e = err(set, profile);
    assert_eq!(e.code, code, "code: {e}");
    assert_eq!(e.source, source, "declaring source: {e}");
    assert_eq!(e.resource.as_deref(), resource, "resource: {e}");
    assert!(e.message.contains(message), "{message:?} not in: {e}");
    e
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
    edit(&mut set, SHARED_BUNDLE, "remove", "/core", None);
    refused(
        &set,
        A,
        Code::ManifestInvalid,
        SHARED_BUNDLE,
        None,
        "needs `core` or `extends`",
    );

    let mut set = synthetic();
    edit(
        &mut set,
        "shared/core/module.toml",
        "replace",
        "/id",
        Some("\"core-two\""),
    );
    refused(
        &set,
        A,
        Code::ReferenceInvalid,
        SHARED_BUNDLE,
        None,
        "`core` must reference the core module",
    );

    let mut set = synthetic();
    edit(
        &mut set,
        PRIVATE_BUNDLE,
        "add",
        "/modules/core",
        Some("\"private-mod/module.toml\""),
    );
    refused(
        &set,
        B,
        Code::CoreReplacement,
        PRIVATE_BUNDLE,
        None,
        "cannot define a module named core",
    );

    let mut set = synthetic();
    edit(
        &mut set,
        "shared/core/module.toml",
        "replace",
        "/requires",
        Some("[\"reusable\"]"),
    );
    refused(
        &set,
        A,
        Code::ManifestInvalid,
        "shared/core/module.toml",
        None,
        "core cannot depend on other modules",
    );
}

#[test]
fn c02_bundle_ancestry_is_one_acyclic_chain_without_redefinition() {
    let mut set = synthetic();
    edit(
        &mut set,
        PRIVATE_BUNDLE,
        "add",
        "/modules/reusable",
        Some("\"private-mod/module.toml\""),
    );
    let e = refused(
        &set,
        B,
        Code::ModuleDuplicate,
        PRIVATE_BUNDLE,
        None,
        "already defined by an inherited bundle",
    );
    assert_eq!(e.conflicts_with.as_deref(), Some(SHARED_BUNDLE));

    let mut set = synthetic();
    edit(&mut set, SHARED_BUNDLE, "remove", "/core", None);
    edit(
        &mut set,
        SHARED_BUNDLE,
        "add",
        "/extends",
        Some("\"../private/bundle.toml\""),
    );
    refused(
        &set,
        B,
        Code::BundleCycle,
        SHARED_BUNDLE,
        None,
        "extend each other in a cycle",
    );

    let mut set = synthetic();
    edit(
        &mut set,
        PRIVATE_BUNDLE,
        "replace",
        "/id",
        Some("\"synthetic-shared\""),
    );
    let e = refused(
        &set,
        B,
        Code::ManifestInvalid,
        SHARED_BUNDLE,
        None,
        "share an ID",
    );
    assert_eq!(e.conflicts_with.as_deref(), Some(PRIVATE_BUNDLE));
}

#[test]
fn c02_ids_are_unique_and_match_their_catalogue_key() {
    let mut set = synthetic();
    let dup = "{ id = \"tool-config\", kind = \"link\", adapter = \"fs-link/v1\", target = \"x\", value = { source = \"files/tool.conf\" } }";
    edit(&mut set, REUSABLE, "add", "/resources/-", Some(dup));
    refused(
        &set,
        A,
        Code::ManifestInvalid,
        REUSABLE,
        Some("reusable.tool-config"),
        "duplicate resource ID",
    );

    let mut set = synthetic();
    edit(
        &mut set,
        SHARED_BUNDLE,
        "replace",
        "/modules/reusable",
        Some("\"core/module.toml\""),
    );
    refused(
        &set,
        A,
        Code::ReferenceInvalid,
        SHARED_BUNDLE,
        None,
        "references a module with a different ID",
    );

    let mut set = synthetic();
    edit(&mut set, REUSABLE, "replace", "/id", Some("\"Reusable\""));
    refused(
        &set,
        A,
        Code::ManifestInvalid,
        REUSABLE,
        None,
        "module id is invalid",
    );
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
    refused(
        &set,
        B,
        Code::ModuleDuplicate,
        B,
        None,
        "module `private-mod` is selected more than once",
    );

    let mut set = synthetic();
    edit(&mut set, B, "add", "/modules/-", Some("\"core\""));
    refused(
        &set,
        B,
        Code::ModuleDuplicate,
        B,
        None,
        "module `core` is selected more than once; core is always selected",
    );

    let mut set = synthetic();
    edit(
        &mut set,
        PRIVATE,
        "replace",
        "/requires",
        Some("[\"reusable\", \"reusable\"]"),
    );
    refused(
        &set,
        B,
        Code::ModuleDuplicate,
        PRIVATE,
        None,
        "`reusable` is required more than once",
    );

    let mut set = synthetic();
    edit(
        &mut set,
        PRIVATE,
        "replace",
        "/requires",
        Some("[\"ghost\"]"),
    );
    refused(
        &set,
        B,
        Code::ModuleUnknown,
        PRIVATE,
        None,
        "module `ghost` is not in the bundle catalogue",
    );
}

#[test]
fn c02_platforms_and_cycles_are_rejected() {
    let mut set = synthetic();
    edit(&mut set, B, "replace", "/platform", Some("\"linux\""));
    let e = refused(
        &set,
        B,
        Code::PlatformUnsupported,
        PRIVATE,
        None,
        "does not support the profile platform",
    );
    assert_eq!(e.conflicts_with.as_deref(), Some(B));

    let mut set = synthetic();
    edit(&mut set, B, "replace", "/platform", Some("\"windows\""));
    refused(
        &set,
        B,
        Code::ManifestInvalid,
        B,
        None,
        "platform must be `macos` or `linux`",
    );

    let mut set = synthetic();
    edit(
        &mut set,
        REUSABLE,
        "replace",
        "/requires",
        Some("[\"private-mod\"]"),
    );
    refused(
        &set,
        B,
        Code::DependencyCycle,
        PRIVATE,
        None,
        "cycle: private-mod, reusable",
    );
}

#[test]
fn c02_overrides_follow_exact_bounded_path_and_type_rules() {
    let rows: &[(&str, &str, Code, Option<&str>, &str)] = &[
        (
            "/tool/retries/x",
            "1",
            Code::OverrideForbidden,
            Some("integer"),
            "the path is protected",
        ),
        (
            "/tool",
            "{ retries = 1 }",
            Code::OverrideForbidden,
            Some("table"),
            "the path is protected",
        ),
        (
            "/tool/missing",
            "1",
            Code::OverrideForbidden,
            Some("integer"),
            "the path is protected",
        ),
        (
            "/tool/paths/0",
            "\"x\"",
            Code::OverrideForbidden,
            Some("string"),
            "the path is protected",
        ),
        (
            "/tool/retries",
            "\"5\"",
            Code::TypeMismatch,
            Some("string"),
            "must be of type integer",
        ),
        (
            "/tool/retries",
            "true",
            Code::TypeMismatch,
            Some("boolean"),
            "must be of type integer",
        ),
        (
            "/tool/retries",
            "1.5",
            Code::TypeMismatch,
            Some("float"),
            "must be of type integer",
        ),
        (
            "/tool/verbose",
            "1",
            Code::TypeMismatch,
            Some("integer"),
            "must be of type boolean",
        ),
        (
            "/tool/paths",
            "\"a\"",
            Code::TypeMismatch,
            Some("string"),
            "must be of type array",
        ),
        (
            "",
            "1",
            Code::ReferenceInvalid,
            None,
            "the root pointer is not allowed",
        ),
        (
            "tool/retries",
            "1",
            Code::ReferenceInvalid,
            None,
            "a pointer starts with /",
        ),
        (
            "/tool/*",
            "1",
            Code::ReferenceInvalid,
            None,
            "wildcards are not allowed",
        ),
        (
            "/tool//retries",
            "1",
            Code::ReferenceInvalid,
            None,
            "empty path segments are not allowed",
        ),
    ];
    for (path, lit, code, category, message) in rows {
        let mut set = synthetic();
        add_override(&mut set, A, SETTINGS, path, lit);
        let e = refused(&set, A, *code, A, Some(SETTINGS), message);
        assert_eq!(e.field.as_deref(), Some(*path), "{path} {lit}");
        if let Some(c) = category {
            assert_eq!(e.rejected, Some(*c), "{path} {lit}");
        }
    }
    // A valid replacement works and changes only that value.
    let mut set = synthetic();
    add_override(&mut set, A, SETTINGS, "/tool/retries", "9");
    let r = ok(&set, A);
    let v = &r.resources[SETTINGS].value;
    assert_eq!(v["tool"]["retries"], val("9"));
    assert_eq!(v["tool"]["timeout"], val("30"));
}

#[test]
fn c02_arrays_are_atomic() {
    let mut set = synthetic();
    add_override(&mut set, A, SETTINGS, "/tool/paths", "[\"z\"]");
    let r = ok(&set, A);
    assert_eq!(r.resources[SETTINGS].value["tool"]["paths"], val("[\"z\"]"));
}

#[test]
fn c02_overrides_name_resources_in_the_selected_composition_and_give_a_reason() {
    for resource in [TOKEN, "ghost.thing", "nodot"] {
        let mut set = synthetic();
        add_override(&mut set, A, resource, "/x", "1");
        let e = refused(
            &set,
            A,
            Code::ReferenceInvalid,
            A,
            Some(resource),
            "is not in the selected composition",
        );
        assert_eq!(e.field.as_deref(), Some("resource"));
    }
    let mut set = synthetic();
    let o = "{ resource = \"core.editor-defaults\", path = \"/core/editor\", value = \"vi\", reason = \"  \" }";
    edit(&mut set, A, "add", "/overrides/-", Some(o));
    let e = refused(
        &set,
        A,
        Code::ManifestInvalid,
        A,
        None,
        "needs a nonempty reason",
    );
    assert_eq!(e.field.as_deref(), Some("/overrides/0/reason"));
}

#[test]
fn c02_contributions_need_an_exposed_map_a_namespaced_key_and_a_free_slot() {
    let contrib = |key: &str, path: &str| {
        format!(
            "{{ resource = \"reusable.agent-settings\", path = \"{path}\", key = \"{key}\", reason = \"t\", value = {{ event = \"E\", type = \"command\", argv = [\"x\"] }} }}"
        )
    };
    for (key, path, code, message) in [
        (
            "other.audit",
            "/handlers",
            Code::ExtensionNamespace,
            "a contribution key must be `private-mod.`",
        ),
        (
            "private-mod.",
            "/handlers",
            Code::ExtensionNamespace,
            "a contribution key must be `private-mod.`",
        ),
        (
            "private-mod.UPPER",
            "/handlers",
            Code::ExtensionNamespace,
            "a contribution key must be `private-mod.`",
        ),
        (
            "audit",
            "/handlers",
            Code::ExtensionNamespace,
            "a contribution key must be `private-mod.`",
        ),
        (
            "private-mod.x",
            "/managed",
            Code::OverrideForbidden,
            "not an exposed extension map",
        ),
        (
            "private-mod.audit",
            "/handlers",
            Code::ExtensionConflict,
            "two contributors claim one key",
        ),
    ] {
        let mut set = synthetic();
        edit(
            &mut set,
            PRIVATE,
            "add",
            "/contributions/-",
            Some(&contrib(key, path)),
        );
        let e = refused(
            &set,
            B,
            code,
            PRIVATE,
            Some("reusable.agent-settings"),
            message,
        );
        assert!(
            e.field.as_deref().is_some_and(|f| f.starts_with(path)),
            "{key} {path}: {e}"
        );
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
    let e = refused(
        &set,
        B,
        Code::ExtensionConflict,
        PRIVATE,
        Some("reusable.agent-settings"),
        "already exists in the owner's map",
    );
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
        let e = refused(
            &set,
            B,
            Code::OverrideConflict,
            PRIVATE,
            Some("core.editor-defaults"),
            "two overrides address the same resource and path",
        );
        assert_eq!(
            e.conflicts_with.as_deref(),
            Some("machine-b (profiles/machine-b.toml)")
        );
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
    let e = refused(
        &set,
        B,
        Code::OverrideForbidden,
        PRIVATE,
        Some(SETTINGS),
        "the path is protected",
    );
    assert_eq!(e.conflicts_with.as_deref(), Some("reusable"));
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
    let p = &r.resources[SETTINGS].provenance["/tool/retries"];
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
    let personal = ok(&set, PERSONAL);
    assert_eq!(personal.resources["agents.skills"].claims.len(), 2);
}

#[test]
fn c03_native_documents_are_marked_so_precedence_is_reported() {
    let r = ok(&synthetic(), A);
    assert!(r.resources[SETTINGS].native_document);
    assert!(r.resources["reusable.agent-settings"].native_document);
    assert!(!r.resources["reusable.tool-config"].native_document);
}

// C04: unknown keys, wrong kinds, unsafe targets, escaping links, unsupported adapters.

#[test]
fn c04_unknown_keys_are_refused_everywhere() {
    let rows: &[(&str, &str, &str, &str, Option<&str>)] = &[
        (A, "profile", "/bogus", A, None),
        (SHARED_BUNDLE, "bundle", "/bogus", SHARED_BUNDLE, None),
        (REUSABLE, "module", "/bogus", REUSABLE, None),
        (REUSABLE, "resource", "/resources/0/bogus", REUSABLE, None),
        (
            REUSABLE,
            "policy",
            "/resources/0/policy/bogus",
            REUSABLE,
            None,
        ),
        (
            REUSABLE,
            "adapter value",
            "/resources/0/value/bogus",
            REUSABLE,
            Some("reusable.tool-config"),
        ),
        (B, "override", "/overrides/0/bogus", B, None),
        (
            PRIVATE,
            "contribution",
            "/contributions/0/bogus",
            PRIVATE,
            None,
        ),
    ];
    for (file, what, path, source, resource) in rows {
        let mut set = synthetic();
        edit(&mut set, file, "add", path, Some("\"v\""));
        let profile = if *file == A { A } else { B };
        let message = if *what == "adapter value" {
            "accepts known keys only"
        } else {
            "unknown key `bogus`"
        };
        let e = refused(
            &set,
            profile,
            Code::FieldUnknown,
            source,
            *resource,
            message,
        );
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
        SHARED_BUNDLE,
        "replace",
        "/core",
        Some("\"bundle.toml\""),
    );
    refused(
        &set,
        A,
        Code::ReferenceInvalid,
        SHARED_BUNDLE,
        None,
        "expected a module document, found a bundle document",
    );
    let mut set = synthetic();
    edit(
        &mut set,
        PRIVATE_BUNDLE,
        "replace",
        "/extends",
        Some("\"../shared/reusable/module.toml\""),
    );
    refused(
        &set,
        B,
        Code::ReferenceInvalid,
        REUSABLE,
        None,
        "expected a bundle document, found a module document",
    );
    let mut set = synthetic();
    edit(
        &mut set,
        A,
        "replace",
        "/bundle",
        Some("\"../shared/reusable/module.toml\""),
    );
    refused(
        &set,
        A,
        Code::ReferenceInvalid,
        REUSABLE,
        None,
        "expected a bundle document, found a module document",
    );
    let mut set = synthetic();
    edit(
        &mut set,
        SHARED_BUNDLE,
        "replace",
        "/modules/reusable",
        Some("\"bundle.toml\""),
    );
    refused(
        &set,
        A,
        Code::ReferenceInvalid,
        SHARED_BUNDLE,
        None,
        "expected a module document, found a bundle document",
    );
}

#[test]
fn c04_unsafe_targets_are_refused() {
    for (target, why) in [
        ("/etc/x", "absolute path"),
        ("../x", "dot component"),
        ("a/../b", "dot component"),
        ("a//b", "empty component"),
        ("a/./b", "dot component"),
        ("~/x", "tilde component"),
        ("a\\u0000b", "control character"),
        ("a\\nb", "control character"),
        ("", "empty path"),
    ] {
        let mut set = synthetic();
        edit(
            &mut set,
            REUSABLE,
            "replace",
            "/resources/0/target",
            Some(&format!("\"{target}\"")),
        );
        let e = refused(
            &set,
            A,
            Code::TargetInvalid,
            REUSABLE,
            Some("reusable.tool-config"),
            &format!("unsafe output path: {why}"),
        );
        assert_eq!(e.field.as_deref(), Some("/target"), "{target:?}");
    }
    let mut set = examples();
    edit(
        &mut set,
        AGENTS_MOD,
        "replace",
        "/resources/2/value/target_roots/0",
        Some("\"../x\""),
    );
    let e = refused(
        &set,
        PERSONAL,
        Code::TargetInvalid,
        AGENTS_MOD,
        Some("agents.skills"),
        "unsafe output path: dot component",
    );
    assert_eq!(e.field.as_deref(), Some("/target_roots/0"));
}

#[test]
fn c04_source_links_may_not_leave_their_root() {
    let tool = "shared/reusable/files/tool.conf";
    let cases = [
        (
            "../../../private/private-mod/files/banner",
            Some((Code::SourceEscape, "leaves the trusted root")),
        ),
        (
            "/etc/hosts",
            Some((Code::SourceEscape, "leaves the trusted root")),
        ),
        (
            "../../../profiles/machine-a.toml",
            Some((Code::SourceEscape, "leaves the trusted root")),
        ),
        (
            "nowhere",
            Some((
                Code::SourceMissing,
                "not in the materialized, trusted sources",
            )),
        ),
        ("tool.conf", Some((Code::SourceEscape, "form a loop"))),
        ("banner", None),
    ];
    for (target, want) in cases {
        let mut set = synthetic();
        set.insert_link(tool, target);
        match want {
            Some((code, message)) => {
                let e = refused(
                    &set,
                    A,
                    code,
                    REUSABLE,
                    Some("reusable.tool-config"),
                    message,
                );
                assert_eq!(e.field.as_deref(), Some("/source"), "{target}");
            }
            None => assert_eq!(
                ok(&set, A).resources["reusable.tool-config"].provenance["/source"]
                    .source_path
                    .as_deref(),
                Some("shared/reusable/files/banner")
            ),
        }
    }
    // A link inside a directory source is checked too.
    let mut set = examples();
    set.insert_link(
        "base/modules/agents/files/sample-audit/escape",
        "../../../core/files/gitconfig",
    );
    let e = refused(
        &set,
        PERSONAL,
        Code::SourceEscape,
        AGENTS_MOD,
        Some("agents.skills"),
        "leaves the trusted root",
    );
    assert_eq!(e.field.as_deref(), Some("/entries/sample-audit/source"));
}

#[test]
fn c04_unsupported_adapters_are_refused() {
    for (path, lit, message) in [
        (
            "/resources/0/adapter",
            "\"fs-link/v2\"",
            "adapter `fs-link/v2` is not built into this manager",
        ),
        (
            "/resources/0/adapter",
            "\"nope/v1\"",
            "adapter `nope/v1` is not built into this manager",
        ),
        (
            "/resources/0/kind",
            "\"render\"",
            "adapter `fs-link/v1` does not implement kind `render`",
        ),
    ] {
        let mut set = synthetic();
        edit(&mut set, REUSABLE, "replace", path, Some(lit));
        refused(
            &set,
            A,
            Code::AdapterUnknown,
            REUSABLE,
            Some("reusable.tool-config"),
            message,
        );
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
    let e = refused(
        &set,
        B,
        Code::TypeMismatch,
        PRIVATE,
        Some(TOKEN),
        "expected type integer",
    );
    assert_eq!(
        (e.rejected, e.field.as_deref()),
        (Some("boolean"), Some("/mode"))
    );
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
    refused(
        &set,
        A,
        Code::SourceEscape,
        A,
        Some("reusable.tool-banner"),
        "leaves the trusted root",
    );
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
    let e = refused(
        &set,
        B,
        Code::OverrideForbidden,
        PRIVATE,
        Some("core.launcher"),
        "the path is protected",
    );
    assert_eq!(e.conflicts_with.as_deref(), Some("core"));
    // Identity fields are not part of `value`, so no pointer can reach them.
    for path in ["/target", "/adapter", "/kind", "/id", "/policy/defaults"] {
        let mut set = synthetic();
        add_override(&mut set, A, "core.editor-defaults", path, "\"x\"");
        let e = refused(
            &set,
            A,
            Code::OverrideForbidden,
            A,
            Some("core.editor-defaults"),
            "the path is protected",
        );
        assert_eq!(e.field.as_deref(), Some(path));
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
    let e = refused(
        &set,
        B,
        Code::OverrideConflict,
        PRIVATE,
        Some("core.editor-defaults"),
        "two overrides address the same resource and path",
    );
    assert_eq!(
        e.conflicts_with.as_deref(),
        Some("machine-b (profiles/machine-b.toml)")
    );
    // Outputs: case-insensitive equal, file against directory, and a file above a claimed file.
    for (target, source, resource, other, why) in [
        (
            ".config/synthetic/TOOL.conf",
            REUSABLE,
            "reusable.tool-config",
            "private-mod.extra",
            "the same output path, compared without case",
        ),
        (
            ".config/synthetic/tool.conf/child",
            PRIVATE,
            "private-mod.extra",
            "reusable.tool-config",
            "a file cannot also be a directory",
        ),
        (
            ".config/synthetic/TOOL.CONF/child",
            PRIVATE,
            "private-mod.extra",
            "reusable.tool-config",
            "a file cannot also be a directory",
        ),
        (
            ".config",
            "shared/core/module.toml",
            "core.editor-defaults",
            "private-mod.extra",
            "a file cannot also be a directory",
        ),
    ] {
        let mut set = synthetic();
        let r = format!(
            "{{ id = \"extra\", kind = \"link\", adapter = \"fs-link/v1\", target = \"{target}\", value = {{ source = \"files/banner\" }} }}"
        );
        edit(&mut set, PRIVATE, "add", "/resources/-", Some(&r));
        let e = refused(&set, B, Code::TargetCollision, source, Some(resource), why);
        assert_eq!(e.conflicts_with.as_deref(), Some(other), "{target}");
    }
    // A distribution leaf against a link.
    let mut set = examples();
    let r = "{ id = \"clash\", kind = \"link\", adapter = \"fs-link/v1\", target = \".claude/skills/sample-audit\", value = { source = \"files/personal.env.tpl\" } }";
    edit(&mut set, PERSONAL_MOD, "add", "/resources/-", Some(r));
    let e = refused(
        &set,
        PERSONAL,
        Code::TargetCollision,
        PERSONAL_MOD,
        Some("personal-dev.clash"),
        "the same output path",
    );
    assert_eq!(e.conflicts_with.as_deref(), Some("agents.skills"));
    // Two skill entries that differ only by case.
    let mut set = examples();
    edit(
        &mut set,
        AGENTS_MOD,
        "add",
        "/resources/2/value/entries/Sample-Audit",
        Some("{ source = \"files/sample-audit\", enabled = true }"),
    );
    let e = refused(
        &set,
        PERSONAL,
        Code::TargetCollision,
        AGENTS_MOD,
        Some("agents.skills"),
        "the same output path, compared without case",
    );
    assert_eq!(e.conflicts_with.as_deref(), Some("agents.skills"));
}

#[test]
fn a_link_to_a_directory_needs_the_explicit_directory_flag() {
    let link = |extra: &str| {
        format!(
            "{{ id = \"dirlink\", kind = \"link\", adapter = \"fs-link/v1\", target = \".agents-dir\", value = {{ source = \"files/sample-audit\"{extra} }} }}"
        )
    };
    let mut set = examples();
    edit(&mut set, AGENTS_MOD, "add", "/resources/-", Some(&link("")));
    let e = refused(
        &set,
        PERSONAL,
        Code::ManifestInvalid,
        AGENTS_MOD,
        Some("agents.dirlink"),
        "is a directory",
    );
    assert!(e.action.contains("directory = true"), "{e}");
    let mut set = examples();
    edit(
        &mut set,
        AGENTS_MOD,
        "add",
        "/resources/-",
        Some(&link(", directory = true")),
    );
    ok(&set, PERSONAL);
    // The flag does not turn a file into a directory link or make the source optional.
    let mut set = examples();
    edit(
        &mut set,
        AGENTS_MOD,
        "add",
        "/resources/-",
        Some(&link(", directory = \"yes\"")),
    );
    assert_eq!(err(&set, PERSONAL).code, Code::TypeMismatch);
}

// C10 and C11: what an error says.

#[test]
fn c10_a_forbidden_override_names_source_resource_field_category_owner_and_fix() {
    let mut set = synthetic();
    add_override(&mut set, B, "core.launcher", "/source", "\"files/x\"");
    let e = refused(
        &set,
        B,
        Code::OverrideForbidden,
        B,
        Some("core.launcher"),
        "the path is protected",
    );
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
    for path in ["/account", "/mode"] {
        let mut set = synthetic();
        edit(
            &mut set,
            PRIVATE,
            "add",
            "/overrides/-",
            Some(&format!(
                "{{ resource = \"{TOKEN}\", path = \"{path}\", value = \"{PLANTED}\", reason = \"t\" }}"
            )),
        );
        let e = refused(
            &set,
            B,
            Code::OverrideForbidden,
            PRIVATE,
            Some(TOKEN),
            "the path is protected",
        );
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
    let e = refused(
        &set,
        B,
        Code::FieldUnknown,
        PRIVATE,
        Some(TOKEN),
        "accepts known keys only",
    );
    assert!(!format!("{e} {e:?}").contains(PLANTED));
    let mut set = synthetic();
    edit(
        &mut set,
        PRIVATE,
        "replace",
        "/resources/0/target",
        Some(&format!("\"../{PLANTED}\"")),
    );
    let e = refused(
        &set,
        B,
        Code::TargetInvalid,
        PRIVATE,
        Some(TOKEN),
        "unsafe output path: dot component",
    );
    assert!(!format!("{e} {e:?}").contains(PLANTED));
    // Secret resources carry no content identity: not even a hash of the template.
    let r = ok(&synthetic(), B);
    let t = &r.resources[TOKEN];
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
    let e = refused(
        &set,
        A,
        Code::FieldUnknown,
        REUSABLE,
        Some("reusable.tool-banner"),
        "unknown key; adapter `fs-link/v1` accepts known keys only",
    );
    assert_eq!(e.code.as_str(), "E_FIELD_UNKNOWN");
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
    let e = refused(
        &set,
        B,
        Code::OverrideConflict,
        PRIVATE,
        Some("core.editor-defaults"),
        "two overrides address the same resource and path",
    );
    assert_eq!(e.code.as_str(), "E_OVERRIDE_CONFLICT");
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

fn link_at(set: &mut SourceSet, id: &str, target: &str) {
    let r = format!(
        "{{ id = \"{id}\", kind = \"link\", adapter = \"fs-link/v1\", target = \"{target}\", value = {{ source = \"files/personal.env.tpl\" }} }}"
    );
    edit(set, PERSONAL_MOD, "add", "/resources/-", Some(&r));
}

fn assemble(id: &str, target: &str, dir: &str) -> String {
    format!(
        "{{ id = \"{id}\", kind = \"assemble\", adapter = \"text-fragments/v1\", target = \"{target}\", value = {{ body = \"files/personal.env.tpl\", fragments_dir = \"{dir}\", pattern = \"*.md\", separator = \"\\n\" }} }}"
    )
}

#[test]
fn m1_a_fragment_directory_accepts_only_matching_files_from_other_resources() {
    // work-01 places `40-work.md` in the assembler's directory, which is how a module supplies a fragment.
    ok(&examples(), "profiles/work-01.toml");
    for (target, why) in [
        (".codex/AGENTS.d/notes.txt", "name does not match *.md"),
        (".codex/AGENTS.d/sub/x.md", "not a direct child"),
        (".codex/AGENTS.d", "the directory itself"),
        (".CODEX/agents.D/X.TXT", "case alias of a non-matching name"),
        (".codex/AGENTS.d/sub/deeper/x.md", "nested"),
    ] {
        let mut set = examples();
        link_at(&mut set, "extra", target);
        let e = refused(
            &set,
            PERSONAL,
            Code::TargetCollision,
            PERSONAL_MOD,
            Some("personal-dev.extra"),
            "may place only a file matching `*.md` directly in it",
        );
        assert_eq!(
            e.conflicts_with.as_deref(),
            Some("agents.codex-instructions"),
            "{target}: {why}"
        );
    }
    // A matching direct child is a fragment, not a collision.
    let mut set = examples();
    link_at(&mut set, "extra", ".codex/AGENTS.d/50-extra.MD");
    ok(&set, PERSONAL);
}

#[test]
fn m1_fragment_directories_may_not_alias_or_nest() {
    // (dir, resource reported, source, other). The reported side is the later one in sorted order.
    for (dir, resource, source, other) in [
        (
            ".codex/AGENTS.d",
            "personal-dev.twin",
            PERSONAL_MOD,
            "agents.codex-instructions",
        ),
        (
            ".Codex/agents.D",
            "personal-dev.twin",
            PERSONAL_MOD,
            "agents.codex-instructions",
        ),
        (
            ".codex/AGENTS.d/inner",
            "personal-dev.twin",
            PERSONAL_MOD,
            "agents.codex-instructions",
        ),
        (
            ".codex",
            "agents.codex-instructions",
            AGENTS_MOD,
            "personal-dev.twin",
        ),
    ] {
        let mut set = examples();
        edit(
            &mut set,
            PERSONAL_MOD,
            "add",
            "/resources/-",
            Some(&assemble("twin", ".codex/OTHER.md", dir)),
        );
        let e = refused(
            &set,
            PERSONAL,
            Code::TargetCollision,
            source,
            Some(resource),
            "fragment directories may not be equal or nested",
        );
        assert_eq!(e.conflicts_with.as_deref(), Some(other), "{dir}");
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
    let e = refused(
        &set,
        PERSONAL,
        Code::TargetCollision,
        PERSONAL_MOD,
        Some("personal-dev.self"),
        "an assembled file cannot be placed in the directory it reads fragments from",
    );
    assert_eq!(e.conflicts_with.as_deref(), Some("personal-dev.self"));
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
        let e = refused(
            &set,
            PERSONAL,
            Code::ManifestInvalid,
            PERSONAL_MOD,
            Some("personal-dev.badpat"),
            "the fragment pattern may use only `*` and `?`",
        );
        assert_eq!(e.field.as_deref(), Some("/pattern"), "{bad:?}");
    }
}

#[test]
fn m1_a_file_above_a_fragment_directory_is_refused() {
    // Resource X claims `.codex` as a file; the assembler reads `.codex/AGENTS.d`.
    for (target, why) in [(".codex", "the grandparent"), (".CODEX", "a case alias")] {
        let mut set = examples();
        link_at(&mut set, "above", target);
        let e = refused(
            &set,
            PERSONAL,
            Code::TargetCollision,
            AGENTS_MOD,
            Some("agents.codex-instructions"),
            "a file cannot also be the parent of a fragment directory",
        );
        assert_eq!(
            e.conflicts_with.as_deref(),
            Some("personal-dev.above"),
            "{why}"
        );
    }
    // The same goes for a file that is the direct parent of a nested directory.
    let mut set = examples();
    edit(
        &mut set,
        PERSONAL_MOD,
        "add",
        "/resources/-",
        Some(&assemble("deep", ".config/deep.md", ".config/a/b/c")),
    );
    link_at(&mut set, "mid", ".config/a/b");
    let e = refused(
        &set,
        PERSONAL,
        Code::TargetCollision,
        PERSONAL_MOD,
        Some("personal-dev.deep"),
        "a file cannot also be the parent of a fragment directory",
    );
    assert_eq!(e.conflicts_with.as_deref(), Some("personal-dev.mid"));
}

// Native precedence is per value, from the authored include order.

#[test]
fn m2_includes_follow_the_order_written_in_the_authored_document() {
    let r = ok(&examples(), "profiles/work-01.toml");
    assert_eq!(r.includes.len(), 1);
    let g = &r.includes[0];
    assert_eq!(g.base, "core.git");
    assert_eq!(g.base_target.as_deref(), Some(".config/git/config"));
    // The ordering rule belongs to the fragment's adapter, in one place.
    assert_eq!(
        r.resources["core.git-defaults"].precedence.as_deref(),
        Some("later include entries override earlier ones for single-valued keys")
    );
    assert_eq!(r.resources["core.git"].precedence, None);
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
fn l4_dotdot_needs_a_real_directory() {
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
        refused(
            &set,
            A,
            Code::SourceMissing,
            REUSABLE,
            Some("reusable.tool-config"),
            "not in the materialized, trusted sources",
        );
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
}

#[test]
fn l4_a_case_alias_cannot_hide_a_link() {
    // A link spelled `Link` is the same entry as `link` on a case-insensitive filesystem. Without
    // the alias the name is simply missing, so SourceEscape proves the case-folded lookup ran.
    let mut control = synthetic();
    edit(
        &mut control,
        REUSABLE,
        "replace",
        "/resources/0/value/source",
        Some("\"files/link\""),
    );
    refused(
        &control,
        A,
        Code::SourceMissing,
        REUSABLE,
        Some("reusable.tool-config"),
        "not in the materialized, trusted sources",
    );
    let mut set = control.clone();
    set.insert_link(
        "shared/reusable/files/Link",
        "../../../private/private-mod/files/banner",
    );
    refused(
        &set,
        A,
        Code::SourceEscape,
        REUSABLE,
        Some("reusable.tool-config"),
        "the path or a symlink in it leaves the trusted root",
    );
    // A link that stays inside the root is followed through the alias to its target.
    let mut inside = control.clone();
    inside.insert_link("shared/reusable/files/Link", "banner");
    let p = &ok(&inside, A).resources["reusable.tool-config"].provenance["/source"];
    assert_eq!(
        p.source_path.as_deref(),
        Some("shared/reusable/files/banner")
    );
    // Two spellings of one name cannot be told apart there. This is the CaseAlias branch.
    let mut set = synthetic();
    set.insert_file("shared/reusable/files/Tool.conf", "other");
    refused(
        &set,
        A,
        Code::SourceEscape,
        REUSABLE,
        Some("reusable.tool-config"),
        "two source entries differ only by case",
    );
}

#[test]
fn l5_bidi_and_zero_width_characters_are_refused_in_targets_and_escaped_in_messages() {
    for target in [
        "a\\u202eb",
        "a\\u200bb",
        "\\ufeffa",
        "a\\u2066b",
        "a\\u061cb",
        "a\\u2028b",
        "a\\u2029b",
        "a\\u034fb",
        "a\\u180eb",
        "a\\U000e0041b",
        "a\\ufe0fb",
        "a\\u3164b",
        "a\\u115fb",
    ] {
        let mut set = synthetic();
        edit(
            &mut set,
            REUSABLE,
            "replace",
            "/resources/0/target",
            Some(&format!("\"{target}\"")),
        );
        refused(
            &set,
            A,
            Code::TargetInvalid,
            REUSABLE,
            Some("reusable.tool-config"),
            "invisible or bidirectional-control character",
        );
    }
    let mut set = synthetic();
    edit(&mut set, REUSABLE, "add", "/bogus\u{202e}x", Some("1"));
    let e = refused(
        &set,
        A,
        Code::FieldUnknown,
        REUSABLE,
        None,
        "unknown key `bogus",
    );
    let shown = e.to_string();
    assert!(
        shown.contains("\\u{202e}") && !shown.contains('\u{202e}'),
        "{shown}"
    );
}

#[test]
fn l6_secret_file_modes_are_bounded_and_private() {
    // 0o600 and 0o400 are private. -1 and 0o1000 are out of range. 0o666, 0o640 are not private.
    for (lit, want) in [
        ("384", None),
        ("256", None),
        ("0", None),
        ("-1", Some("a file mode is an integer from 0 to 0o777")),
        ("512", Some("a file mode is an integer from 0 to 0o777")),
        ("4096", Some("a file mode is an integer from 0 to 0o777")),
        (
            "438",
            Some("a private file mode must clear group and other bits"),
        ),
        (
            "416",
            Some("a private file mode must clear group and other bits"),
        ),
    ] {
        let mut set = synthetic();
        edit(
            &mut set,
            PRIVATE,
            "replace",
            "/resources/0/value/mode",
            Some(lit),
        );
        match want {
            None => {
                ok(&set, B);
            }
            Some(message) => {
                let e = refused(
                    &set,
                    B,
                    Code::ManifestInvalid,
                    PRIVATE,
                    Some(TOKEN),
                    message,
                );
                assert_eq!(
                    (e.rejected, e.field.as_deref()),
                    (Some("integer"), Some("/mode")),
                    "{lit}"
                );
            }
        }
    }
}
