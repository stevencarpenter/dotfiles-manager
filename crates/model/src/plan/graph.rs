//! Operation order and cycle detection. Edges are per operation, never per module, so one
//! module's operations never wait for another module's unrelated ones.

use std::collections::{BTreeMap, BTreeSet};

/// Orders `nodes` (id, ids it must follow) so each follows its prerequisites, ties broken by id
/// (Kahn's algorithm with an ordered ready set). Edges to ids outside `nodes` are ignored. On a
/// cycle returns one cycle as a closed path (`a, b, a`), chosen deterministically.
pub fn order(nodes: &[(String, Vec<String>)]) -> Result<Vec<String>, Vec<String>> {
    let deps: BTreeMap<&str, BTreeSet<&str>> = nodes
        .iter()
        .map(|(i, a)| (i.as_str(), a.iter().map(String::as_str).collect()))
        .collect();
    let mut waiting: BTreeMap<&str, usize> = BTreeMap::new();
    let mut followers: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, ds) in &deps {
        let known: Vec<&str> = ds
            .iter()
            .copied()
            .filter(|d| deps.contains_key(d))
            .collect();
        waiting.insert(id, known.len());
        for d in known {
            followers.entry(d).or_default().push(id);
        }
    }
    let mut ready: BTreeSet<&str> = waiting
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut out = Vec::with_capacity(nodes.len());
    while let Some(id) = ready.pop_first() {
        out.push(id.to_string());
        for f in followers.get(id).into_iter().flatten() {
            if let Some(n) = waiting.get_mut(f) {
                *n -= 1;
                if *n == 0 {
                    ready.insert(f);
                }
            }
        }
    }
    if out.len() == deps.len() {
        return Ok(out);
    }
    // Every node left waits for another node that is also left, so following the smallest such
    // prerequisite from the smallest node revisits a node.
    let done: BTreeSet<&str> = out.iter().map(String::as_str).collect();
    let left = |id: &str| !done.contains(id) && deps.contains_key(id);
    let mut path: Vec<&str> = Vec::new();
    let mut cur = *deps.keys().find(|k| left(k)).unwrap_or(&"");
    loop {
        if let Some(at) = path.iter().position(|p| *p == cur) {
            let mut cycle: Vec<String> = path[at..].iter().map(|s| s.to_string()).collect();
            cycle.push(cur.to_string());
            return Err(cycle);
        }
        path.push(cur);
        match deps
            .get(cur)
            .and_then(|d| d.iter().copied().find(|d| left(d)))
        {
            Some(next) => cur = next,
            None => return Err(vec![cur.to_string()]),
        }
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

    #[test]
    fn a_large_chain_orders_without_quadratic_work() {
        let nodes: Vec<_> = (0..20_000)
            .map(|i| {
                (
                    format!("n{i:05}"),
                    if i == 0 {
                        vec![]
                    } else {
                        vec![format!("n{:05}", i - 1)]
                    },
                )
            })
            .collect();
        let got = order(&nodes).unwrap();
        assert_eq!(got.len(), 20_000);
        assert_eq!(got[19_999], "n19999");
    }
}
