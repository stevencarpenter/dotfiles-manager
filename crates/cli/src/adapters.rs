//! The adapter registry the resolver validates against (docs/architecture.md section 3: the CLI
//! builds the registry). Each entry states what the resolver needs to know about an adapter: its
//! kind, whether it owns a target, how its `value` is typed, and which outputs it claims. No
//! adapter is run here.

use dfm_model::resolve::{AdapterCatalog, AdapterSpec, Claims, Field, Schema, TargetRule};

fn field(name: &'static str, schema: Schema, required: bool) -> Field {
    Field {
        name,
        schema,
        required,
    }
}

fn spec(
    id: &str,
    kind: &'static str,
    target: TargetRule,
    claims: Claims,
    value: Schema,
) -> AdapterSpec {
    AdapterSpec {
        id: id.into(),
        kind,
        target,
        secret_values: false,
        native_document: false,
        claims,
        value,
    }
}

fn package(id: &str) -> AdapterSpec {
    spec(
        id,
        "package",
        TargetRule::Forbidden,
        Claims::None,
        Schema::Table(vec![
            field("name", Schema::Str, true),
            field("update_policy", Schema::Str, true),
            field("downgrade", Schema::Str, false),
        ]),
    )
}

pub fn builtin_catalog() -> AdapterCatalog {
    let native = |mut s: AdapterSpec| {
        s.native_document = true;
        s
    };
    AdapterCatalog::new([
        spec(
            "fs-link/v1",
            "link",
            TargetRule::Required,
            Claims::Target,
            Schema::Table(vec![field("source", Schema::Source, true)]),
        ),
        native(spec(
            "git-config/v1",
            "render",
            TargetRule::Required,
            Claims::Target,
            Schema::Native,
        )),
        native(spec(
            "claude-settings/v1",
            "merge",
            TargetRule::Required,
            Claims::Target,
            Schema::Table(vec![
                field("managed", Schema::Native, true),
                field("seed", Schema::Native, false),
                field(
                    "handlers",
                    Schema::Map(Box::new(Schema::Table(vec![
                        field("event", Schema::Str, true),
                        field("type", Schema::Str, true),
                        field("argv", Schema::Array(Box::new(Schema::Str)), true),
                    ]))),
                    true,
                ),
            ]),
        )),
        native(spec(
            "text-fragments/v1",
            "assemble",
            TargetRule::Required,
            Claims::Target,
            Schema::Table(vec![
                field("body", Schema::Source, true),
                field("fragments_dir", Schema::HomePath, true),
                field("pattern", Schema::Str, true),
                field("separator", Schema::Str, true),
            ]),
        )),
        spec(
            "skills/v1",
            "distribution",
            TargetRule::Forbidden,
            Claims::Leaves {
                roots: "/target_roots",
                entries: "/entries",
                enabled: "enabled",
            },
            Schema::Table(vec![
                field(
                    "target_roots",
                    Schema::Array(Box::new(Schema::HomePath)),
                    true,
                ),
                field(
                    "entries",
                    Schema::Map(Box::new(Schema::Table(vec![
                        field("source", Schema::Source, true),
                        field("enabled", Schema::Bool, true),
                    ]))),
                    true,
                ),
            ]),
        ),
        AdapterSpec {
            secret_values: true,
            ..spec(
                "op-template/v1",
                "secret",
                TargetRule::Required,
                Claims::Target,
                Schema::Table(vec![
                    field("template", Schema::Source, true),
                    field("account", Schema::Str, true),
                    field("mode", Schema::Int, true),
                ]),
            )
        },
        package("homebrew/v1"),
        package("mise/v1"),
        package("apt/v1"),
        spec(
            "firstmate-native/v1",
            "application",
            TargetRule::Forbidden,
            Claims::None,
            Schema::Table(vec![
                field("name", Schema::Str, true),
                field("update_policy", Schema::Str, true),
                field("completion", Schema::Str, true),
            ]),
        ),
    ])
}
