//! `explain`: declaration ownership, permitted changes, provenance, native precedence and
//! runtime-verification status for one resolved resource (DESIGN.md section 7). Pure text from a
//! [`Resolved`] value. Values of resources that hold secret references print as `[redacted]`.

use super::error::visible;
use super::{ChangeKind, Resolved, ResolvedResource};
use crate::{Code, Refusal};
use std::fmt::Write;

const REDACTED: &str = "[redacted]";
const MAX_VALUE_CHARS: usize = 200;

fn show(r: &ResolvedResource, v: &toml::Value) -> String {
    if r.secret_values {
        return REDACTED.into();
    }
    let s = visible(&v.to_string());
    if s.chars().count() > MAX_VALUE_CHARS {
        format!("{}...", s.chars().take(MAX_VALUE_CHARS).collect::<String>())
    } else {
        s
    }
}

fn describe(r: &ResolvedResource, out: &mut String) {
    let list = |v: &[String]| {
        if v.is_empty() {
            "none".to_string()
        } else {
            v.join(", ")
        }
    };
    let _ = writeln!(out, "resource {}", visible(&r.id));
    let _ = writeln!(
        out,
        "  declared by module {} in {}",
        visible(&r.module),
        visible(&r.declared_in)
    );
    let _ = writeln!(
        out,
        "  kind {}, adapter {}",
        visible(&r.kind),
        visible(&r.adapter)
    );
    if let Some(t) = &r.target {
        let _ = writeln!(out, "  target {}", visible(t));
    }
    for c in r.claims.iter().filter(|c| Some(*c) != r.target.as_ref()) {
        let _ = writeln!(out, "  claims {}", visible(c));
    }
    let _ = writeln!(
        out,
        "  permitted overrides: {}",
        visible(&list(&r.defaults))
    );
    let _ = writeln!(
        out,
        "  permitted extensions: {}",
        visible(&list(&r.extensions))
    );
    if r.native_document {
        let _ = writeln!(
            out,
            "  native precedence: this adapter writes into a native document. The application \
             decides precedence between fragments and includes. The manager does not validate \
             the resulting native configuration, so it cannot enforce a protected setting there."
        );
    }
    let _ = writeln!(
        out,
        "  runtime verification: not verified. The resolver reads declarations only and inspects no machine state."
    );
    let _ = writeln!(out, "  values:");
    for (ptr, p) in &r.provenance {
        let value = ptr
            .split('/')
            .skip(1)
            .map(|t| t.replace("~1", "/").replace("~0", "~"))
            .try_fold(&r.value, |cur, t| cur.get(t.as_str()));
        let shown = value.map_or_else(|| REDACTED.to_string(), |v| show(r, v));
        let _ = writeln!(out, "    {} = {}", visible(ptr), shown);
        let _ = writeln!(out, "      owner {}", visible(&p.owner));
        match (p.change, &p.changer) {
            (Some(kind), Some(by)) => {
                let how = match kind {
                    ChangeKind::Override => "override",
                    ChangeKind::Contribution => "contribution",
                };
                let _ = writeln!(out, "      changed by {} ({how})", visible(by));
                if let Some(reason) = &p.reason {
                    let _ = writeln!(out, "      reason: {}", visible(reason));
                }
                if let Some(o) = &p.original {
                    let _ = writeln!(out, "      declared value: {}", show(r, o));
                }
            }
            _ => {
                let _ = writeln!(out, "      declared value, not changed");
            }
        }
        let _ = writeln!(out, "      supplied by {}", visible(&p.source_file));
        if let Some(path) = &p.source_path {
            let _ = writeln!(out, "      source file {}", visible(path));
        }
        if let Some(id) = &p.content_id {
            let _ = writeln!(out, "      content {id}");
        }
    }
}

fn fold(s: &str) -> String {
    s.to_lowercase()
}

/// Explains every resource whose ID equals `query`, or whose target or claimed output equals it
/// (compared without case). An unknown query is a usage error.
pub fn explain(resolved: &Resolved, query: &str) -> Result<String, Refusal> {
    let mut hits: Vec<&ResolvedResource> = Vec::new();
    if let Some(r) = resolved.resources.get(query) {
        hits.push(r);
    } else {
        let q = fold(query.trim_end_matches('/'));
        hits.extend(
            resolved
                .resources
                .values()
                .filter(|r| r.target.iter().chain(&r.claims).any(|t| fold(t) == q)),
        );
    }
    if hits.is_empty() {
        return Err(Refusal::new(
            Code::Usage,
            format!(
                "no resource or output path `{}` in profile {}",
                visible(query),
                visible(&resolved.profile)
            ),
        ));
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "profile {} ({}/{}), modules: {}",
        visible(&resolved.profile),
        visible(&resolved.platform),
        visible(&resolved.architecture),
        visible(&resolved.modules.join(", "))
    );
    for r in hits {
        describe(r, &mut out);
    }
    Ok(out)
}
