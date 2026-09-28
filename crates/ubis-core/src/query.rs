//! Query cascade.
//!
//! ```text
//! Planner ─► Stage A: operators generate candidates (union)
//!        ─► Stage B: s(j) = Σ_f w_f · φ_f(j),  φ_f = raw_f / max raw_f
//!        ─► Stage C: collapse overlaps, lift crowded siblings, adaptive K
//! ```
//!
//! Every operator is deterministic. New signals (term expansion, spectral
//! relation scores, co-change) plug in as further [`Operator`]s and enter the
//! default plan only after the evaluation harness shows a gain.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Result;
use serde::Serialize;

use crate::cochange::CoChange;
use crate::model::*;
use crate::store::{Store, UnitRow};
use crate::tokenize::{is_identifier_shaped, tokenize};

#[derive(Debug, Clone)]
pub struct Query {
    pub text: String,
    /// Unit the agent is currently looking at, if any.
    pub anchor: Option<String>,
    /// Restrict results to paths with this prefix.
    pub scope: Option<String>,
    pub k_max: usize,
    /// Lower bound of the adaptive cut. `None` uses [`K_MIN`]; `Some(k_max)`
    /// disables the adaptive cut.
    pub k_min: Option<usize>,
}

impl Query {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            anchor: None,
            scope: None,
            k_max: 10,
            k_min: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Via {
    pub op: &'static str,
    pub contribution: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub id: UnitId,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: UnitKind,
    pub label: String,
    pub signature: String,
    pub score: f64,
    pub via: Vec<Via>,
    /// Number of child hits folded into this one by sibling lifting.
    pub lifted: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub plan: Vec<(&'static str, f64)>,
    pub candidates: usize,
    pub hits: Vec<Hit>,
}

/// Resolved query context shared by operators.
pub struct Context {
    /// The anchor and all of its descendants.
    pub anchor_set: BTreeSet<UnitId>,
    pub anchor: Option<UnitRow>,
}

pub trait Operator {
    fn name(&self) -> &'static str;
    /// Candidate units with a non-negative raw score (higher is better).
    fn generate(&self, store: &Store, q: &Query, ctx: &Context) -> Result<Vec<(UnitId, f64)>>;
}

// ------------------------------------------------------------------ operators

/// Okapi BM25 over leaf units.
pub struct Lexical {
    pub limit: usize,
    pub k1: f64,
    pub b: f64,
}

impl Default for Lexical {
    fn default() -> Self {
        Self {
            limit: 200,
            k1: 1.2,
            b: 0.75,
        }
    }
}

impl Operator for Lexical {
    fn name(&self) -> &'static str {
        "lexical"
    }

    fn generate(&self, store: &Store, q: &Query, _ctx: &Context) -> Result<Vec<(UnitId, f64)>> {
        let mut terms = tokenize(&q.text);
        terms.sort();
        terms.dedup();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let stats = store.stats()?;
        let n = stats.leaves as f64;
        let avg = stats.avg_len.max(1.0);
        let mut scores: HashMap<UnitId, f64> = HashMap::new();
        for term in terms {
            let postings = store.postings(&term)?;
            let df = postings.len() as f64;
            if df == 0.0 {
                continue;
            }
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            for (id, tf, len, path) in postings {
                if let Some(scope) = &q.scope {
                    if !path.starts_with(scope.as_str()) {
                        continue;
                    }
                }
                let denom = tf + self.k1 * (1.0 - self.b + self.b * len / avg);
                *scores.entry(id).or_default() += idf * tf * (self.k1 + 1.0) / denom;
            }
        }
        Ok(top(scores, self.limit))
    }
}

/// Query terms that name a file path (`printer: fix …` → `crates/printer/…`).
/// Files are scored by IDF over path tokens; leaves in the top files that
/// contain a query term inherit their file's score, so path evidence ranks
/// the right file's units up without flooding in unrelated ones.
pub struct PathMatch {
    pub files: usize,
    pub title_only: bool,
}

impl Operator for PathMatch {
    fn name(&self) -> &'static str {
        "path"
    }

