//! `explain`: declaration ownership, permitted changes, provenance, native precedence and
//! runtime-verification status for one resolved resource (DESIGN.md section 7). Pure text from a
//! [`Resolved`] value. Values of resources that hold secret references print as `[redacted]`.
//! Output is bounded: long strings are clipped and an oversized report is refused.

use super::error::visible;
use super::pointer;
use super::{ChangeKind, IncludeEntry, IncludeGroup, Resolved, ResolvedResource};
use crate::{Code, Refusal};
use std::fmt::Write;

const REDACTED: &str = "[redacted]";
const MAX_VALUE_CHARS: usize = 200;
const MAX_OUTPUT_BYTES: usize = 1 << 20;
const MAX_RESOURCES: usize = 100;

/// Escapes untrusted text and clips it to `MAX_VALUE_CHARS`.
fn clip(s: &str) -> String {
    let s = visible(s);
    if s.chars().count() > MAX_VALUE_CHARS {
        format!("{}...", s.chars().take(MAX_VALUE_CHARS).collect::<String>())
    } else {
        s
    }
}

fn show(r: &ResolvedResource, v: &toml::Value) -> String {
    if r.secret_values {
        REDACTED.into()
    } else {
        clip(&v.to_string())
    }
}

fn entry_text(e: &IncludeEntry) -> String {
    let cond = e
        .condition
        .as_ref()
        .map_or(String::new(), |c| format!(" only when {}", clip(c)));
    match (&e.resource, &e.module) {
        (Some(r), Some(m)) => format!(
            "{}. {}{cond} -> {} (module {})",
            e.position,
            clip(&e.path),
            clip(r),
            clip(m)
        ),
        _ => format!(
            "{}. {}{cond} -> no resource in this composition declares this file",
            e.position,
            clip(&e.path)
        ),
    }
}

fn native_header(r: &ResolvedResource, groups: &[IncludeGroup], out: &mut String) {
    let rule = r
        .precedence
        .as_deref()
        .unwrap_or("the application decides which value wins");
    let _ = writeln!(
        out,
        "  native precedence: this adapter writes into a native document. Rule: {}. The manager \
         does not validate the resulting native configuration, so it cannot enforce a protected \
         setting there.",
        clip(rule)
    );
    for g in groups {
        if let Some(me) = g
            .entries
            .iter()
            .find(|e| e.resource.as_deref() == Some(&r.id))
        {
            let _ = writeln!(
                out,
                "  included by {} ({}) at position {} of {}{}",
                clip(&g.base),
                clip(g.base_target.as_deref().unwrap_or("no target")),
                me.position,
                g.entries.len(),
                me.condition
                    .as_ref()
                    .map_or(String::new(), |c| format!(", only when {}", clip(c)))
            );
            let _ = writeln!(out, "  sibling fragments in read order:");
            for e in &g.entries {
                let _ = writeln!(out, "    {}", entry_text(e));
            }
        }
    }
}

/// Other fragments in the same include group that also set `ptr`, with their position.
fn also_set<'a>(
    r: &ResolvedResource,
    ptr: &str,
    groups: &'a [IncludeGroup],
    resolved: &Resolved,
) -> Vec<(&'a IncludeEntry, String)> {
    let mut out = Vec::new();
    for g in groups {
        if !g
            .entries
            .iter()
            .any(|e| e.resource.as_deref() == Some(&r.id))
        {
            continue;
        }
        for e in &g.entries {
            if let Some(other) = e.resource.as_deref().filter(|o| *o != r.id)
                && resolved.resources[other].provenance.contains_key(ptr)
            {
                out.push((e, other.to_string()));
            }
        }
    }
    out
}

fn too_big() -> Refusal {
    Refusal::new(
        Code::LimitExceeded,
        "the explanation is too large; ask for one resource",
    )
}

