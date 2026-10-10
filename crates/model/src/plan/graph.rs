//! Operation order and cycle detection. Edges are per operation, never per module, so one
//! module's operations never wait for another module's unrelated ones.

use std::collections::{BTreeMap, BTreeSet};

/// Orders `nodes` (id, ids it must follow) so each follows its prerequisites, ties broken by id.
/// Edges to ids outside `nodes` are ignored. On a cycle returns one cycle as a closed path
/// (`a, b, a`), chosen deterministically.
pub fn order(nodes: &[(String, Vec<String>)]) -> Result<Vec<String>, Vec<String>> {
    let ids: BTreeSet<&str> = nodes.iter().map(|(i, _)| i.as_str()).collect();
    let deps: BTreeMap<&str, BTreeSet<&str>> = nodes
        .iter()
        .map(|(i, a)| {
            (
                i.as_str(),
                a.iter()
                    .map(String::as_str)
                    .filter(|d| ids.contains(d))
                    .collect(),
            )
        })
        .collect();
    let mut left: BTreeMap<&str, BTreeSet<&str>> = deps.clone();
    let mut out = Vec::with_capacity(nodes.len());
    loop {
        let ready: Option<&str> = left.iter().find(|(_, d)| d.is_empty()).map(|(id, _)| *id);
        let Some(id) = ready else { break };
        left.remove(id);
        for d in left.values_mut() {
            d.remove(id);
        }
        out.push(id.to_string());
    }
    if left.is_empty() {
        return Ok(out);
    }
    // Every node left has a prerequisite that is also left, so following the smallest one from
    // the smallest node must revisit a node.
    let mut path: Vec<&str> = Vec::new();
    let mut cur = *left.keys().next().expect("left is not empty");
    loop {
        if let Some(at) = path.iter().position(|p| *p == cur) {
            let mut cycle: Vec<String> = path[at..].iter().map(|s| s.to_string()).collect();
            cycle.push(cur.to_string());
            return Err(cycle);
        }
        path.push(cur);
        cur = *left[cur]
            .iter()
            .next()
            .expect("a left node has a left prerequisite");
    }
}

#[cfg(test)]
mod tests {
    use super::order;

    fn n(id: &str, after: &[&str]) -> (String, Vec<String>) {
        (id.into(), after.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn orders_by_prerequisite_then_id() {
        let got = order(&[n("c", &["a"]), n("b", &[]), n("a", &[])]).unwrap();
        assert_eq!(got, ["a", "b", "c"]);
    }

    #[test]
    fn finds_a_cycle_and_names_it() {
        let got = order(&[n("x", &[]), n("a", &["b"]), n("b", &["a"]), n("c", &["b"])]);
        assert_eq!(got.unwrap_err(), ["a", "b", "a"]);
        assert_eq!(order(&[n("s", &["s"])]).unwrap_err(), ["s", "s"]);
    }

    #[test]
    fn unknown_prerequisites_are_ignored() {
        assert_eq!(order(&[n("a", &["gone"])]).unwrap(), ["a"]);
    }
}
