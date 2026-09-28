//! Co-change: units that changed together in the past tend to change together
//! again.
//!
//! Derived from commit evidence only. Each commit `c` touching unit set `S_c`
//! (as units that exist in the current index) adds, for every pair in `S_c`,
//!
//! ```text
//! X_ij += exp(-(T - t_c) / tau) / (|S_c| - 1)
//! ```
//!
//! so a commit spreads one unit of mass per member, decayed by age. Pairs seen
//! in fewer than `min_support` commits are dropped. The matrix is symmetric and
//! always recomputable from `commits` + `hunks`; it is stored in the derived
//! `cochange` table (like `edges`) and read by the [`CoChange`] operator.
//!
//! Unit IDs are those of the index at build time. Files edited afterwards may
//! leave rows pointing at IDs that no longer exist; the cascade drops those,
//! and the next `ubis index --git` rebuilds the table.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;

use crate::model::UnitId;
use crate::query::{Context, Operator, Query};
use crate::store::Store;

/// Defaults measured with ubis-bench (REPORT.md, E1).
pub const TAU_DAYS: f64 = 365.0;
pub const MIN_SUPPORT: usize = 2;
pub const MAX_SET: usize = 40;

#[derive(Debug, Clone)]
pub struct CoChangeParams {
    /// Reference time `T` (unix seconds), usually the newest evidence commit.
    pub now: i64,
    /// Decay time constant in seconds.
    pub tau: f64,
    pub min_support: usize,
    /// Commits touching more units than this are ignored (bulk edits).
    pub max_set: usize,
}

impl CoChangeParams {
    pub fn at(now: i64) -> Self {
        Self {
            now,
            tau: TAU_DAYS * 86400.0,
            min_support: MIN_SUPPORT,
            max_set: MAX_SET,
        }
    }
}

/// Sparse symmetric co-change matrix, rows sorted by id for determinism.
#[derive(Debug, Default)]
pub struct CoChangeIndex {
    rows: BTreeMap<UnitId, Vec<(UnitId, f64)>>,
}

impl CoChangeIndex {
    /// Build from `(commit time, touched units)` transactions.
    pub fn build(transactions: &[(i64, BTreeSet<UnitId>)], p: &CoChangeParams) -> Self {
        let mut acc: BTreeMap<(UnitId, UnitId), (f64, usize)> = BTreeMap::new();
        for (ts, set) in transactions {
            if set.len() < 2 || set.len() > p.max_set {
                continue;
            }
            let age = (p.now - ts).max(0) as f64;
            let w = (-age / p.tau).exp() / (set.len() - 1) as f64;
            let v: Vec<&UnitId> = set.iter().collect();
            for i in 0..v.len() {
                for j in (i + 1)..v.len() {
                    let e = acc.entry((v[i].clone(), v[j].clone())).or_default();
                    e.0 += w;
                    e.1 += 1;
                }
            }
        }
        let mut rows: BTreeMap<UnitId, Vec<(UnitId, f64)>> = BTreeMap::new();
        for ((a, b), (w, n)) in acc {
            if n < p.min_support || w <= 0.0 {
                continue;
            }
            rows.entry(a.clone()).or_default().push((b.clone(), w));
            rows.entry(b).or_default().push((a, w));
        }
        for r in rows.values_mut() {
            r.sort_by(|x, y| x.0.cmp(&y.0));
        }
        Self { rows }
    }

    pub fn neighbors(&self, id: &str) -> &[(UnitId, f64)] {
        self.rows.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Every stored direction `(src, dst, weight)`, sorted.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str, f64)> {
        self.rows
            .iter()
            .flat_map(|(a, r)| r.iter().map(move |(b, w)| (a.as_str(), b.as_str(), *w)))
    }

    pub fn pairs(&self) -> usize {
        self.rows.values().map(Vec::len).sum::<usize>() / 2
    }
}

/// Units that co-changed with the anchor (or its descendants).
pub struct CoChange {
    pub limit: usize,
}

impl Operator for CoChange {
    fn name(&self) -> &'static str {
        "cochange"
    }

    fn generate(&self, store: &Store, _q: &Query, ctx: &Context) -> Result<Vec<(UnitId, f64)>> {
        let mut scores: BTreeMap<UnitId, f64> = BTreeMap::new();
        for a in &ctx.anchor_set {
            for (b, w) in store.cochange_from(a)? {
                if !ctx.anchor_set.contains(&b) {
                    *scores.entry(b).or_default() += w;
                }
            }
        }
        let mut v: Vec<_> = scores.into_iter().collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(self.limit);
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ids: &[&str]) -> BTreeSet<UnitId> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn support_and_decay() {
        let p = CoChangeParams {
            now: 100,
            tau: 100.0,
            min_support: 2,
            max_set: 10,
        };
        let tx = vec![
            (100, set(&["a", "b"])),
            (0, set(&["a", "b", "c"])),
            (100, set(&["a", "c"])),
            (100, set(&["x"])),
        ];
        let idx = CoChangeIndex::build(&tx, &p);
        // a-b: 1 + e^-1/2, a-c: 1 + e^-1/2, b-c: support 1 → dropped.
        let w = 1.0 + (-1.0f64).exp() / 2.0;
        assert_eq!(idx.pairs(), 2);
        assert!((idx.neighbors("a")[0].1 - w).abs() < 1e-12);
        assert!(idx.neighbors("b").iter().all(|(n, _)| n != "c"));
        assert!(idx.neighbors("x").is_empty());
    }
}
