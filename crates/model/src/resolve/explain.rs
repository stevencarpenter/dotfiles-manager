//! `explain`: declaration ownership, permitted changes, provenance, native precedence and
//! runtime-verification status for one resolved resource (DESIGN.md section 7). Pure text from a
//! [`Resolved`] value. Values of resources that hold secret references print as `[redacted]`.
//! Output is bounded: long strings are clipped and an oversized report is refused.

use super::compose::glob;
use super::error::visible;
use super::pointer;
use super::{ChangeKind, IncludeEntry, IncludeGroup, Resolved, ResolvedResource};
use crate::{Code, Refusal};
use std::collections::BTreeMap;
use std::fmt::Write;

const REDACTED: &str = "[redacted]";
const MAX_VALUE_CHARS: usize = 200;
const MAX_OUTPUT_BYTES: usize = 1 << 20;
const MAX_RESOURCES: usize = 100;
const MAX_OVERLAP_LINES: usize = 20;
const MAX_SUPPLIER_LINES: usize = 200;

/// Writes one line and refuses as soon as the report is over the size limit, so no list can
/// grow past it between checks.
macro_rules! put {
    ($out:expr, $($arg:tt)*) => {{
        let _ = writeln!($out, $($arg)*);
        if $out.len() > MAX_OUTPUT_BYTES {
            return Err(too_big());
        }
    }};
}

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

fn too_big() -> Refusal {
    Refusal::new(
        Code::LimitExceeded,
        "the explanation is too large; ask for one resource",
    )
}

fn native_header(
    r: &ResolvedResource,
    groups: &[IncludeGroup],
    out: &mut String,
) -> Result<(), Refusal> {
    let rule = r
        .precedence
        .as_deref()
        .unwrap_or("the application decides which value wins");
    put!(
        out,
        "  native precedence: this adapter writes into a native document. Rule: {}. The manager \
         does not validate the resulting native configuration, so it cannot enforce a protected \
         setting there. Keys that the authored document sets itself are not modelled.",
        clip(rule)
    );
    for g in groups {
        // A file can be included more than once. Each position is shown.
        let mine: Vec<&IncludeEntry> = g
            .entries
            .iter()
            .filter(|e| e.resource.as_deref() == Some(&r.id))
            .collect();
        if mine.is_empty() {
            continue;
        }
        for me in mine {
            put!(
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
        }
        put!(out, "  sibling fragments in read order:");
        for e in &g.entries {
            put!(out, "    {}", entry_text(e));
        }
    }
    Ok(())
}

/// For each value pointer of `r`, the other fragments in its include groups that also set it,
/// as `(position, resource)`. Built from the other fragments' values once, not per value.
fn overlaps<'a>(
    r: &ResolvedResource,
    resolved: &'a Resolved,
) -> BTreeMap<&'a str, Vec<(usize, &'a str)>> {
    let mut map: BTreeMap<&str, Vec<(usize, &str)>> = BTreeMap::new();
    for g in &resolved.includes {
        if !g
            .entries
            .iter()
            .any(|e| e.resource.as_deref() == Some(&r.id))
        {
            continue;
        }
        for e in &g.entries {
            let Some(other) = e.resource.as_deref().filter(|o| *o != r.id) else {
                continue;
            };
            for ptr in resolved.resources[other].provenance.keys() {
                if r.provenance.contains_key(ptr) {
                    map.entry(ptr.as_str())
                        .or_default()
                        .push((e.position, other));
                }
            }
        }
    }
    map
}