    fn generate(&self, store: &Store, q: &Query, _ctx: &Context) -> Result<Vec<(UnitId, f64)>> {
        let text = if self.title_only { q.text.lines().next().unwrap_or("") } else { q.text.as_str() };
        let mut terms = tokenize(text);
        terms.sort();
        terms.dedup();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let paths: Vec<(String, BTreeSet<String>)> = store
            .file_paths()?
            .into_iter()
            .filter(|p| q.scope.as_ref().is_none_or(|s| p.starts_with(s.as_str())))
            .map(|p| {
                let t = tokenize(&p).into_iter().collect();
                (p, t)
            })
            .collect();
        let n = paths.len() as f64;
        let mut file_scores: Vec<(f64, &str)> = paths
            .iter()
            .map(|(p, toks)| {
                let s: f64 = terms
                    .iter()
                    .filter(|t| toks.contains(*t))
                    .map(|t| {
                        let df = paths.iter().filter(|(_, o)| o.contains(t)).count() as f64;
                        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
                    })
                    .sum();
                (s, p.as_str())
            })
            .filter(|(s, _)| *s > 0.0)
            .collect();
        file_scores.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        file_scores.truncate(self.files);
        let chosen: BTreeMap<&str, f64> = file_scores.iter().map(|(s, p)| (*p, *s)).collect();
        let mut scores: HashMap<UnitId, f64> = HashMap::new();
        for term in &terms {
            for (id, _, _, path) in store.postings(term)? {
                if let Some(s) = chosen.get(path.as_str()) {
                    scores.insert(id, *s);
                }
            }
        }
        Ok(top(scores, 200))
    }
}

/// Exact symbol-name match against definitions. A query word naming a
/// symbol defined in `n` places gives each definition `1/n`.
pub struct Symbol {
    pub limit: usize,
}

impl Operator for Symbol {
    fn name(&self) -> &'static str {
        "symbol"
    }

    fn generate(&self, store: &Store, q: &Query, _ctx: &Context) -> Result<Vec<(UnitId, f64)>> {
        let mut words: Vec<&str> = q
            .text
            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
            .map(|w| w.trim_matches(':'))
            .filter(|w| w.chars().count() >= 3)
            .collect();
        words.sort();
        words.dedup();
        let mut scores: HashMap<UnitId, f64> = HashMap::new();
        for w in words {
            let defs: Vec<_> = store
                .symbol_definitions(w)?
                .into_iter()
                .filter(|(_, path)| q.scope.as_ref().is_none_or(|s| path.starts_with(s.as_str())))
                .collect();
            let n = defs.len() as f64;
            for (id, _) in defs {
                *scores.entry(id).or_default() += 1.0 / n;
            }
        }
        Ok(top(scores, self.limit))
    }
}

/// Units the anchor refers to (anchor → j), weighted by resolution mass.
pub struct RefsOut {
    pub limit: usize,
}

impl Operator for RefsOut {
    fn name(&self) -> &'static str {
        "refs_out"
    }
    fn generate(&self, store: &Store, _q: &Query, ctx: &Context) -> Result<Vec<(UnitId, f64)>> {
        let mut scores: HashMap<UnitId, f64> = HashMap::new();
        for a in &ctx.anchor_set {
            for e in store.edges_from(a)? {
                if !ctx.anchor_set.contains(&e.dst) {
                    // A target everyone references says little about this anchor.
                    let spec = if SPECIFICITY { specificity(store.in_degree(&e.dst)?) } else { 1.0 };
                    *scores.entry(e.dst).or_default() += e.weight * spec;
                }
            }
        }
        Ok(top(scores, self.limit))
    }
}

/// Units that refer to the anchor (j → anchor).
pub struct RefsIn {
    pub limit: usize,
}

impl Operator for RefsIn {
    fn name(&self) -> &'static str {
        "refs_in"
    }
    fn generate(&self, store: &Store, _q: &Query, ctx: &Context) -> Result<Vec<(UnitId, f64)>> {
        let mut scores: HashMap<UnitId, f64> = HashMap::new();
        for a in &ctx.anchor_set {
            for e in store.edges_to(a)? {
                if !ctx.anchor_set.contains(&e.src) {
                    // A caller that references everything says little about this anchor.
                    let spec = if SPECIFICITY { specificity(store.out_degree(&e.src)?) } else { 1.0 };
                    *scores.entry(e.src).or_default() += e.weight * spec;
                }
            }
        }
        Ok(top(scores, self.limit))
    }
}

/// Siblings of the anchor, closer in document order scores higher.
pub struct TreeNear {
    pub limit: usize,
}

