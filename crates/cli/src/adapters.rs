//! The adapter registry the resolver validates against (docs/architecture.md section 3: the CLI
//! builds the registry). Each entry states what the resolver needs to know about an adapter: its
//! kind, whether it owns a target, how its `value` is typed, and which outputs it claims. No
//! adapter is run here.

mod generate;
mod merge_plan;
pub use generate::{Gen, Generators, plan_outputs};
pub use merge_plan::merge_facts;

use dfm_model::resolve::{AdapterCatalog, AdapterSpec, Claims, Field, Include, Schema, TargetRule};

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
        sensitive: false,
        native_document: false,
        claims,
        value,
        source_includes: None,
        precedence: None,
    }
}

/// A Git config value as `git config` returns it: double quotes group text, a backslash escapes
/// the next character (`\n`, `\t`, `\b`, `\\`, `\"`), and an unquoted `#` or `;` starts a comment.
/// Unquoted leading and trailing white space is dropped.
fn config_value(raw: &str) -> String {
    let (mut out, mut keep, mut quoted) = (String::new(), 0, false);
    let mut chars = raw.trim_start().chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                keep = out.len();
            }
            '#' | ';' if !quoted => break,
            '\\' => {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('b') => out.push('\u{8}'),
                    Some(other) => out.push(other),
                    None => break,
                }
                keep = out.len();
            }
            c => {
                out.push(c);
                if quoted || !c.is_whitespace() {
                    keep = out.len();
                }
            }
        }
    }
    out.truncate(keep);
    out
}

/// Include directives of a Git config, in file order: `[include]` and `[includeIf "<cond>"]`
/// sections with a `path` key. Nothing else in the file matters for precedence. A key may follow
/// the header on the same line (`[include] path = x`), which Git accepts. A section name that
/// only starts with `includeif` (such as `includeiffy`) is not an include.
fn git_includes(text: &str) -> Vec<Include> {
    fn section(name: &str) -> Option<Option<String>> {
        if name.eq_ignore_ascii_case("include") {
            return Some(None);
        }
        let (head, rest) = (name.get(..9)?, name.get(9..)?);
        if !head.eq_ignore_ascii_case("includeif") || !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let cond = rest.trim().strip_prefix('"')?.strip_suffix('"')?;
        Some(Some(cond.to_string()))
    }
    let mut current: Option<Option<String>> = None;
    let mut out = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let key_line = match line.strip_prefix('[').and_then(|l| l.split_once(']')) {
            Some((name, tail)) => {
                current = section(name.trim());
                tail.trim()
            }
            None => line,
        };
        if key_line.is_empty() || key_line.starts_with('#') || key_line.starts_with(';') {
            continue;
        }
        if let (Some(cond), Some((key, value))) = (&current, key_line.split_once('='))
            && key.trim().eq_ignore_ascii_case("path")
        {
            out.push(Include {
                path: config_value(value),
                condition: cond.clone(),
            });
        }
    }
    out
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

/// One adapter: what the resolver validates against, and the generator if the adapter produces
/// file content. This table is the only place that names an adapter, so the catalog and the
/// generators cannot disagree.
pub struct Adapter {
    pub spec: AdapterSpec,
    pub generator: Option<Gen>,
}

impl Adapter {
    /// Whether the adapter is the sensitive co-owned merge. It has no `Generator::generate`
    /// output: apply runs `Generator::merge` for it.
    pub fn merges(&self) -> bool {
        self.spec.kind == "merge"
            && self.spec.sensitive
            && self.spec.id == dfm_model::merge::ADAPTER
    }
}