fn describe(resolved: &Resolved, r: &ResolvedResource, out: &mut String) -> Result<(), Refusal> {
    let list = |v: &[String]| {
        if v.is_empty() {
            "none".to_string()
        } else {
            v.join(", ")
        }
    };
    put!(out, "resource {}", clip(&r.id));
    put!(
        out,
        "  declared by module {} in {}",
        clip(&r.module),
        clip(&r.declared_in)
    );
    put!(
        out,
        "  kind {}, adapter {}",
        clip(&r.kind),
        clip(&r.adapter)
    );
    if let Some(t) = &r.target {
        put!(out, "  target {}", clip(t));
    }
    for c in r.claims.iter().filter(|c| Some(*c) != r.target.as_ref()) {
        put!(out, "  claims {}", clip(c));
    }
    for d in &r.dir_claims {
        put!(
            out,
            "  reads fragments {} from {}",
            clip(&d.pattern),
            clip(&d.dir)
        );
        // Any selected module may place a matching file here, and its name sets its position.
        // The owner does not opt in, so every supplier is listed.
        let dir = crate::fold::fold(&d.dir);
        let pattern = crate::fold::fold(&d.pattern);
        let mut suppliers: Vec<(&str, &ResolvedResource)> = Vec::new();
        for o in resolved.resources.values().filter(|o| o.id != r.id) {
            for claim in &o.claims {
                if let Some((parent, name)) = claim.rsplit_once('/')
                    && crate::fold::fold(parent) == dir
                    && glob(&pattern, &crate::fold::fold(name))
                {
                    suppliers.push((name, o));
                }
            }
        }
        suppliers.sort_by(|a, b| (a.0, &a.1.id).cmp(&(b.0, &b.1.id)));
        put!(
            out,
            "  fragment suppliers, in the order the assembler reads them (by file name):{}",
            if suppliers.is_empty() { " none" } else { "" }
        );
        for (i, (name, o)) in suppliers.iter().enumerate().take(MAX_SUPPLIER_LINES) {
            put!(
                out,
                "    {}. {} <- {} (module {}, declared in {})",
                i + 1,
                clip(name),
                clip(&o.id),
                clip(&o.module),
                clip(&o.declared_in)
            );
        }
        if suppliers.len() > MAX_SUPPLIER_LINES {
            put!(out, "    and {} more", suppliers.len() - MAX_SUPPLIER_LINES);
        }
    }
    put!(out, "  permitted overrides: {}", clip(&list(&r.defaults)));
    put!(
        out,
        "  permitted extensions: {}",
        clip(&list(&r.extensions))
    );
    if let Some(g) = resolved.includes.iter().find(|g| g.base == r.id) {
        put!(out, "  includes, in the order the application reads them:");
        for e in &g.entries {
            put!(out, "    {}", entry_text(e));
        }
    }
    if r.native_document {
        native_header(r, &resolved.includes, out)?;
    }
    put!(
        out,
        "  runtime verification: not verified. The resolver reads declarations only and inspects no machine state."
    );
    put!(out, "  values:");
    let shared = if r.native_document {
        overlaps(r, resolved)
    } else {
        BTreeMap::new()
    };
    for (ptr, p) in &r.provenance {
        let value = pointer::parse(ptr)
            .ok()
            .and_then(|t| pointer::get(&r.value, &t));
        let shown = value.map_or_else(|| "[unavailable]".to_string(), |v| show(r, v));
        put!(out, "    {} = {}", clip(ptr), shown);
        put!(out, "      owner {}", clip(&p.owner));
        match (p.change, &p.changer) {
            (Some(kind), Some(by)) => {
                let how = match kind {
                    ChangeKind::Override => "override",
                    ChangeKind::Contribution => "contribution",
                };
                put!(out, "      changed by {} ({how})", clip(by));
                if let Some(reason) = &p.reason {
                    put!(out, "      reason: {}", clip(reason));
                }
                if let Some(o) = &p.original {
                    put!(out, "      declared value: {}", show(r, o));
                }
            }
            _ => {
                put!(out, "      declared value, not changed");
            }
        }
        put!(out, "      supplied by {}", clip(&p.source_file));
        if let Some(path) = &p.source_path {
            put!(out, "      source file {}", clip(path));
        }
        if let Some(id) = &p.content_id {
            put!(out, "      content {id}");
        }
        if let Some(others) = shared.get(ptr.as_str()) {
            for (position, other) in others.iter().take(MAX_OVERLAP_LINES) {
                put!(
                    out,
                    "      also set by {} (include position {position}); the final value depends on the application's include order",
                    clip(other)
                );
            }
            if others.len() > MAX_OVERLAP_LINES {
                put!(
                    out,
                    "      and {} more fragments set it",
                    others.len() - MAX_OVERLAP_LINES
                );
            }
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
        let q = crate::fold::fold(query.trim_end_matches('/'));
        hits.extend(resolved.resources.values().filter(|r| {
            r.target
                .iter()
                .chain(&r.claims)
                .any(|t| crate::fold::fold(t) == q)
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
    put!(
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
