//! Derive edges from mentions ⋈ definitions.
//!
//! Resolution is deliberately simple and deterministic:
//!
//! * `link` mentions match an anchor definition by exact normalized name.
//! * Code mentions (`call`, `type`, `import`) prefer definitions in the same
//!   file; otherwise every definition with the same simple name is a
//!   candidate.
//! * Qualified code mentions (`Store::open`, from `Self::open()` or
//!   `self.open()`) match only owner-qualified method definitions, so
//!   `Vec::new()` never resolves to an unrelated local `new`.
//! * `bridge` mentions (identifiers in prose) match code symbols only, and are
//!   dropped when the name is too ambiguous to be a useful signal.
//!
//! * `method` mentions (`x.len()`, receiver type unknown) resolve like calls,
//!   but one extra share is reserved for "a method outside the project", so a
//!   lone local `len` gets 1/2, not a certain edge from every `.len()`.
//!
//! An ambiguous mention with `m` candidates contributes `1/m` to each. Mass is
//! split instead of discarded because UBIS narrows candidates for an agent; it
//! does not claim a resolved call graph.

use std::collections::BTreeMap;

use crate::model::*;

/// Bridges to more than this many symbols carry no locating signal.
pub const MAX_BRIDGE_CANDIDATES: usize = 3;