fn describe(resolved: &Resolved, r: &ResolvedResource, out: &mut String) -> Result<(), Refusal> {
    let list = |v: &[String]| {
        if v.is_empty() {
            "none".to_string()
        } else {
            v.join(", ")
        }
    };
    let _ = writeln!(out, "resource {}", clip(&r.id));
    let _ = writeln!(
        out,
        "  declared by module {} in {}",
        clip(&r.module),
        clip(&r.declared_in)
    );
    let _ = writeln!(
        out,
        "  kind {}, adapter {}",
        clip(&r.kind),
        clip(&r.adapter)
    );
    if let Some(t) = &r.target {
        let _ = writeln!(out, "  target {}", clip(t));
    }
    for c in r.claims.iter().filter(|c| Some(*c) != r.target.as_ref()) {
        let _ = writeln!(out, "  claims {}", clip(c));
    }
    for d in &r.dir_claims {
        let _ = writeln!(
            out,
            "  reads fragments {} from {}",
            clip(&d.pattern),
            clip(&d.dir)
        );
    }
    let _ = writeln!(out, "  permitted overrides: {}", clip(&list(&r.defaults)));
    let _ = writeln!(
        out,
        "  permitted extensions: {}",
        clip(&list(&r.extensions))
    );
    if let Some(g) = resolved.includes.iter().find(|g| g.base == r.id) {
        let _ = writeln!(out, "  includes, in the order the application reads them:");
        for e in &g.entries {
            let _ = writeln!(out, "    {}", entry_text(e));
        }
    }
    if r.native_document {
        native_header(r, &resolved.includes, out);
    }
    let _ = writeln!(
        out,
        "  runtime verification: not verified. The resolver reads declarations only and inspects no machine state."
    );
    let _ = writeln!(out, "  values:");
    for (ptr, p) in &r.provenance {
        let value = pointer::parse(ptr)
            .ok()
            .and_then(|t| pointer::get(&r.value, &t));
        let shown = value.map_or_else(|| "[unavailable]".to_string(), |v| show(r, v));
        let _ = writeln!(out, "    {} = {}", clip(ptr), shown);
        let _ = writeln!(out, "      owner {}", clip(&p.owner));
        match (p.change, &p.changer) {
            (Some(kind), Some(by)) => {
                let how = match kind {
                    ChangeKind::Override => "override",
                    ChangeKind::Contribution => "contribution",
                };
                let _ = writeln!(out, "      changed by {} ({how})", clip(by));
                if let Some(reason) = &p.reason {
                    let _ = writeln!(out, "      reason: {}", clip(reason));
                }
                if let Some(o) = &p.original {
                    let _ = writeln!(out, "      declared value: {}", show(r, o));
                }
            }
            _ => {
                let _ = writeln!(out, "      declared value, not changed");
            }
        }
        let _ = writeln!(out, "      supplied by {}", clip(&p.source_file));
        if let Some(path) = &p.source_path {
            let _ = writeln!(out, "      source file {}", clip(path));
        }
        if let Some(id) = &p.content_id {
            let _ = writeln!(out, "      content {id}");
        }
        if r.native_document {
            for (e, other) in also_set(r, ptr, &resolved.includes, resolved) {
                let _ = writeln!(
                    out,
                    "      also set by {} (include position {}); the final value depends on the application's include order",
                    clip(&other),
                    e.position
                );
            }
        }
        if out.len() > MAX_OUTPUT_BYTES {
            return Err(too_big());
        }
    }
    Ok(())
}

/// Explains every resource whose ID equals `query`, or whose target or claimed output equals it
/// (compared without case). An unknown query is a usage error. A report over 100 resources or
/// 1 MiB is refused.
pub fn explain(resolved: &Resolved, query: &str) -> Result<String, Refusal> {
    let mut hits: Vec<&ResolvedResource> = Vec::new();
    if let Some(r) = resolved.resources.get(query) {
        hits.push(r);
    } else {
        let q = query.trim_end_matches('/').to_lowercase();
        hits.extend(resolved.resources.values().filter(|r| {
            r.target
                .iter()
                .chain(&r.claims)
                .any(|t| t.to_lowercase() == q)
        }));
    }
    if hits.is_empty() {
        return Err(Refusal::new(
            Code::Usage,
            format!(
                "no resource or output path `{}` in profile {}",
                clip(query),
                clip(&resolved.profile)
            ),
        ));
    }
    if hits.len() > MAX_RESOURCES {
        return Err(too_big());
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "profile {} ({}/{}), modules: {}",
        clip(&resolved.profile),
        clip(&resolved.platform),
        clip(&resolved.architecture),
        clip(&resolved.modules.join(", "))
    );
    for r in hits {
        describe(resolved, r, &mut out)?;
    }
    Ok(out)
}