pub fn registry() -> Vec<Adapter> {
    let native = |mut s: AdapterSpec| {
        s.native_document = true;
        s
    };
    vec![
        Adapter {
            spec: AdapterSpec {
                source_includes: Some(git_includes),
                ..spec(
                    "fs-link/v1",
                    "link",
                    TargetRule::Required,
                    Claims::Target,
                    Schema::Table(vec![
                        field("source", Schema::Source, true),
                        field("directory", Schema::Bool, false),
                    ]),
                )
            },
            generator: None,
        },
        Adapter {
            spec: native(AdapterSpec {
                precedence: Some(
                    "later include entries override earlier ones for single-valued keys",
                ),
                ..spec(
                    "git-config/v1",
                    "render",
                    TargetRule::Required,
                    Claims::Target,
                    Schema::Native,
                )
            }),
            generator: Some(generate::GIT_CONFIG),
        },
        // The live file can hold credentials in fields the manager does not own.
        Adapter {
            spec: native(AdapterSpec {
                sensitive: true,
                ..spec(
                    "claude-settings/v1",
                    "merge",
                    TargetRule::Fixed(".claude/settings.json"),
                    Claims::TargetAndFragments {
                        dir: "/fragments_dir",
                        pattern: "/pattern",
                    },
                    Schema::Table(vec![
                        field("managed", Schema::Native, true),
                        field("seed", Schema::Native, false),
                        field("append", Schema::Array(Box::new(Schema::Str)), false),
                        field("fragments_dir", Schema::HomePath, false),
                        field("pattern", Schema::Str, false),
                        field(
                            "handlers",
                            Schema::Map(Box::new(Schema::Table(vec![
                                field("event", Schema::Str, true),
                                field("type", Schema::Str, true),
                                field("argv", Schema::Array(Box::new(Schema::Str)), true),
                                field("matcher", Schema::Str, false),
                                field("timeout", Schema::Int, false),
                            ]))),
                            true,
                        ),
                    ]),
                )
            }),
            generator: None,
        },
        Adapter {
            spec: native(spec(
                "text-fragments/v1",
                "assemble",
                TargetRule::Required,
                Claims::TargetAndFragments {
                    dir: "/fragments_dir",
                    pattern: "/pattern",
                },
                Schema::Table(vec![
                    field("body", Schema::Source, true),
                    field("fragments_dir", Schema::HomePath, true),
                    field("pattern", Schema::Str, true),
                    field("separator", Schema::Str, true),
                ]),
            )),
            generator: Some(generate::TEXT_FRAGMENTS),
        },
        Adapter {
            spec: spec(
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
            generator: None,
        },
        Adapter {
            spec: AdapterSpec {
                secret_values: true,
                ..spec(
                    "op-template/v1",
                    "secret",
                    TargetRule::Required,
                    Claims::Target,
                    Schema::Table(vec![
                        field("template", Schema::Source, true),
                        field("account", Schema::Str, true),
                        field("mode", Schema::FileMode { private: true }, true),
                    ]),
                )
            },
            generator: None,
        },
        Adapter {
            spec: package("homebrew/v1"),
            generator: None,
        },
        Adapter {
            spec: package("mise/v1"),
            generator: None,
        },
        Adapter {
            spec: package("apt/v1"),
            generator: None,
        },
        Adapter {
            spec: spec(
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
            generator: None,
        },
    ]
}

#[doc(hidden)]
pub fn builtin_catalog() -> AdapterCatalog {
    AdapterCatalog::new(registry().into_iter().map(|a| a.spec))
}

#[cfg(test)]
mod tests {
    use super::{git_includes, registry};

    #[test]
    fn every_generated_kind_has_a_generator_and_nothing_else_has_one() {
        let table = registry();
        assert!(!table.is_empty());
        let mut ids: Vec<&str> = table.iter().map(|a| a.spec.id.as_str()).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "an adapter id appears twice");
        for a in &table {
            let generated = matches!(a.spec.kind, "render" | "assemble");
            assert_eq!(
                a.generator.is_some(),
                generated,
                "{} ({}): a render or assemble adapter needs a generator and no other kind has one",
                a.spec.id,
                a.spec.kind
            );
        }
        // Merge has no `generate` output. It is sensitive, and apply runs `Generator::merge`.
        let merge: Vec<_> = table.iter().filter(|a| a.spec.kind == "merge").collect();
        assert!(!merge.is_empty());
        assert!(
            merge
                .iter()
                .all(|a| a.generator.is_none() && a.spec.sensitive && a.merges())
        );
        assert_eq!(table.iter().filter(|a| a.merges()).count(), merge.len());
    }

    #[test]
    fn untrusted_section_names_never_panic_on_a_character_boundary() {
        // Byte 9 falls inside the euro sign; slicing there would panic.
        for text in [
            "[abcdefgh\u{20ac}]\npath = x\n",
            "[\u{20ac}\u{20ac}\u{20ac}\u{20ac}]\n",
            "[\u{1f600}includeif]\n",
            "[includeif\u{20ac}\"c\"]\npath = ~/p\n",
            "[",
            "[]",
            "path",
        ] {
            let _ = git_includes(text);
        }
        // A real section with a multibyte condition still parses.
        let got = git_includes("[includeIf \"gitdir:~/\u{e9}/\"]\npath = ~/a\n");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].condition.as_deref(), Some("gitdir:~/\u{e9}/"));
    }

    #[test]
    fn only_real_include_sections_count() {
        let got = |t: &str| {
            git_includes(t)
                .into_iter()
                .map(|i| i.path)
                .collect::<Vec<_>>()
        };
        // Lookalike section names are not includes.
        assert!(got("[includeiffy]\npath = ~/a\n").is_empty());
        assert!(got("[includeIfx \"c\"]\npath = ~/a\n").is_empty());
        assert!(got("[includeIf]\npath = ~/a\n").is_empty());
        assert!(got("[includeIf c]\npath = ~/a\n").is_empty());
        assert!(got("[include \"x\"]\npath = ~/a\n").is_empty());
        assert!(got("[includes]\npath = ~/a\n").is_empty());
        // A key on the header line counts, in file order with the others.
        assert_eq!(
            got(
                "[include] path = ~/one\n[includeIf \"g:~/\"] path = ~/two\n[include]\npath = ~/three\n[user] path = ~/no\n"
            ),
            ["~/one", "~/two", "~/three"]
        );
        // A comment after the header is not a key.
        assert!(got("[include] # path = ~/a\n").is_empty());
    }

    #[test]
    fn an_include_value_is_read_as_git_reads_it() {
        let got = |t: &str| {
            git_includes(t)
                .into_iter()
                .map(|i| i.path)
                .collect::<Vec<_>>()
        };
        // Quotes and a trailing comment are not part of the path.
        assert_eq!(
            got("[include]\npath = \"~/.config/foo.inc\" # local identity\n"),
            ["~/.config/foo.inc"]
        );
        assert_eq!(
            got("[include]\npath = ~/a.inc ; note\n[include]\npath=~/b.inc#x\n"),
            ["~/a.inc", "~/b.inc"]
        );
        // A comment character inside quotes stays, and so does white space inside quotes.
        assert_eq!(got("[include]\npath = \"~/a#b ;c \"  \n"), ["~/a#b ;c "]);
        // A backslash escapes the next character.
        assert_eq!(got("[include]\npath = \"~/a\\\"b\"\n"), ["~/a\"b"]);
    }

    #[test]
    fn git_include_directives_keep_file_order_and_conditions() {
        let text = "# c\n[user]\n  path = ignored\n[include]\n  path = ~/a.inc\n[includeIf \"gitdir:~/w/\"]\n  path = ~/b.inc\n[include]\n  PATH = ~/c.inc\n";
        let got: Vec<(String, Option<String>)> = git_includes(text)
            .into_iter()
            .map(|i| (i.path, i.condition))
            .collect();
        assert_eq!(
            got,
            [
                ("~/a.inc".to_string(), None),
                ("~/b.inc".to_string(), Some("gitdir:~/w/".to_string())),
                ("~/c.inc".to_string(), None),
            ]
        );
    }
}