pub fn resolve(defs: &[(Definition, String)], mentions: &[(Mention, String)]) -> Vec<Edge> {
    let mut by_name: BTreeMap<&str, Vec<(&Definition, &str)>> = BTreeMap::new();
    for (d, path) in defs {
        by_name.entry(d.name.as_str()).or_default().push((d, path.as_str()));
    }
    for v in by_name.values_mut() {
        v.sort_by(|a, b| a.0.unit_id.cmp(&b.0.unit_id));
        v.dedup_by(|a, b| a.0.unit_id == b.0.unit_id);
    }

    // (src, dst, kind) -> (origin, weight, first line)
    let mut acc: BTreeMap<(String, String, MentionKind), (String, f64, usize)> = BTreeMap::new();
    for (m, path) in mentions {
        let Some(cands) = by_name.get(m.name.as_str()) else {
            continue;
        };
        let (targets, origin): (Vec<&Definition>, &str) = match m.kind {
            MentionKind::Link => (
                cands
                    .iter()
                    .filter(|(d, _)| d.kind == DefKind::Anchor)
                    .map(|(d, _)| *d)
                    .collect(),
                "explicit",
            ),
            MentionKind::Bridge => {
                let code: Vec<_> = cands
                    .iter()
                    .filter(|(d, p)| d.kind == DefKind::Symbol && *p != path)
                    .map(|(d, _)| *d)
                    .collect();
                if code.len() > MAX_BRIDGE_CANDIDATES {
                    continue;
                }
                (code, "bridge")
            }
            MentionKind::Call | MentionKind::Method | MentionKind::Type | MentionKind::Import => {
                let symbols: Vec<_> = cands
                    .iter()
                    .filter(|(d, _)| d.kind == DefKind::Symbol)
                    .collect();
                let local: Vec<_> = symbols
                    .iter()
                    .filter(|(_, p)| *p == path)
                    .map(|(d, _)| *d)
                    .collect();
                if !local.is_empty() {
                    (local, "same_file")
                } else {
                    (symbols.iter().map(|(d, _)| *d).collect(), "global")
                }
            }
        };
        let targets: Vec<&Definition> = targets
            .into_iter()
            .filter(|d| d.unit_id != m.unit_id)
            .collect();
        if targets.is_empty() {
            continue;
        }
        let external = usize::from(m.kind == MentionKind::Method);
        let w = 1.0 / (targets.len() + external) as f64;
        for d in targets {
            let entry = acc
                .entry((m.unit_id.clone(), d.unit_id.clone(), m.kind))
                .or_insert_with(|| (origin.to_string(), 0.0, m.line));
            entry.1 += w;
            entry.2 = entry.2.min(m.line);
        }
    }

    acc.into_iter()
        .map(|((src, dst, kind), (origin, weight, line))| Edge {
            src,
            dst,
            kind,
            origin,
            weight,
            line,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(id: &str, name: &str, kind: DefKind, path: &str) -> (Definition, String) {
        (
            Definition {
                unit_id: id.into(),
                name: name.into(),
                kind,
            },
            path.into(),
        )
    }
    fn men(id: &str, name: &str, kind: MentionKind, path: &str) -> (Mention, String) {
        (
            Mention {
                unit_id: id.into(),
                name: name.into(),
                kind,
                line: 1,
            },
            path.into(),
        )
    }

    #[test]
    fn ambiguous_calls_split_mass() {
        let defs = vec![
            def("a.rs::resolve", "resolve", DefKind::Symbol, "a.rs"),
            def("b.rs::resolve", "resolve", DefKind::Symbol, "b.rs"),
        ];
        let ms = vec![men("c.rs::run", "resolve", MentionKind::Call, "c.rs")];
        let e = resolve(&defs, &ms);
        assert_eq!(e.len(), 2);
        assert!(e.iter().all(|e| (e.weight - 0.5).abs() < 1e-12 && e.origin == "global"));
    }

    /// `x.iter()` with a lone local `iter`: half the mass stays reserved for
    /// methods outside the project.
    #[test]
    fn unknown_receiver_reserves_external_share() {
        let defs = vec![def("a.rs::Index::iter", "iter", DefKind::Symbol, "a.rs")];
        let ms = vec![men("b.rs::run", "iter", MentionKind::Method, "b.rs")];
        let e = resolve(&defs, &ms);
        assert_eq!(e.len(), 1);
        assert!((e[0].weight - 0.5).abs() < 1e-12);
    }

    #[test]
    fn same_file_wins() {
        let defs = vec![
            def("a.rs::helper", "helper", DefKind::Symbol, "a.rs"),
            def("b.rs::helper", "helper", DefKind::Symbol, "b.rs"),
        ];
        let ms = vec![men("a.rs::main", "helper", MentionKind::Call, "a.rs")];
        let e = resolve(&defs, &ms);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].dst, "a.rs::helper");
        assert_eq!(e[0].weight, 1.0);
        assert_eq!(e[0].origin, "same_file");
    }

    #[test]
    fn qualified_calls_only_match_owner() {
        let defs = vec![
            def("a.rs::Store::new", "new", DefKind::Symbol, "a.rs"),
            def("a.rs::Store::new", "Store::new", DefKind::Symbol, "a.rs"),
        ];
        let ms = vec![
            men("a.rs::f", "Vec::new", MentionKind::Call, "a.rs"),
            men("a.rs::g", "Store::new", MentionKind::Call, "a.rs"),
        ];
        let e = resolve(&defs, &ms);
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].src.as_str(), e[0].dst.as_str()), ("a.rs::g", "a.rs::Store::new"));
    }

    #[test]
    fn repeated_mentions_accumulate() {
        let defs = vec![def("a.rs::f", "f", DefKind::Symbol, "a.rs")];
        let ms = vec![
            men("a.rs::g", "f", MentionKind::Call, "a.rs"),
            men("a.rs::g", "f", MentionKind::Call, "a.rs"),
        ];
        let e = resolve(&defs, &ms);
        assert_eq!(e[0].weight, 2.0);
    }

    #[test]
    fn bridges_skip_ambiguous_and_self_file() {
        let mut defs: Vec<_> = (0..5)
            .map(|i| def(&format!("f{i}.rs::new"), "new", DefKind::Symbol, &format!("f{i}.rs")))
            .collect();
        defs.push(def("s.rs::HnswIndex", "HnswIndex", DefKind::Symbol, "s.rs"));
        let ms = vec![
            men("doc.md#a", "new", MentionKind::Bridge, "doc.md"),
            men("doc.md#a", "HnswIndex", MentionKind::Bridge, "doc.md"),
        ];
        let e = resolve(&defs, &ms);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].dst, "s.rs::HnswIndex");
    }

    #[test]
    fn links_only_hit_anchors() {
        let defs = vec![
            def("d.md#intro", "d.md#intro", DefKind::Anchor, "d.md"),
            def("x.rs::intro", "d.md#intro", DefKind::Symbol, "x.rs"),
        ];
        let ms = vec![men("e.md#top", "d.md#intro", MentionKind::Link, "e.md")];
        let e = resolve(&defs, &ms);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].dst, "d.md#intro");
        assert_eq!(e[0].origin, "explicit");
    }
}