impl Operator for TreeNear {
    fn name(&self) -> &'static str {
        "tree_near"
    }
    fn generate(&self, store: &Store, _q: &Query, ctx: &Context) -> Result<Vec<(UnitId, f64)>> {
        let Some(anchor) = &ctx.anchor else {
            return Ok(Vec::new());
        };
        let Some(parent) = &anchor.parent else {
            return Ok(Vec::new());
        };
        let mut scores = HashMap::new();
        for s in store.children(parent)? {
            if ctx.anchor_set.contains(&s.id) {
                continue;
            }
            let d = (s.ord - anchor.ord).unsigned_abs() as f64;
            scores.insert(s.id, 1.0 / (1.0 + d));
        }
        Ok(top(scores, self.limit))
    }
}

/// Other leaves in the anchor's file, closer in document order scores higher.
/// Much co-change is intra-file; this reaches it at unit granularity instead
/// of reading the whole file.
pub struct SameFile {
    pub limit: usize,
}

impl Operator for SameFile {
    fn name(&self) -> &'static str {
        "same_file"
    }
    fn generate(&self, store: &Store, _q: &Query, ctx: &Context) -> Result<Vec<(UnitId, f64)>> {
        let Some(anchor) = &ctx.anchor else {
            return Ok(Vec::new());
        };
        let mut scores = HashMap::new();
        for u in store.units_in_file(&anchor.path)? {
            if !u.is_leaf || ctx.anchor_set.contains(&u.id) {
                continue;
            }
            let d = (u.ord - anchor.ord).unsigned_abs() as f64;
            scores.insert(u.id, 1.0 / (1.0 + d / 4.0));
        }
        Ok(top(scores, self.limit))
    }
}

/// Degree discount on refs (REPORT.md E8): mixed results, not adopted.
pub const SPECIFICITY: bool = false;

/// IDF-like discount for a unit with `degree` edges on the other side.
fn specificity(degree: usize) -> f64 {
    1.0 / (1.0 + (degree as f64).ln_1p())
}

fn top(scores: HashMap<UnitId, f64>, limit: usize) -> Vec<(UnitId, f64)> {
    let mut v: Vec<_> = scores.into_iter().filter(|(_, s)| *s > 0.0).collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(limit);
    v
}

// -------------------------------------------------------------------- planner

pub struct Plan {
    pub ops: Vec<(Box<dyn Operator>, f64)>,
}

/// Deterministic routing from query shape to operator weights.
pub fn plan(q: &Query) -> Plan {
    let words: Vec<&str> = q.text.split_whitespace().collect();
    let identifier_query =
        !words.is_empty() && words.len() <= 3 && words.iter().any(|w| is_identifier_shaped(w));
    let mut ops: Vec<(Box<dyn Operator>, f64)> = Vec::new();
    if !words.is_empty() {
        // With an anchor, the anchor is the stronger evidence; text refines.
        // (Measured with ubis-bench: text at full weight drowned anchor signals.)
        let (lex, sym) = match (q.anchor.is_some(), identifier_query) {
            (true, _) => (0.25, 0.25),
            (false, true) => (1.0, 1.2),
            (false, false) => (1.0, 0.3),
        };
        ops.push((Box::new(Lexical::default()), lex));
        ops.push((Box::new(Symbol { limit: 50 }), sym));
        ops.push((Box::new(PathMatch { files: 5, title_only: PATH_TITLE_ONLY }), lex * PATH_WEIGHT));
    }
    if q.anchor.is_some() {
        ops.push((Box::new(RefsIn { limit: 50 }), 0.8));
        ops.push((Box::new(RefsOut { limit: 50 }), 0.8));
        ops.push((Box::new(TreeNear { limit: 30 }), 0.3));
        ops.push((Box::new(SameFile { limit: 30 }), 0.2));
        // Empty without git history (`ubis index --git`); measured in REPORT.md E1.
        ops.push((Box::new(CoChange { limit: 50 }), 0.5));
    }
    Plan { ops }
}

// ------------------------------------------------------------------- cascade

/// `path` weight relative to `lexical`. Measured on PR tasks (REPORT.md E7):
/// 0.5 balances ripgrep/requests/flask gains against fd; 1.0 hurt flask.
pub const PATH_WEIGHT: f64 = 0.5;
/// Match paths against the first line only: scopes like `printer:` live in
/// titles; PR bodies add noise (measured: title-only better on 3 of 4).
pub const PATH_TITLE_ONLY: bool = true;
/// Contributions below this are left out of `via` (they would print as 0.00).
pub const VIA_MIN: f64 = 0.005;
pub const LIFT_MIN_SIBLINGS: usize = 4;
pub const LIFT_MAX_LINES: usize = 300;
/// Floor of the adaptive cut. Measured with ubis-bench: 3 cut anchor queries
/// too early (fd anchor recall 0.322 → 0.351 at 5), and candidates are cheap.
pub const K_MIN: usize = 5;
pub const MASS_TAU: f64 = 0.5;

pub fn search(store: &Store, q: &Query) -> Result<Response> {
    search_with(store, q, plan(q))
}

pub fn search_with(store: &Store, q: &Query, plan: Plan) -> Result<Response> {
    let anchor = match &q.anchor {
        Some(a) => store.find_unit(a)?.into_iter().next(),
        None => None,
    };
    let anchor_set: BTreeSet<UnitId> = match &anchor {
        Some(a) => store.subtree(&a.id)?.into_iter().map(|u| u.id).collect(),
        None => BTreeSet::new(),
    };
    let ctx = Context { anchor_set, anchor };

    // Stage A + B.
    let mut total: BTreeMap<UnitId, (f64, Vec<Via>)> = BTreeMap::new();
    let mut plan_desc = Vec::new();
    for (op, w) in &plan.ops {
        plan_desc.push((op.name(), *w));
        let cands = op.generate(store, q, &ctx)?;
        let max = cands.iter().map(|c| c.1).fold(0.0, f64::max);
        if max <= 0.0 {
            continue;
        }
        for (id, raw) in cands {
            let c = w * raw / max;
            let e = total.entry(id).or_insert((0.0, Vec::new()));
            e.0 += c;
            // Negligible evidence still counts in the score but is not shown.
            if c >= VIA_MIN {
                e.1.push(Via {
                    op: op.name(),
                    contribution: c,
                });
            }
        }
    }
    let candidates = total.len();

    let mut ranked: Vec<(UnitRow, f64, Vec<Via>, usize)> = Vec::new();
    for (id, (score, via)) in total {
        if let Some(u) = store.unit(&id)? {
            if let Some(scope) = &q.scope {
                if !u.path.starts_with(scope.as_str()) {
                    continue;
                }
            }
            ranked.push((u, score, via, 0));
        }
    }
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.id.cmp(&b.0.id)));
    ranked.truncate(200);

    // Stage C.
    let ranked = collapse_ancestors(store, ranked)?;
    let ranked = lift_siblings(store, ranked, q.k_max)?;
    let scores: Vec<f64> = ranked.iter().map(|r| r.1).collect();
    let k = adaptive_k_range(&scores, q.k_min.unwrap_or(K_MIN), q.k_max);

    let hits = ranked
        .into_iter()
        .take(k)
        .map(|(u, score, via, lifted)| Hit {
            signature: u.signature(),
            id: u.id,
            path: u.path,
            start_line: u.start_line,
            end_line: u.end_line,
            kind: u.kind,
            label: u.label,
            score,
            via,
            lifted,
        })
        .collect();
    Ok(Response {
        plan: plan_desc,
        candidates,
        hits,
    })
}

type Ranked = Vec<(UnitRow, f64, Vec<Via>, usize)>;

/// Drop a hit when one of its descendants is also a hit: the smaller span is
/// the more useful answer, and the parent is reachable by zooming out.
fn collapse_ancestors(store: &Store, ranked: Ranked) -> Result<Ranked> {
    let ids: BTreeSet<UnitId> = ranked.iter().map(|r| r.0.id.clone()).collect();
    let mut covered = BTreeSet::new();
    for r in &ranked {
        for a in store.ancestors(&r.0.id)? {
            if ids.contains(&a.id) {
                covered.insert(a.id);
            }
        }
    }
    Ok(ranked
        .into_iter()
        .filter(|r| !covered.contains(&r.0.id))
        .collect())
}

/// When many hits share one small parent, return the parent once instead.
fn lift_siblings(store: &Store, ranked: Ranked, k_max: usize) -> Result<Ranked> {
    let window = (k_max * 2).min(ranked.len());
    let mut groups: BTreeMap<UnitId, Vec<usize>> = BTreeMap::new();
    for (i, r) in ranked.iter().take(window).enumerate() {
        if let Some(p) = &r.0.parent {
            groups.entry(p.clone()).or_default().push(i);
        }
    }
    let mut replace: BTreeMap<usize, UnitRow> = BTreeMap::new();
    let mut drop: BTreeSet<usize> = BTreeSet::new();
    for (parent, members) in groups {
        if members.len() < LIFT_MIN_SIBLINGS {
            continue;
        }
        let Some(p) = store.unit(&parent)? else { continue };
        if p.end_line + 1 - p.start_line > LIFT_MAX_LINES {
            continue;
        }
        replace.insert(members[0], p);
        drop.extend(members[1..].iter().copied());
    }
    if replace.is_empty() {
        return Ok(ranked);
    }
    let mut members_of: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (first, p) in &replace {
        let group: Vec<usize> = ranked
            .iter()
            .take(window)
            .enumerate()
            .filter(|(_, r)| r.0.parent.as_deref() == Some(p.id.as_str()))
            .map(|(i, _)| i)
            .collect();
        members_of.insert(*first, group);
    }
    let mut out = Vec::with_capacity(ranked.len());
    let snapshot: Vec<(f64, Vec<Via>)> = ranked.iter().map(|r| (r.1, r.2.clone())).collect();
    for (i, r) in ranked.into_iter().enumerate() {
        if drop.contains(&i) {
            continue;
        }
        if let Some(p) = replace.remove(&i) {
            let group = &members_of[&i];
            let mut via: BTreeMap<&'static str, f64> = BTreeMap::new();
            for &g in group {
                for v in &snapshot[g].1 {
                    let e = via.entry(v.op).or_default();
                    *e = e.max(v.contribution);
                }
            }
            let via = via
                .into_iter()
                .map(|(op, contribution)| Via { op, contribution })
                .collect();
            out.push((p, snapshot[i].0, via, group.len()));
        } else {
            out.push(r);
        }
    }
    Ok(out)
}

/// Cut at the largest score drop within `[K_MIN, k_max]`, subject to keeping
/// at least `MASS_TAU` of the score mass of the top `k_max`.
pub fn adaptive_k(scores: &[f64], k_max: usize) -> usize {
    adaptive_k_range(scores, K_MIN, k_max)
}

pub fn adaptive_k_range(scores: &[f64], k_min: usize, k_max: usize) -> usize {
    let hi = k_max.min(scores.len());
    if hi == 0 {
        return 0;
    }
    let lo = k_min.clamp(1, hi);
    let mass: f64 = scores[..hi].iter().sum();
    let mut best = hi;
    let mut best_gap = f64::NEG_INFINITY;
    let mut cum: f64 = scores[..lo - 1].iter().sum();
    for k in lo..=hi {
        cum += scores[k - 1];
        if mass > 0.0 && cum / mass < MASS_TAU {
            continue;
        }
        let next = scores.get(k).copied().unwrap_or(0.0);
        let gap = scores[k - 1] - next;
        // Ties go to the larger K: a flat score profile means the ranking is
        // unsure, so showing more candidates is the cheaper mistake.
        if gap >= best_gap - 1e-12 {
            best_gap = gap;
            best = k;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_k_cuts_at_largest_gap() {
        let s = [1.0, 0.95, 0.9, 0.2, 0.1, 0.05];
        assert_eq!(adaptive_k_range(&s, 3, 10), 3);
        assert_eq!(adaptive_k_range(&[1.0, 0.1, 0.09], 3, 10), 3); // floor
        assert_eq!(adaptive_k(&[1.0; 20], 10), 10); // flat: take everything allowed
        assert_eq!(adaptive_k(&[], 10), 0);
        assert_eq!(adaptive_k_range(&s, 10, 10), 6); // floor = max disables the cut
    }

    #[test]
    fn adaptive_k_respects_mass() {
        // Largest gap after the 3rd, but the first 3 hold < 50% of the mass.
        let s: [f64; 12] = [0.3, 0.3, 0.3, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05];
        let mut long = s.to_vec();
        long.extend([0.29; 4]);
        long.sort_by(|a, b| b.total_cmp(a));
        let k = adaptive_k(&long, 10);
        let mass: f64 = long[..10].iter().sum();
        assert!(long[..k].iter().sum::<f64>() / mass >= MASS_TAU);
    }
}
